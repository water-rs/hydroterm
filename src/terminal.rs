//! PTY + terminal emulation: alacritty_terminal's `Term` driven by its own
//! `EventLoop` thread, with events funneled into a channel the renderer
//! drains each frame.

use std::borrow::Cow;
use std::path::PathBuf;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{ClipboardType, Config, Term};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};
use alacritty_terminal::vte::ansi::Rgb;
use polling::{Event as PollingEvent, PollMode, Poller};

use crate::osctap::{OscScanner, TapEvent};

/// Everything the render loop needs to know that isn't cell data.
pub enum TermEvent {
    /// OSC 0/2 title change; empty = reset to default.
    Title(String),
    /// Terminal asked to write the clipboard (OSC 52); reply via the closure.
    ClipboardStore(ClipboardType, String),
    /// Terminal wants clipboard contents; call the formatter and write back.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Send + Sync>),
    /// Query a palette entry; respond with the formatted color string.
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Send + Sync>),
    /// Query the text area size; respond with formatted size.
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Send + Sync>),
    /// BEL.
    Bell,
    /// The shell child died.
    ChildExit(String),
    /// Event loop itself shut down.
    Exit,
    /// Byte-stream tap (OSC 133/7/9/777, APC) that vte drops before `Term`.
    Tap(TapEvent),
    /// kitty graphics payload with the cursor position at transmit time:
    /// `(payload, absolute line, col)` — same line convention as marks.
    Apc(Vec<u8>, i64, usize),
}

/// Grid dimensions handed to `Term` — what `Dimensions` wants.
pub struct TermSize {
    pub cols: usize,
    pub lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The `EventListener` alacritty talks back through. `Send`, lives inside the
/// parser thread.
#[derive(Clone)]
pub struct EventProxy {
    inner: Arc<ProxyInner>,
}

struct ProxyInner {
    /// Where `PtyWrite` goes once the event loop channel exists.
    notifier: OnceLock<Notifier>,
    /// UI-thread queue for everything the renderer must act on.
    events: Sender<TermEvent>,
    /// Wakes the UI thread — plugged in by the GpuView once it holds a
    /// `RedrawHandle`.
    wake: Mutex<Box<dyn Fn() + Send + Sync>>,
}

impl EventProxy {
    fn new() -> (Self, Receiver<TermEvent>) {
        let (events, rx) = mpsc::channel();
        let inner = Arc::new(ProxyInner {
            notifier: OnceLock::new(),
            events,
            wake: Mutex::new(Box::new(|| {})),
        });
        (Self { inner }, rx)
    }

    /// Install the wake callback (called by the GpuView once it holds a
    /// `RedrawHandle`).
    pub fn set_wake(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.inner.wake.lock().unwrap() = Box::new(f);
    }

    fn wake(&self) {
        (self.inner.wake.lock().unwrap())();
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Some(notifier) = self.inner.notifier.get() {
                    notifier.notify(text.into_bytes());
                }
            }
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => self.wake(),
            Event::Bell => {
                let _ = self.inner.events.send(TermEvent::Bell);
                self.wake();
            }
            Event::Title(title) => {
                let _ = self.inner.events.send(TermEvent::Title(title));
                self.wake();
            }
            Event::ResetTitle => {
                let _ = self.inner.events.send(TermEvent::Title(String::new()));
                self.wake();
            }
            Event::ClipboardStore(ty, text) => {
                let _ = self.inner.events.send(TermEvent::ClipboardStore(ty, text));
            }
            Event::ClipboardLoad(ty, fmt) => {
                let _ = self.inner.events.send(TermEvent::ClipboardLoad(ty, fmt));
            }
            Event::ColorRequest(i, fmt) => {
                let _ = self.inner.events.send(TermEvent::ColorRequest(i, fmt));
            }
            Event::TextAreaSizeRequest(fmt) => {
                let _ = self.inner.events.send(TermEvent::TextAreaSizeRequest(fmt));
            }
            Event::ChildExit(status) => {
                let _ = self
                    .inner
                    .events
                    .send(TermEvent::ChildExit(format!("{status:?}")));
                self.wake();
            }
            Event::Exit => {
                let _ = self.inner.events.send(TermEvent::Exit);
                self.wake();
            }
        }
    }
}

/// One terminal session: the shared `Term`, a sender to write PTY input, and
/// the queue of events the renderer consumes.
pub struct Terminal {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    // EventLoopSender wraps mpsc::SyncSender, which is !Sync — keep it behind
    // a Mutex so Terminal stays Send+Sync.
    io: Mutex<EventLoopSender>,
    pub proxy: EventProxy,
    /// Absolute grid rows of OSC 133 prompt-start marks
    /// (`history_size + screen line`, recorded on the reader thread).
    /// Rows drift if scrollback overflows — the oldest lines drop without a
    /// hook to rebase stored marks.
    pub prompt_marks: Arc<std::sync::Mutex<Vec<i64>>>,
    pub events: Mutex<Receiver<TermEvent>>,
    _join: std::thread::JoinHandle<(EventLoop<TapPty, EventProxy>, State)>,
}

impl Terminal {
    /// Spawn a shell on a PTY and start parsing. `shell` overrides the
    /// auto-injected shell integration (used by `shell =` config and `-e`).
    pub fn spawn(config: Config, cols: usize, lines: usize, cell_px: (u16, u16), cwd: Option<std::path::PathBuf>, shell: Option<Shell>) -> io::Result<Self> {
        let (proxy, events_rx) = EventProxy::new();
        let term = Term::new(config, &TermSize { cols, lines }, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let options = Options {
            shell: Some(shell.unwrap_or_else(shell_with_integration)),
            working_directory: cwd,
            drain_on_exit: false,
            env: [
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("COLORTERM".to_owned(), "truecolor".to_owned()),
                ("TERM_PROGRAM".to_owned(), "hydroterm".to_owned()),
            ]
            .into_iter()
            .collect(),
        };
        let pty = tty::new(
            &options,
            WindowSize {
                num_lines: lines as u16,
                num_cols: cols as u16,
                cell_width: cell_px.0,
                cell_height: cell_px.1,
            },
            0,
        )?;
        let prompt_marks: Arc<std::sync::Mutex<Vec<i64>>> = Arc::default();
        let pty = TapPty::new(
            pty,
            term.clone(),
            prompt_marks.clone(),
            proxy.inner.events.clone(),
        );

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let io = event_loop.channel();
        proxy.inner.notifier.set(Notifier(io.clone())).ok();
        let join = event_loop.spawn();

        Ok(Self { term, io: Mutex::new(io), proxy, prompt_marks, events: Mutex::new(events_rx), _join: join })
    }

    /// Write user input bytes to the PTY.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.io.lock().unwrap().send(Msg::Input(bytes.into()));
    }

    /// Tell the PTY the grid resized.
    pub fn resize(&self, cols: u16, lines: u16, cell_px: (u16, u16)) {
        {
            let mut term = self.term.lock();
            term.resize(TermSize { cols: cols as usize, lines: lines as usize });
        }
        let _ = self.io.lock().unwrap().send(Msg::Resize(WindowSize {
            num_lines: lines,
            num_cols: cols,
            cell_width: cell_px.0,
            cell_height: cell_px.1,
        }));
    }

    /// Ask the event loop to quit (kills the child).
    pub fn shutdown(&self) {
        let _ = self.io.lock().unwrap().send(Msg::Shutdown);
    }
}

/// The user's `$SHELL` plus args that inject shell integration where
/// supported — bash gets `--rcfile <generated>` emitting OSC 133 prompt
/// marks and OSC 7 cwd. Other shells spawn plain (integration TODO).
fn shell_with_integration() -> Shell {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let name = program.rsplit('/').next().unwrap_or("");
    match name {
        "bash" => match bash_integration_rc() {
            Some(rc) => Shell::new(program, vec!["--rcfile".into(), rc]),
            None => Shell::new(program, Vec::new()),
        },
        _ => Shell::new(program, Vec::new()),
    }
}

/// Write the bash integration rcfile to the cache dir; returns its path.
fn bash_integration_rc() -> Option<String> {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache"))
        .join("hydroterm");
    std::fs::create_dir_all(&base).ok()?;
    let path = base.join("shell-integration.bash");
    std::fs::write(&path, BASH_INTEGRATION).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Bash rcfile: sources the user's normal rc, then emits OSC 133 marks
/// (`A` before PS1, `B` at PS1 end, `C` pre-exec via PS0, `D` post-exec)
/// and OSC 7 cwd on every prompt.
const BASH_INTEGRATION: &str = r#"# hydroterm shell integration (auto-generated)
[ -f /etc/bash.bashrc ] && . /etc/bash.bashrc
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
__hydro_osc() {
  local s=$?
  printf '\e]133;D;%s\e\\\e]7;file://%s%s\e\\\e]133;A\e\\' "$s" "$HOSTNAME" "$PWD"
}
case ";$PROMPT_COMMAND;" in
  *__hydro_osc*) ;;
  *) PROMPT_COMMAND="__hydro_osc${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
PS0='\e]133;C\e\\'
PS1='\e]133;B\e\\'"$PS1"
"#;


// -- Byte-stream tap ---------------------------------------------------------

/// PTY wrapper whose reader additionally feeds an `OscScanner`, so sequences
/// vte's `osc_dispatch` drops (OSC 133, OSC 7, OSC 9/777, APC) still reach us.
/// The scanner also segments reads at string boundaries, so the cursor
/// position sampled when a string completes is exactly where its sender saw
/// it — `EventLoop` locks the `Term` via `try_lock_unfair` while parsing,
/// and never holds it during `read` itself.
pub struct TapPty {
    inner: tty::Pty,
    reader: TapReader,
}

/// A `*const Term` dereferenced only on the event-loop thread. The
/// `EventLoop`'s `terminal` Option keeps the `FairMutex` guard alive across
/// the whole `pty_read`, so `try_lock_unfair` inside `read` always fails on
/// the second iteration onward. `cursor.point` and `history_size` are only
/// written on that same thread (resize is the sole exception — a torn read
/// just yields a slightly stale mark), so a raw read on it is race-free.
struct TermPtr(*const Term<EventProxy>);

// SAFETY: the pointer is only dereferenced inside `TapReader::read`, which
// runs exclusively on the event-loop thread.
unsafe impl Send for TermPtr {}

/// Reads the PTY and scans for the sequences the VT layer ignores.
pub struct TapReader {
    file: std::fs::File,
    scanner: OscScanner,
    scratch: Vec<u8>,
    term: TermPtr,
    /// Keeps the `Term` pointed to by `term` alive.
    _term: Arc<FairMutex<Term<EventProxy>>>,
    marks: Arc<std::sync::Mutex<Vec<i64>>>,
    sink: Sender<TermEvent>,
}

impl TapPty {
    fn new(
        pty: tty::Pty,
        term: Arc<FairMutex<Term<EventProxy>>>,
        marks: Arc<std::sync::Mutex<Vec<i64>>>,
        sink: Sender<TermEvent>,
    ) -> Self {
        let term_ptr = {
            let guard = term.lock();
            TermPtr(&*guard as *const Term<EventProxy>)
        };
        let reader = TapReader {
            // `try_clone` yields a second fd onto the same open file
            // description: the reader consumes the identical byte stream the
            // event loop's `register()` polls on the inner file.
            file: pty.file().try_clone().expect("dup pty fd"),
            scanner: OscScanner::new(),
            scratch: vec![0; 65536],
            term: term_ptr,
            _term: term,
            marks,
            sink,
        };
        Self { inner: pty, reader }
    }
}

impl TapReader {
    /// Route a completed tap event: prompt marks snapshot the cursor
    /// position (this runs on the event-loop thread, with the `Term` only
    /// try-lockable — `lock` would deadlock against the reader's lease).
    fn dispatch(&mut self, ev: TapEvent) {
        if matches!(ev, TapEvent::PromptStart) {
            // SAFETY: dereferenced on the event-loop thread only (see
            // `TermPtr`); the `Arc<FairMutex<Term>>` outlives the reader.
            let term = unsafe { &*self.term.0 };
            let abs = term.grid().history_size() as i64
                + i64::from(term.grid().cursor.point.line.0);
            let mut marks = self.marks.lock().unwrap();
            if marks.last() != Some(&abs) {
                marks.push(abs);
            }
        }
        if let TapEvent::Apc(payload) = ev {
            // SAFETY: same TermPtr read as marks — the cursor still sits at
            // the placement position on the reader thread.
            let term = unsafe { &*self.term.0 };
            let abs = term.grid().history_size() as i64
                + i64::from(term.grid().cursor.point.line.0);
            let col = term.grid().cursor.point.column.0;
            let _ = self.sink.send(TermEvent::Apc(payload, abs, col));
            return;
        }
        let _ = self.sink.send(TermEvent::Tap(ev));
    }
}

impl Read for TapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let (n, events) = self.scanner.take(buf);
            for ev in events {
                self.dispatch(ev);
            }
            if n > 0 {
                return Ok(n);
            }
            let got = self.file.read(&mut self.scratch)?;
            if got == 0 {
                return Ok(0);
            }
            self.scanner.feed(&self.scratch[..got]);
        }
    }
}

impl EventedReadWrite for TapPty {
    type Reader = TapReader;
    type Writer = std::fs::File;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: PollingEvent,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        unsafe { self.inner.register(poll, interest, poll_opts) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, interest: PollingEvent, poll_opts: PollMode) -> io::Result<()> {
        self.inner.reregister(poll, interest, poll_opts)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.inner.writer()
    }
}

impl EventedPty for TapPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for TapPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.inner.on_resize(window_size);
    }
}

//! PTY + terminal emulation: alacritty_terminal's `Term` driven by its own
//! `EventLoop` thread, with events funneled into a channel the renderer
//! drains each frame.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{ClipboardType, Config, Term};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};

/// Absolute-row span of a command's output: `(C-mark row, Option<D-mark
/// row>)` — `None` end while the command is still running.
type OutputSpan = Option<(i64, Option<i64>)>;
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
    /// Wakes the UI thread — plugged in by the scene content once it has
    /// its channel installed.
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

    /// Install the wake callback (called by `TermSurface::set_invalidator` —
    /// a cross-thread ping into the main thread's local-executor queue).
    pub fn set_wake(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.inner.wake.lock().unwrap() = Box::new(f);
    }

    /// Ask the surface to build a frame (e.g. a queued palette action that
    /// arrives without a PTY event to trigger one).
    pub fn request_frame(&self) {
        self.wake();
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
    /// Absolute rows of the last command's output: `Some((start, end))`
    /// where `start` is the OSC 133 `C` (CommandStart) row and `end` is
    /// the `D` (CommandEnd) row — `None` end while the command is still
    /// running. Recorded on the reader thread like `prompt_marks`.
    pub last_output: Arc<Mutex<OutputSpan>>,
    pub events: Mutex<Receiver<TermEvent>>,
    /// Duplicated master fd — `tcgetpgrp` answers the slave's foreground
    /// pgroup without taking the reader's term lock.
    pty_file: std::fs::File,
    /// PID of the spawned child == its process group (the shell is the
    /// foreground job when nothing else runs).
    shell_pid: i32,
    _join: std::thread::JoinHandle<(EventLoop<TapPty, EventProxy>, State)>,
}

/// Process-side inputs to [`Terminal::spawn`] — everything the child
/// inherits that is not part of the grid.
pub struct SpawnOpts<'a> {
    /// Working directory (`None` = inherit the process cwd).
    pub cwd: Option<std::path::PathBuf>,
    /// Shell override — bypasses shell-integration injection
    /// (`shell =` config and `-e`).
    pub shell: Option<Shell>,
    /// `$TERM` value (`term` config).
    pub term_name: &'a str,
    /// `env = NAME=VALUE` config lines, applied last so a user entry can
    /// override even defaults and integration vars.
    pub env_extra: &'a [(String, String)],
}

impl Terminal {
    /// Spawn a shell on a PTY and start parsing. `shell` overrides the
    /// auto-injected shell integration (used by `shell =` config and `-e`).
    pub fn spawn(
        config: Config,
        cols: usize,
        lines: usize,
        cell_px: (u16, u16),
        opts: SpawnOpts<'_>,
    ) -> io::Result<Self> {
        let (proxy, events_rx) = EventProxy::new();
        let term = Term::new(config, &TermSize { cols, lines }, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let (shell, extra_env) = match opts.shell {
            Some(s) => (s, HashMap::new()),
            None => shell_with_integration(),
        };
        let mut env: HashMap<String, String> = [
            ("TERM".to_owned(), opts.term_name.to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "hydroterm".to_owned()),
        ]
        .into_iter()
        .collect();
        env.extend(extra_env);
        // `env = NAME=VALUE` config lines — applied last so a user entry
        // can override even the defaults and integration vars above.
        for (k, v) in opts.env_extra {
            env.insert(k.clone(), v.clone());
        }
        let options = Options {
            shell: Some(shell),
            working_directory: opts.cwd,
            // Drain the PTY's last bytes on child exit so the final
            // output isn't lost — `wait-after-command` (and any exit)
            // shows the complete last frame.
            drain_on_exit: true,
            env,
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
        // Grab a duplicated master fd + the child's pid before the Pty
        // moves into the tap wrapper — `confirm-close` later asks
        // `tcgetpgrp(master) != shell pgroup`.
        let pty_file = pty.file().try_clone()?;
        let shell_pid = pty.child().id() as i32;
        let prompt_marks: Arc<std::sync::Mutex<Vec<i64>>> = Arc::default();
        let last_output: Arc<Mutex<OutputSpan>> = Arc::default();
        let pty = TapPty::new(
            pty,
            term.clone(),
            prompt_marks.clone(),
            last_output.clone(),
            proxy.inner.events.clone(),
        );

        // `drain_on_exit` is an EventLoop parameter, not an Options one:
        // drain the PTY's last bytes on child exit so the final output
        // isn't lost — `wait-after-command` (and any exit) shows the
        // complete last frame.
        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, true, false)?;
        let io = event_loop.channel();
        proxy.inner.notifier.set(Notifier(io.clone())).ok();
        let join = event_loop.spawn();

        Ok(Self { term, io: Mutex::new(io), proxy, prompt_marks, last_output, events: Mutex::new(events_rx), pty_file, shell_pid, _join: join })
    }

    /// The program holding the PTY's foreground process group, or `None`
    /// when the shell itself is foreground (i.e. sitting at the prompt).
    /// `confirm-close` gates on this: a running program wants an OK first.
    pub fn foreground_program(&self) -> Option<String> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: tcgetpgrp on a live pty master fd is a plain query.
        let pgid = unsafe { libc::tcgetpgrp(self.pty_file.as_raw_fd()) };
        if pgid <= 0 || pgid == self.shell_pid {
            return None;
        }
        std::fs::read_to_string(format!("/proc/{pgid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(format!("process {pgid}")))
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

/// The user's `$SHELL` plus args/env that inject shell integration where
/// supported — bash gets `--rcfile <generated>`, zsh a `ZDOTDIR` with a
/// chain-sourcing `.zshrc`, fish a `-C` init command. All emit OSC 133
/// prompt marks and OSC 7 cwd.
fn shell_with_integration() -> (Shell, HashMap<String, String>) {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let name = program.rsplit('/').next().unwrap_or("");
    match name {
        "bash" => match bash_integration_rc() {
            Some(rc) => (Shell::new(program, vec!["--rcfile".into(), rc]), HashMap::new()),
            None => (Shell::new(program, Vec::new()), HashMap::new()),
        },
        "zsh" => match zsh_integration_dir() {
            Some(dir) => {
                let env = HashMap::from([("ZDOTDIR".to_owned(), dir)]);
                (Shell::new(program, Vec::new()), env)
            }
            None => (Shell::new(program, Vec::new()), HashMap::new()),
        },
        "fish" => (Shell::new(program, vec!["-C".into(), FISH_INTEGRATION.into()]), HashMap::new()),
        _ => (Shell::new(program, Vec::new()), HashMap::new()),
    }
}

/// Cache dir for generated integration files.
fn integration_base() -> Option<PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache"))
        .join("hydroterm");
    std::fs::create_dir_all(&base).ok()?;
    Some(base)
}

/// Write the bash integration rcfile to the cache dir; returns its path.
fn bash_integration_rc() -> Option<String> {
    let path = integration_base()?.join("shell-integration.bash");
    std::fs::write(&path, BASH_INTEGRATION).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Write the zsh `ZDOTDIR` (a dir containing `.zshrc` that chain-sources
/// the user's real rc); returns the dir path.
fn zsh_integration_dir() -> Option<String> {
    let dir = integration_base()?.join("zsh");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join(".zshrc"), ZSH_INTEGRATION).ok()?;
    Some(dir.to_string_lossy().into_owned())
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
PS0='\[\e]133;C\e\\\]'
PS1='\[\e]133;B\e\\\]'"$PS1"
"#;

/// Zsh `ZDOTDIR/.zshrc`: sources the user's real zshrc, then hooks
/// `precmd`/`preexec` for OSC 133 marks + OSC 7 cwd. `B` is injected at
/// the head of PS1; zsh's $HOST is the short hostname.
const ZSH_INTEGRATION: &str = r#"# hydroterm shell integration (auto-generated)
[ -f /etc/zsh/zshrc ] && . /etc/zsh/zshrc
[ -f "$HOME/.zshrc" ] && . "$HOME/.zshrc"
__hydro_precmd() {
  local s=$?
  printf '\e]133;D;%s\e\\\e]7;file://%s%s\e\\\e]133;A\e\\' "$s" "$HOST" "$PWD"
}
__hydro_preexec() { printf '\e]133;C\e\\' }
precmd_functions+=(__hydro_precmd)
preexec_functions+=(__hydro_preexec)
PS1=$'%{\e]133;B\e\\%}'$PS1
"#;

/// Fish `-C` init command: `fish_postexec`/`fish_preexec` events carry
/// the marks; `fish_prompt` is wrapped — its sequential stdout is the
/// prompt, so `A`, the original prompt, and `B` print in order.
const FISH_INTEGRATION: &str = "function __hydro_postexec --on-event fish_postexec; printf '\\e]133;D;%s\\e\\\\\\e]7;file://%s%s\\e\\\\' $status (hostname) $PWD; end; \
function __hydro_preexec --on-event fish_preexec; printf '\\e]133;C\\e\\\\'; end; \
functions -c fish_prompt __hydro_orig_fish_prompt 2>/dev/null; or function __hydro_orig_fish_prompt; echo -n '> '; end; \
function fish_prompt; printf '\\e]133;A\\e\\\\'; __hydro_orig_fish_prompt; printf '\\e]133;B\\e\\\\'; end";


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
    output: Arc<Mutex<OutputSpan>>,
    sink: Sender<TermEvent>,
}

impl TapPty {
    fn new(
        pty: tty::Pty,
        term: Arc<FairMutex<Term<EventProxy>>>,
        marks: Arc<std::sync::Mutex<Vec<i64>>>,
        output: Arc<Mutex<OutputSpan>>,
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
            output,
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
        if matches!(
            ev,
            TapEvent::PromptStart | TapEvent::CommandStart | TapEvent::CommandEnd(_)
        ) {
            // SAFETY: dereferenced on the event-loop thread only (see
            // `TermPtr`); the `Arc<FairMutex<Term>>` outlives the reader.
            let term = unsafe { &*self.term.0 };
            let abs = term.grid().history_size() as i64
                + i64::from(term.grid().cursor.point.line.0);
            match ev {
                TapEvent::PromptStart => {
                    let mut marks = self.marks.lock().unwrap();
                    if marks.last() != Some(&abs) {
                        marks.push(abs);
                    }
                }
                TapEvent::CommandStart => {
                    *self.output.lock().unwrap() = Some((abs, None));
                }
                TapEvent::CommandEnd(_) => {
                    let mut out = self.output.lock().unwrap();
                    // A `D` with no pending `C` keeps the stale span.
                    if let Some((start, None)) = *out {
                        *out = Some((start, Some(abs)));
                    }
                }
                _ => unreachable!(),
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
            let t = std::time::Instant::now();
            let (n, events) = self.scanner.take(buf);
            stat_add(&STAT_TAKE_NS, t.elapsed().as_nanos() as u64);
            for ev in events {
                self.dispatch(ev);
            }
            if n > 0 {
                return Ok(n);
            }
            let t = std::time::Instant::now();
            let got = self.file.read(&mut self.scratch)?;
            stat_add(&STAT_READ_NS, t.elapsed().as_nanos() as u64);
            if got == 0 {
                return Ok(0);
            }
            stat_add(&STAT_BYTES, got as u64);
            let t = std::time::Instant::now();
            self.scanner.feed(&self.scratch[..got]);
            stat_add(&STAT_FEED_NS, t.elapsed().as_nanos() as u64);
            report_reader_stats();
        }
    }
}

static STAT_FEED_NS: AtomicU64 = AtomicU64::new(0);
static STAT_TAKE_NS: AtomicU64 = AtomicU64::new(0);
static STAT_READ_NS: AtomicU64 = AtomicU64::new(0);
static STAT_BYTES: AtomicU64 = AtomicU64::new(0);
static STAT_LAST: AtomicU64 = AtomicU64::new(0);

fn stat_add(slot: &AtomicU64, v: u64) {
    slot.fetch_add(v, Ordering::Relaxed);
}

/// With `HYDROTERM_INPUT_STATS`, dump the reader-stage breakdown once per
/// second while bytes flow — the split between scanning and the syscall.
fn report_reader_stats() {
    if !crate::surface::input_stats() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = STAT_LAST.load(Ordering::Relaxed);
    if prev != 0 && now - prev < 1000 {
        return;
    }
    if STAT_LAST
        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    if prev == 0 {
        return;
    }
    let (feed, take, rd, by) = (
        STAT_FEED_NS.swap(0, Ordering::Relaxed),
        STAT_TAKE_NS.swap(0, Ordering::Relaxed),
        STAT_READ_NS.swap(0, Ordering::Relaxed),
        STAT_BYTES.swap(0, Ordering::Relaxed),
    );
    eprintln!(
        "rstats bytes={by} feed_ms={:.1} take_ms={:.1} read_ms={:.1}",
        feed as f64 / 1e6,
        take as f64 / 1e6,
        rd as f64 / 1e6
    );
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

/// Merge ZWJ-joined scalars into single cells.
///
/// `alacritty_terminal` stores each scalar of a ZWJ sequence in its own
/// cell: U+200D lands on the previous cell's zerowidth list, but the next
/// base scalar opens a new (usually wide) cell pair, so a family emoji such
/// as 👨‍👩‍👧 occupies six cells instead of the two its grapheme needs —
/// and every terminal reporting cursor or cell geometry disagrees with the
/// app that wrote it. Rejoin the sequence here: while a cell's zerowidth
/// chain still ends in U+200D, fold the following scalar cell into it, then
/// shift the row's remaining cells left so the cluster occupies exactly the
/// width of its head scalar. Runs over the display rows only, each pump —
/// a pathological program paying for all 24x(N) scans is still microseconds.
pub fn fixup_graphemes<T: EventListener>(term: &mut Term<T>) {
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::cell::Flags;

    let grid = term.grid_mut();
    let lines = grid.screen_lines();
    let columns = grid.columns();
    let cursor_line = grid.cursor.point.line;

    for l in 0..lines {
        let line = Line(l as i32);
        let row_len = grid[line].len();
        if row_len != columns {
            continue;
        }

        // Fast check: does any cell in this row end a zerowidth chain on
        // U+200D? Scanning zerowidth is cheaper than reconstructing rows.
        let has_zwj = (0..columns).any(|c| {
            grid[line][alacritty_terminal::index::Column(c)]
                .zerowidth()
                .is_some_and(|zw| zw.last() == Some(&'\u{200D}'))
        });
        if !has_zwj {
            continue;
        }

        // Compact the row: copy cells left to right into `out`; a cell whose
        // zerowidth chain ends in U+200D absorbs the following scalar cells
        // (each donating its base char and its own zerowidth list) until the
        // chain no longer asks for a continuation. Pair cells (wide-char
        // spacers) travel with their head cell; consumed donors contribute
        // nothing, and the row is padded out with cursor-template blanks.
        let mut out: Vec<alacritty_terminal::term::cell::Cell> = Vec::with_capacity(columns);
        // orig real-cell index of each consumed donor cell, for cursor fixup.
        let mut consumed: Vec<(usize, usize)> = Vec::new(); // (orig_col, cell_width)
        let mut col = 0usize;
        while col < columns {
            let cell = grid[line][alacritty_terminal::index::Column(col)].clone();
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            let head_i = out.len();
            out.push(cell);

            // Copy the spacer that completes a wide pair.
            if wide && col + 1 < columns {
                out.push(
                    grid[line][alacritty_terminal::index::Column(col + 1)].clone(),
                );
            }
            let mut next = col + if wide { 2 } else { 1 };

            // While the head's chain ends in U+200D, absorb the next scalar.
            loop {
                let ends_zwj = out[head_i]
                    .zerowidth()
                    .is_some_and(|zw| zw.last() == Some(&'\u{200D}'));
                if !ends_zwj || next >= columns {
                    break;
                }
                let donor_col = next;
                let donor = grid[line][alacritty_terminal::index::Column(donor_col)].clone();
                let donor_wide = donor.flags.contains(Flags::WIDE_CHAR);
                // Do not absorb a bare spacer or an untouched blank tail.
                if donor.c == ' '
                    && donor
                        .zerowidth()
                        .is_none_or(|zw| zw.is_empty())
                {
                    break;
                }
                let head = &mut out[head_i];
                head.push_zerowidth(donor.c);
                if let Some(zw) = donor.zerowidth() {
                    for c in zw {
                        head.push_zerowidth(*c);
                    }
                }
                consumed.push((donor_col, if donor_wide { 2 } else { 1 }));
                next = donor_col + if donor_wide { 2 } else { 1 };
            }
            col = next;
        }

        if consumed.is_empty() {
            continue;
        }

        // Pad the compacted row with blanks matching the cursor template.
        while out.len() < columns {
            out.push(grid.cursor.template.clone());
        }

        // Rewrite the row in place.
        for (c, cell) in out.into_iter().enumerate() {
            grid[line][alacritty_terminal::index::Column(c)] = cell;
        }

        // Re-anchor the cursor: the written prefix shrank by the cells the
        // merges consumed before the cursor's original column.
        if cursor_line == line {
            let old_col = grid.cursor.point.column.0;
            let shrink: usize = consumed
                .iter()
                .filter(|(c, _)| *c < old_col)
                .map(|(_, w)| *w)
                .sum();
            let new_col = old_col.saturating_sub(shrink);
            grid.cursor.point.column = alacritty_terminal::index::Column(new_col);
            grid.cursor.input_needs_wrap =
                grid.cursor.input_needs_wrap && new_col + 1 >= columns;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prompt-mark escapes injected into PS1/PS0 must be wrapped in the
    /// shell's non-printing markers (`\[ \]` for bash, `%{ %}` for zsh) or
    /// readline counts the OSC bytes toward prompt width and corrupts
    /// multi-line redisplay (overwrites, stray cursor offsets).
    #[test]
    fn bash_prompt_marks_are_zero_width() {
        assert!(BASH_INTEGRATION.contains(r#"PS1='\[\e]133;B\e\\\]'"#));
        assert!(BASH_INTEGRATION.contains(r#"PS0='\[\e]133;C\e\\\]'"#));
    }

    #[test]
    fn zsh_prompt_marks_are_zero_width() {
        assert!(ZSH_INTEGRATION.contains("PS1=$'%{\\e]133;B\\e\\\\%}'"));
    }

    // -- grapheme fixup -----------------------------------------------------

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone, Copy)]
    struct Sz(usize, usize);
    impl Dimensions for Sz {
        fn total_lines(&self) -> usize {
            self.0
        }
        fn screen_lines(&self) -> usize {
            self.0
        }
        fn columns(&self) -> usize {
            self.1
        }
    }

    fn feed(term: &mut Term<VoidListener>, bytes: &str) {
        let mut p: Processor = Processor::new();
        p.advance(term, bytes.as_bytes());
    }

    /// Split bench for the reader-thread pipeline (r16): the same 50 MB
    /// payload through (a) alacritty `Processor::advance` alone, (b) the
    /// `OscScanner` feed/take alone, (c) the combined pipeline — at the
    /// real grid size and 64 KiB chunks, no rendering anywhere. Run with
    /// `--release --nocapture`; debug numbers are reported separately.
    #[test]
    fn bench_parse_pipeline() {
        use crate::osctap::OscScanner;
        use std::time::Instant;

        const BYTES: usize = 50 * 1024 * 1024;
        let line = b"log payload xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";
        let mut data = Vec::with_capacity(BYTES + line.len());
        while data.len() < BYTES {
            data.extend_from_slice(line);
        }
        data.truncate(BYTES);
        let mb = || BYTES as f64 / 1e6;

        // (a) advance alone.
        let mut term = Term::new(Config::default(), &Sz(26, 69), VoidListener);
        let mut p: Processor = Processor::new();
        let t = Instant::now();
        for chunk in data.chunks(65536) {
            p.advance(&mut term, chunk);
        }
        let dt = t.elapsed().as_secs_f64();
        eprintln!("[bench] advance-only  {dt:6.2}s = {:7.1} MB/s", mb() / dt);

        // (b) scanner feed+take alone.
        let mut s = OscScanner::new();
        let mut out = vec![0u8; 65536];
        let t = Instant::now();
        for chunk in data.chunks(65536) {
            s.feed(chunk);
            loop {
                let (n, _) = s.take(&mut out);
                if n == 0 {
                    break;
                }
            }
        }
        let dt = t.elapsed().as_secs_f64();
        eprintln!("[bench] scanner-only  {dt:6.2}s = {:7.1} MB/s", mb() / dt);

        // (c) the combined reader pipeline: scan -> take -> advance.
        let mut term = Term::new(Config::default(), &Sz(26, 69), VoidListener);
        let mut p: Processor = Processor::new();
        let mut s = OscScanner::new();
        let t = Instant::now();
        for chunk in data.chunks(65536) {
            s.feed(chunk);
            loop {
                let (n, _) = s.take(&mut out);
                if n == 0 {
                    break;
                }
                p.advance(&mut term, &out[..n]);
            }
        }
        let dt = t.elapsed().as_secs_f64();
        eprintln!("[bench] pipeline      {dt:6.2}s = {:7.1} MB/s", mb() / dt);
    }

    /// Row text reconstructed as the renderer sees it: each cell's base char
    /// followed by its zerowidth list, blanks as '.'.
    fn row_text<L: EventListener>(term: &Term<L>, line: i32) -> String {
        use alacritty_terminal::index::{Column, Line};
        let mut out = String::new();
        for c in 0..term.columns() {
            let cell = &term.grid()[Line(line)][Column(c)];
            out.push(if cell.c == ' ' { '.' } else { cell.c });
            if let Some(zw) = cell.zerowidth() {
                for c in zw {
                    out.push(*c);
                }
            }
        }
        out
    }

    /// A ZWJ sequence occupies exactly its head scalar's cells: the whole
    /// cluster lands in one cell's zerowidth list, following text stays
    /// adjacent, and the cursor anchors to the cluster's logical end.
    #[test]
    fn zwj_cluster_occupies_two_cells() {
        let mut term = Term::new(Config::default(), &Sz(24, 80), VoidListener);
        feed(&mut term, "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}ok");
        // Before fixup alacritty spreads the scalars over six cells.
        fixup_graphemes(&mut term);
        let text = row_text(&term, 0);
        assert!(
            text.starts_with("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.ok"),
            "cluster not merged into one cell: {text:?}"
        );
        assert_eq!(term.grid().cursor.point.column.0, 4, "cursor not anchored");
    }

    /// The same join works mid-row and for longer families.
    #[test]
    fn zwj_cluster_mid_row() {
        let mut term = Term::new(Config::default(), &Sz(24, 80), VoidListener);
        feed(
            &mut term,
            "x\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}y",
        );
        fixup_graphemes(&mut term);
        let text = row_text(&term, 0);
        assert!(
            text.starts_with("x\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}.y"),
            "{text:?}"
        );
        assert_eq!(term.grid().cursor.point.column.0, 4);
    }

    /// `wait-after-command` plumbing: an instant-exit child's last
    /// output must reach the grid (drain_on_exit) and ChildExit/Exit
    /// must be queued — the path that used to leave a zombie window.
    #[test]
    fn instant_exit_drains_output_and_queues_exit() {
        let terminal = Terminal::spawn(
            Config::default(),
            80,
            24,
            (9, 18),
            SpawnOpts {
                cwd: None,
                shell: Some(Shell::new(
                    "/bin/echo".to_owned(),
                    vec!["WAITMARK_DRAIN".to_owned()],
                )),
                term_name: "xterm-256color",
                env_extra: &[],
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(700));
        let text = {
            let t = terminal.term.lock();
            (0..24)
                .map(|i| row_text(&t, i))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(text.contains("WAITMARK_DRAIN"), "drained output missing:\n{text}");
        let evs: Vec<TermEvent> = terminal.events.lock().unwrap().try_iter().collect();
        assert!(
            evs.iter()
                .any(|e| matches!(e, TermEvent::ChildExit(_) | TermEvent::Exit)),
            "exit events missing"
        );
    }

    /// Plain wide emoji without ZWJ are left untouched.
    #[test]
    fn plain_emoji_untouched() {
        let mut term = Term::new(Config::default(), &Sz(24, 80), VoidListener);
        feed(&mut term, "\u{1F600}\u{1F601}");
        fixup_graphemes(&mut term);
        let text = row_text(&term, 0);
        assert!(text.starts_with("\u{1F600}.\u{1F601}."), "{text:?}");
        assert_eq!(term.grid().cursor.point.column.0, 4);
    }
}

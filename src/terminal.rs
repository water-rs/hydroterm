//! PTY + terminal emulation: alacritty_terminal's `Term` driven by its own
//! `EventLoop` thread, with events funneled into a channel the renderer
//! drains each frame.

use std::borrow::Cow;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{ClipboardType, Config, Term};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options};
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
    pub events: Mutex<Receiver<TermEvent>>,
    _join: std::thread::JoinHandle<(EventLoop<TapPty, EventProxy>, State)>,
}

impl Terminal {
    /// Spawn a shell on a PTY and start parsing.
    pub fn spawn(config: Config, cols: usize, lines: usize, cell_px: (u16, u16), cwd: Option<std::path::PathBuf>) -> io::Result<Self> {
        let (proxy, events_rx) = EventProxy::new();
        let term = Term::new(config, &TermSize { cols, lines }, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let options = Options {
            shell: None,
            working_directory: cwd,
            drain_on_exit: false,
            env: [
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("COLORTERM".to_owned(), "truecolor".to_owned()),
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
        let pty = TapPty::new(pty, proxy.inner.events.clone());

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let io = event_loop.channel();
        proxy.inner.notifier.set(Notifier(io.clone())).ok();
        let join = event_loop.spawn();

        Ok(Self { term, io: Mutex::new(io), proxy, events: Mutex::new(events_rx), _join: join })
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

// -- Byte-stream tap ---------------------------------------------------------

/// PTY wrapper whose reader additionally feeds an `OscScanner`, so sequences
/// vte's `osc_dispatch` drops (OSC 133, OSC 7, OSC 9/777, APC) still reach us.
pub struct TapPty {
    inner: tty::Pty,
    reader: TapReader,
}

/// Reads the PTY and scans for the sequences the VT layer ignores.
pub struct TapReader {
    file: std::fs::File,
    scanner: OscScanner,
}

impl TapPty {
    fn new(pty: tty::Pty, sink: Sender<TermEvent>) -> Self {
        let reader = TapReader {
            // `try_clone` yields a second fd onto the same open file
            // description: the reader consumes the identical byte stream the
            // event loop's `register()` polls on the inner file.
            file: pty.file().try_clone().expect("dup pty fd"),
            scanner: OscScanner::new(sink),
        };
        Self { inner: pty, reader }
    }
}

impl Read for TapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if n > 0 {
            self.scanner.feed(&buf[..n]);
        }
        Ok(n)
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

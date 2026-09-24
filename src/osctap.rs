//! Byte-stream tap on the PTY output, catching sequences vte drops before
//! `Term` ever sees them: OSC 133 prompt marks, OSC 7 working directory,
//! OSC 9/777 notifications, APC kitty graphics.
//!
//! The scanner doubles as a read filter: `feed` buffers raw bytes, and
//! `take` hands them to the event loop in segments cut at string
//! boundaries — plain text before a tapped string is returned separately
//! from the string itself. Since the event loop parses each returned
//! segment immediately, the cursor position sampled while emitting a
//! string segment is exactly where the sender's prompt sat.

use std::collections::VecDeque;
use std::path::PathBuf;

/// Max buffered bytes inside one OSC/APC sequence before we drop it.
/// kitty image payloads chunk at ~4KiB each; 1MiB is far past a sane OSC.
const MAX_STRING_LEN: usize = 1 << 20;

/// How much of the prompt the shell repaints after a resize — the
/// `redraw` option on OSC 133 `A` (kitty `redraw=0|1`, plus `last` for
/// shells like bash that repaint only the prompt line under the cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellRedraw {
    /// Full repaint: the prompt region may be cleared wholesale.
    True,
    /// No repaint: nothing may be cleared.
    False,
    /// Only the cursor's line is repainted, so only that row may be
    /// cleared — blanking untouched prompt lines would erase text the
    /// shell never rewrites.
    Last,
}

/// Events extracted from the raw byte stream.
#[derive(Debug, Clone)]
pub enum TapEvent {
    /// OSC 133 ; A — prompt start.
    PromptStart,
    /// OSC 133 ; A ; … ; redraw=X — repaint mode announced at the mark.
    ShellRedraw(ShellRedraw),
    /// OSC 133 ; B — prompt end / command start.
    PromptEnd,
    /// OSC 133 ; C — command output start (pre-execution).
    CommandStart,
    /// OSC 133 ; D ; code — command finished with exit status.
    CommandEnd(Option<i32>),
    /// OSC 7 file://host/path — the shell's cwd.
    Cwd(PathBuf),
    /// OSC 9 ; text / OSC 777 ; notify ; title ; body — notification.
    Notify(String, String),
    /// APC kitty graphics payload — the bytes after `ESC _` up to ST.
    Apc(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    /// In a string payload; bool = previous byte was ESC (maybe ST next).
    Osc(bool),
    Apc(bool),
    /// DCS / SOS / PM: consume to ST, don't buffer.
    Skip(bool),
}

/// Incremental scanner fed with raw PTY bytes, emitting them in
/// string-boundary-aligned segments.
pub struct OscScanner {
    state: State,
    /// Payload of the current string sequence.
    buf: Vec<u8>,
    /// Fed bytes not yet taken.
    pending: Vec<u8>,
    /// Segment ends within `pending`: (end index, is string end).
    cuts: VecDeque<(usize, bool)>,
    /// Events produced since the last emitted string segment.
    events: Vec<TapEvent>,
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            pending: Vec::new(),
            cuts: VecDeque::new(),
            events: Vec::new(),
        }
    }

    /// Scan raw bytes, buffering them for segmented `take` calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.pending.push(b);
            let prev = self.state;
            self.step(b);
            match (prev, self.state) {
                // A tapped string opens: cut the segment just before its ESC.
                (State::Esc, State::Osc(_)) | (State::Esc, State::Apc(_)) => {
                    self.cuts.push_back((self.pending.len() - 2, false));
                }
                // A tapped string closed: cut right after its terminator.
                (State::Osc(_), State::Ground) | (State::Apc(_), State::Ground) => {
                    self.cuts.push_back((self.pending.len(), true));
                }
                _ => {}
            }
        }
    }

    /// Any staged bytes not yet emitted by `take`.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Count of staged bytes not yet emitted.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Emit the next pending segment into `out`; returns the byte count and
    /// the events to dispatch — attached to the segment that completes a
    /// tapped string, when the parser has consumed everything before it.
    pub fn take(&mut self, out: &mut [u8]) -> (usize, Vec<TapEvent>) {
        while let Some(&(cut, string_end)) = self.cuts.front() {
            let n = cut.min(out.len());
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            for c in self.cuts.iter_mut() {
                c.0 -= n;
            }
            if n == cut {
                self.cuts.pop_front();
            }
            if n > 0 {
                let events = if string_end {
                    std::mem::take(&mut self.events)
                } else {
                    Vec::new()
                };
                return (n, events);
            }
            // Zero-length cut at the segment start — pop and continue.
        }
        if !self.pending.is_empty() {
            let n = self.pending.len().min(out.len());
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            // A partially emitted in-progress string shifts the recorded
            // end cut; plain text never carries one.
            for c in self.cuts.iter_mut() {
                c.0 -= n;
            }
            return (n, Vec::new());
        }
        (0, Vec::new())
    }

    fn step(&mut self, b: u8) {
        use State::*;
        match self.state {
            Ground => {
                if b == 0x1b {
                    self.state = Esc;
                }
            }
            Esc => {
                self.state = match b {
                    b'[' => Csi,
                    b']' => self.enter(Osc(false)),
                    b'_' => self.enter(Apc(false)),
                    b'P' | b'X' | b'^' => Skip(false),
                    0x1b => Esc,
                    _ => Ground,
                };
            }
            Csi => {
                if b == 0x1b {
                    self.state = Esc;
                } else if (0x40..=0x7e).contains(&b) {
                    self.state = Ground;
                }
            }
            Osc(esc) | Apc(esc) | Skip(esc) => {
                let in_osc = matches!(self.state, Osc(_));
                let in_apc = matches!(self.state, Apc(_));
                if esc {
                    self.state = if b == b'\\' {
                        self.finish_string(in_osc, in_apc);
                        Ground
                    } else if b == 0x1b {
                        // ESC ESC inside a string: stay pending.
                        min_state(true, in_osc, in_apc)
                    } else {
                        // ESC + other byte aborts the string per ECMA-48.
                        self.buf.clear();
                        Esc
                    };
                } else if b == 0x07 && !matches!(self.state, Skip(_)) {
                    self.state = Ground;
                    self.finish_string(in_osc, in_apc);
                } else if b == 0x1b {
                    self.state = match self.state {
                        Osc(_) => Osc(true),
                        Apc(_) => Apc(true),
                        Skip(_) => Skip(true),
                        _ => unreachable!(),
                    };
                } else {
                    if !matches!(self.state, Skip(_)) && self.buf.len() < MAX_STRING_LEN {
                        self.buf.push(b);
                    }
                    self.state = match self.state {
                        Osc(_) => Osc(false),
                        Apc(_) => Apc(false),
                        Skip(_) => Skip(false),
                        _ => unreachable!(),
                    };
                }
            }
        }
    }

    fn enter(&mut self, s: State) -> State {
        self.buf.clear();
        s
    }

    fn finish_string(&mut self, osc: bool, apc: bool) {
        if osc {
            self.dispatch_osc();
        } else if apc {
            let payload = std::mem::take(&mut self.buf);
            self.events.push(TapEvent::Apc(payload));
        }
        self.buf.clear();
    }

    fn dispatch_osc(&mut self) {
        let params: Vec<&[u8]> = self.buf.split(|&b| b == b';').collect();
        let tap = |e: TapEvent, evs: &mut Vec<TapEvent>| evs.push(e);
        match params.first().copied().unwrap_or(b"") {
            b"7" => {
                if let Some(p) = params.get(1).and_then(|u| cwd_from_uri(u)) {
                    tap(TapEvent::Cwd(p), &mut self.events);
                }
            }
            b"9" => {
                if let Some(t) = params.get(1).and_then(|p| std::str::from_utf8(p).ok()) {
                    tap(TapEvent::Notify(String::new(), t.to_owned()), &mut self.events);
                }
            }
            b"133" => match params.get(1).copied().unwrap_or(b"") {
                b"A" => {
                    tap(TapEvent::PromptStart, &mut self.events);
                    if let Some(r) = params[2..].iter().find_map(|p| parse_redraw(p)) {
                        tap(TapEvent::ShellRedraw(r), &mut self.events);
                    }
                }
                b"B" => tap(TapEvent::PromptEnd, &mut self.events),
                b"C" => tap(TapEvent::CommandStart, &mut self.events),
                // `133;D;{code}` — also `133;D` alone.
                p if p.first() == Some(&b'D') => {
                    let code = params
                        .get(2)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<i32>().ok());
                    tap(TapEvent::CommandEnd(code), &mut self.events);
                }
                _ => (),
            },
            b"777" => {
                // `777;notify;title;body`
                if let (Some(t), Some(b)) = (params.get(2), params.get(3))
                    && let (Ok(t), Ok(b)) = (std::str::from_utf8(t), std::str::from_utf8(b))
                {
                    tap(TapEvent::Notify(t.to_owned(), b.to_owned()), &mut self.events);
                }
            }
            _ => (),
        }
    }
}

/// `redraw=0|1|last` → [`ShellRedraw`].
fn parse_redraw(param: &[u8]) -> Option<ShellRedraw> {
    let v = param.strip_prefix(b"redraw=")?;
    Some(match v {
        b"0" => ShellRedraw::False,
        b"1" => ShellRedraw::True,
        b"last" => ShellRedraw::Last,
        _ => return None,
    })
}

/// `file://host/path` → decoded path (localhost or matching hostname only).
fn cwd_from_uri(uri: &[u8]) -> Option<PathBuf> {
    let uri = std::str::from_utf8(uri).ok()?;
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let path = &rest[slash..];
    let mut out = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).as_ref()))
}

/// The string state to re-enter when a pending-ESC turns out to be another
/// ESC inside the same string.
fn min_state(esc: bool, in_osc: bool, in_apc: bool) -> State {
    if in_osc {
        State::Osc(esc)
    } else if in_apc {
        State::Apc(esc)
    } else {
        State::Skip(esc)
    }
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed bytes, then drain every segment collecting events and the
    /// re-emitted stream.
    fn scan(bytes: &[u8]) -> (Vec<TapEvent>, Vec<u8>) {
        let mut s = OscScanner::new();
        s.feed(bytes);
        let mut events = Vec::new();
        let mut stream = Vec::new();
        let mut out = [0u8; 64];
        loop {
            let (n, evs) = s.take(&mut out);
            events.extend(evs);
            if n == 0 {
                break;
            }
            stream.extend_from_slice(&out[..n]);
        }
        (events, stream)
    }

    #[test]
    fn passthrough_preserves_stream() {
        let input = b"text\x1b]133;A\x07more\x1b_Ga=T;XX\x1b\\tail\x1bPq\x1b]7;x\x07y\x1b\\end";
        let (_evs, stream) = scan(input);
        assert_eq!(stream, input);
    }

    #[test]
    fn segments_split_around_string() {
        let mut s = OscScanner::new();
        s.feed(b"pre\x1b]133;A\x07post");
        let mut out = [0u8; 64];
        let (n1, e1) = s.take(&mut out);
        assert_eq!(&out[..n1], b"pre");
        assert!(e1.is_empty());
        let (n2, e2) = s.take(&mut out);
        assert_eq!(&out[..n2], b"\x1b]133;A\x07");
        assert!(matches!(e2.as_slice(), [TapEvent::PromptStart]));
        let (n3, e3) = s.take(&mut out);
        assert_eq!(&out[..n3], b"post");
        assert!(e3.is_empty());
    }

    #[test]
    fn osc133_marks() {
        let (ev, _) = scan(b"\x1b]133;A\x07prompt$ \x1b]133;B\x07ls\x1b]133;D;0\x07");
        assert!(matches!(ev.as_slice(), [TapEvent::PromptStart, TapEvent::PromptEnd, TapEvent::CommandEnd(Some(0))]));
    }

    #[test]
    fn osc133_st_terminated() {
        let (ev, _) = scan(b"\x1b]133;C\x1b\\rest");
        assert!(matches!(ev.as_slice(), [TapEvent::CommandStart]));
    }

    #[test]
    fn osc133_split_across_feeds() {
        let mut s = OscScanner::new();
        s.feed(b"ls\x1b]13");
        s.feed(b"3;A\x07hi");
        s.feed(b"\x1b]133;D;42\x1b\\");
        let mut events = Vec::new();
        let mut out = [0u8; 64];
        loop {
            let (n, evs) = s.take(&mut out);
            let empty = n == 0 && evs.is_empty();
            events.extend(evs);
            if empty {
                break;
            }
        }
        assert!(matches!(events.as_slice(), [TapEvent::PromptStart, TapEvent::CommandEnd(Some(42))]));
    }

    #[test]
    fn osc133_redraw_option() {
        let (ev, _) = scan(b"\x1b]133;A;redraw=last\x07");
        assert!(matches!(
            ev.as_slice(),
            [TapEvent::PromptStart, TapEvent::ShellRedraw(ShellRedraw::Last)]
        ));
        let (ev, _) = scan(b"\x1b]133;A;redraw=0\x07");
        assert!(ev.iter().any(|e| matches!(e, TapEvent::ShellRedraw(ShellRedraw::False))));
        // Unknown values are ignored; a bare A emits no redraw event.
        let (ev, _) = scan(b"\x1b]133;A;redraw=wat\x07\x1b]133;A\x07");
        assert_eq!(ev.iter().filter(|e| matches!(e, TapEvent::ShellRedraw(_))).count(), 0);
    }

    #[test]
    fn osc7_cwd() {
        let (ev, _) = scan(b"\x1b]7;file://devin-box/home/ubuntu/projects%20x\x07");
        assert!(
            matches!(ev.as_slice(), [TapEvent::Cwd(p)] if p == &PathBuf::from("/home/ubuntu/projects x"))
        );
    }

    #[test]
    fn no_false_positive_in_dcs() {
        // An ESC] inside DCS must not start an OSC.
        let (ev, stream) = scan(b"\x1bPq\x1b]133;A\x07stuff\x1b\\after");
        assert!(ev.is_empty());
        assert_eq!(stream, b"\x1bPq\x1b]133;A\x07stuff\x1b\\after");
    }

    #[test]
    fn esc_abort() {
        // ESC inside OSC aborts it; the next sequence still parses.
        let (ev, _) = scan(b"\x1b]133;\x1bX\x1b]133;A\x07");
        assert!(matches!(ev.as_slice(), [TapEvent::PromptStart]));
    }

    #[test]
    fn apc_captured() {
        let (ev, _) = scan(b"\x1b_Ga=T,f=32;QUJD\x1b\\x");
        assert!(matches!(ev.as_slice(), [TapEvent::Apc(p)] if p == b"Ga=T,f=32;QUJD"));
    }
}

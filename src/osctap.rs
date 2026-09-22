//! Byte-stream tap on the PTY output, catching sequences vte drops before
//! `Term` ever sees them: OSC 133 prompt marks, OSC 7 working directory,
//! OSC 9/777 notifications, APC kitty graphics.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Max buffered bytes inside one OSC/APC sequence before we drop it.
/// kitty image payloads chunk at ~4KiB each; 1MiB is far past a sane OSC.
const MAX_STRING_LEN: usize = 1 << 20;

/// Events extracted from the raw byte stream.
#[derive(Debug, Clone)]
pub enum TapEvent {
    /// OSC 133 ; A — prompt start.
    PromptStart,
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

/// Where the tap sends decoded events (same channel `EventProxy` uses).
pub type TapSink = Sender<crate::terminal::TermEvent>;

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

/// Incremental scanner fed with raw PTY bytes.
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
    sink: TapSink,
}

impl OscScanner {
    pub fn new(sink: TapSink) -> Self {
        Self { state: State::Ground, buf: Vec::new(), sink }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
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
                let in_osc = self.state == Osc(true) || self.state == Osc(false);
                let in_apc = self.state == Apc(true) || self.state == Apc(false);
                if esc {
                    self.state = if b == b'\\' {
                        self.finish_string(in_osc, in_apc);
                        Ground
                    } else if b == 0x1b {
                        // ESC ESC inside a string: stay pending.
                        Osc(true).min_state(in_osc, in_apc)
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
            let _ = self.sink.send(crate::terminal::TermEvent::Tap(TapEvent::Apc(payload)));
        }
        self.buf.clear();
    }

    fn dispatch_osc(&self) {
        let params: Vec<&[u8]> = self.buf.split(|&b| b == b';').collect();
        let tap = |e: TapEvent| {
            let _ = self.sink.send(crate::terminal::TermEvent::Tap(e));
        };
        match params.first().copied().unwrap_or(b"") {
            b"7" => {
                if let Some(p) = params.get(1).and_then(|u| cwd_from_uri(u)) {
                    tap(TapEvent::Cwd(p));
                }
            }
            b"9" => {
                if let Some(t) = params.get(1).and_then(|p| std::str::from_utf8(p).ok()) {
                    tap(TapEvent::Notify(String::new(), t.to_owned()));
                }
            }
            b"133" => match params.get(1).copied().unwrap_or(b"") {
                b"A" => tap(TapEvent::PromptStart),
                b"B" => tap(TapEvent::PromptEnd),
                b"C" => tap(TapEvent::CommandStart),
                // `133;D;{code}` — also `133;D` alone.
                p if p.first() == Some(&b'D') => {
                    let code = params
                        .get(2)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<i32>().ok());
                    tap(TapEvent::CommandEnd(code));
                }
                _ => (),
            },
            b"777" => {
                // `777;notify;title;body`
                if let (Some(t), Some(b)) = (params.get(2), params.get(3))
                    && let (Ok(t), Ok(b)) = (std::str::from_utf8(t), std::str::from_utf8(b))
                {
                    tap(TapEvent::Notify(t.to_owned(), b.to_owned()));
                }
            }
            _ => (),
        }
    }
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

impl State {
    /// Keep the same string state when re-entering pending-ESC.
    fn min_state(self, osc: bool, apc: bool) -> Self {
        let _ = self;
        if osc {
            State::Osc(true)
        } else if apc {
            State::Apc(true)
        } else {
            State::Skip(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TermEvent;
    use std::sync::mpsc;

    fn scan(bytes: &[u8]) -> Vec<TapEvent> {
        let (tx, rx) = mpsc::channel();
        let mut s = OscScanner::new(tx);
        s.feed(bytes);
        drop(s);
        rx.iter()
            .filter_map(|e| match e {
                TermEvent::Tap(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn osc133_marks() {
        let ev = scan(b"\x1b]133;A\x07prompt$ \x1b]133;B\x07ls\x1b]133;D;0\x07");
        assert!(matches!(ev.as_slice(), [TapEvent::PromptStart, TapEvent::PromptEnd, TapEvent::CommandEnd(Some(0))]));
    }

    #[test]
    fn osc133_st_terminated() {
        let ev = scan(b"\x1b]133;C\x1b\\rest");
        assert!(matches!(ev.as_slice(), [TapEvent::CommandStart]));
    }

    #[test]
    fn osc133_split_across_reads() {
        let (tx, rx) = mpsc::channel();
        let mut s = OscScanner::new(tx);
        s.feed(b"ls\x1b]13");
        s.feed(b"3;A\x07hi");
        s.feed(b"\x1b]133;D;42\x1b\\");
        drop(s);
        let ev: Vec<TapEvent> = rx
            .iter()
            .filter_map(|e| match e {
                TermEvent::Tap(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(matches!(ev.as_slice(), [TapEvent::PromptStart, TapEvent::CommandEnd(Some(42))]));
    }

    #[test]
    fn osc7_cwd() {
        let ev = scan(b"\x1b]7;file://devin-box/home/ubuntu/projects%20x\x07");
        assert!(
            matches!(ev.as_slice(), [TapEvent::Cwd(p)] if p == &PathBuf::from("/home/ubuntu/projects x"))
        );
    }

    #[test]
    fn no_false_positive_in_dcs() {
        // An ESC] inside DCS must not start an OSC.
        let ev = scan(b"\x1bPq\x1b]133;A\x07stuff\x1b\\after");
        assert!(ev.is_empty());
    }

    #[test]
    fn esc_abort() {
        // ESC inside OSC aborts it; the next sequence still parses.
        let ev = scan(b"\x1b]133;\x1bX\x1b]133;A\x07");
        assert!(matches!(ev.as_slice(), [TapEvent::PromptStart]));
    }

    #[test]
    fn apc_captured() {
        let ev = scan(b"\x1b_Ga=T,f=32;QUJD\x1b\\x");
        assert!(matches!(ev.as_slice(), [TapEvent::Apc(p)] if p == b"Ga=T,f=32;QUJD"));
    }
}

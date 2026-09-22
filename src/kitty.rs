//! kitty graphics protocol subset: `ESC _ G <keys>;<base64> ST`.
//!
//! Supported: `a=T` (transmit+display) and `a=t`/`a=q`, formats `f=100`
//! (PNG), `f=32` (RGBA) and `f=24` (RGB) with `s=`/`v=` pixel dims,
//! `m=` chunk assembly, `i=`/`I=` image ids, `c=`/`r=` cell spans,
//! `a=d` delete (by `i=` or all), `q=` quiet, `t=f` regular-file
//! medium (payload is the base64 of the path). Placement anchors to the
//! cursor row at transmit time and scrolls with the buffer.
//! Not supported: unicode placements (`p=`, `u=`), `z=` layers,
//! `t=t`/`t=s`/`t=o` shared mediums, `x`/`y`/`w`/`h` crops, `a=f`/`a=p`.

use std::collections::BTreeMap;

use peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};

/// One decoded `G...;payload` command.
pub struct KittyCmd {
    pub keys: BTreeMap<char, String>,
    /// Raw base64 payload text (may be empty for queries/deletes).
    pub data: String,
}

impl KittyCmd {
    fn get(&self, k: char) -> Option<&str> {
        self.keys.get(&k).map(String::as_str)
    }
    fn num(&self, k: char) -> Option<u32> {
        self.get(k).and_then(|v| v.parse().ok())
    }
    /// Response target id (`i=` preferred over `I=`).
    pub fn id(&self) -> u32 {
        self.num('i').or_else(|| self.num('I')).unwrap_or(0)
    }
    /// `q=1`/`q=2` suppress the reply.
    pub fn quiet(&self) -> bool {
        self.num('q').is_some_and(|q| q >= 1)
    }
    /// More chunks follow this one.
    pub fn more(&self) -> bool {
        self.num('m') == Some(1)
    }
}

/// Split `G<k=v>,...,<k=v>;<base64>` into keys + payload text.
/// The APC payload keeps its leading `G` graphics-command letter.
pub fn parse(payload: &[u8]) -> Option<KittyCmd> {
    let text = std::str::from_utf8(payload).ok()?.strip_prefix('G')?;
    let (keys, data) = text.split_once(';')?;
    let mut map = BTreeMap::new();
    for kv in keys.split(',') {
        if kv.is_empty() {
            continue;
        }
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        map.insert(k.chars().next()?, v.to_string());
    }
    Some(KittyCmd {
        keys: map,
        data: data.to_string(),
    })
}

/// Minimal base64 decoder (RFC 4648, `+/` alphabet, `=` pad, no whitespace).
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a' + 26)),
            b'0'..=b'9' => Some(u32::from(b - b'0' + 52)),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in &bytes {
        if b == b'=' {
            break;
        }
        acc = (acc << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// A placed image: absolute buffer row + column at transmit time.
pub struct KittyImage {
    pub id: u32,
    pub brush: peniko::ImageBrush,
    /// Absolute line index (same convention as prompt marks).
    pub line: i64,
    pub col: usize,
    /// Display size in cells (0,0 = native pixel size).
    pub cols: u32,
    pub rows: u32,
    /// Source pixel size for the draw transform.
    pub px_w: u32,
    pub px_h: u32,
}

/// Per-session image store + in-flight chunked transmission.
#[derive(Default)]
pub struct KittyStore {
    pub images: Vec<KittyImage>,
    pending: Option<(BTreeMap<char, String>, String)>,
}

/// Outcome of handling one command: `(image id, status text)`.
pub type Handled = (u32, String);

impl KittyStore {
    /// Feed one parsed command; returns the `(id, status)` reply payload.
    pub fn handle(&mut self, cmd: KittyCmd, line: i64, col: usize) -> Handled {
        // Chunk assembly: `m=1` stashes keys + payload until `m=0`.
        if cmd.more() {
            match &mut self.pending {
                Some((_, data)) => {
                    data.push_str(&cmd.data);
                    return (cmd.id(), "OK".to_string());
                }
                None => {
                    let id = cmd.id();
                    self.pending = Some((cmd.keys.clone(), cmd.data));
                    return (id, "OK".to_string());
                }
            }
        }
        let (keys, data) = match self.pending.take() {
            Some((mut keys, mut data)) => {
                // Final chunk may carry no keys of its own.
                keys.extend(cmd.keys.clone());
                data.push_str(&cmd.data);
                (keys, data)
            }
            None => (cmd.keys.clone(), cmd.data),
        };
        let cmd = KittyCmd { keys, data };
        match cmd.get('a').unwrap_or("T") {
            "d" => {
                match cmd.num('i') {
                    Some(id) => self.images.retain(|img| img.id != id),
                    None => self.images.clear(),
                }
                (cmd.id(), "OK".to_string())
            }
            "q" => (cmd.id(), "OK".to_string()),
            "t" | "T" | "" => self.place(&cmd, line, col),
            _ => (cmd.id(), "EINVAL:unsupported action".to_string()),
        }
    }

    fn place(&mut self, cmd: &KittyCmd, line: i64, col: usize) -> Handled {
        let raw = match cmd.get('t').unwrap_or("d") {
            "d" => match b64_decode(&cmd.data) {
                Some(raw) => raw,
                None => return (cmd.id(), "EBADMSG:base64".to_string()),
            },
            // `t=f`: the payload is the base64 of the file's path.
            "f" => {
                let Some(path) =
                    b64_decode(&cmd.data).and_then(|p| String::from_utf8(p).ok())
                else {
                    return (cmd.id(), "EBADMSG:path".to_string());
                };
                match std::fs::read(path.trim()) {
                    Ok(raw) => raw,
                    Err(e) => return (cmd.id(), format!("ENOENT:{e}")),
                }
            }
            _ => return (cmd.id(), "EINVAL:unsupported medium".to_string()),
        };
        let fmt = cmd.get('f').unwrap_or("100");
        let rgba: Option<(Vec<u8>, u32, u32)> = match fmt {
            "100" => decode_png(&raw),
            "32" | "24" => {
                let (w, h) = (cmd.num('s'), cmd.num('v'));
                match (w, h) {
                    (Some(w), Some(h)) => {
                        let bpp = if fmt == "24" { 3 } else { 4 };
                        if raw.len() < (w * h) as usize * bpp {
                            None
                        } else if bpp == 4 {
                            Some((raw[..(w * h * 4) as usize].to_vec(), w, h))
                        } else {
                            let mut out = Vec::with_capacity((w * h * 4) as usize);
                            for px in raw[..(w * h * 3) as usize].chunks_exact(3) {
                                out.extend_from_slice(px);
                                out.push(255);
                            }
                            Some((out, w, h))
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((px, w, h)) = rgba else {
            return (cmd.id(), "EINVAL:decode".to_string());
        };
        let image = ImageData {
            data: Blob::new(std::sync::Arc::new(px)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: w,
            height: h,
        };
        let id = cmd.id();
        // Replace same-id placement.
        self.images.retain(|img| img.id != id);
        self.images.push(KittyImage {
            id,
            brush: peniko::ImageBrush::new(image),
            line,
            col,
            cols: cmd.num('c').unwrap_or(0),
            rows: cmd.num('r').unwrap_or(0),
            px_w: w,
            px_h: h,
        });
        (id, "OK".to_string())
    }
}

/// PNG → RGBA8. Kept small via the `png` crate.
fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder.read_info().ok()?;
    let out_size = reader.output_buffer_size();
    if out_size == 0 {
        return None;
    }
    let mut buf = vec![0u8; out_size];
    let info = reader.next_frame(&mut buf).ok()?;
    let bytes = buf[..info.buffer_size()].to_vec();
    match info.color_type {
        png::ColorType::Rgba => Some((bytes, info.width, info.height)),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for px in bytes.chunks_exact(3) {
                out.extend_from_slice(px);
                out.push(255);
            }
            Some((out, info.width, info.height))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys() {
        let cmd = parse(b"Ga=T,f=100,i=7;iVBOR").unwrap();
        assert_eq!(cmd.get('a'), Some("T"));
        assert_eq!(cmd.get('f'), Some("100"));
        assert_eq!(cmd.id(), 7);
        assert_eq!(cmd.data, "iVBOR");
    }

    #[test]
    fn b64_roundtrip() {
        assert_eq!(b64_decode("QUJD").unwrap(), b"ABC");
        assert_eq!(b64_decode("QUJDRA==").unwrap(), b"ABCD");
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        assert!(b64_decode("!!").is_none());
    }

    #[test]
    fn b64_unpadded_tail() {
        // "AA==" → 1 byte; "AAA" (no pad) also yields 2 bytes.
        assert_eq!(b64_decode("AA==").unwrap(), vec![0]);
        assert_eq!(b64_decode("AAA").unwrap(), vec![0, 0]);
    }

    #[test]
    fn rgb_expands_alpha() {
        let mut store = KittyStore::default();
        // f=24 RGB, 1x1 red pixel.
        let cmd = parse(b"Ga=T,f=24,s=1,v=1;/wAA").unwrap();
        let (id, status) = store.handle(cmd, 5, 3);
        assert_eq!((id, status.as_str()), (0, "OK"));
        assert_eq!(store.images.len(), 1);
        assert_eq!(store.images[0].line, 5);
        assert_eq!(store.images[0].col, 3);
    }

    #[test]
    fn chunked_assembly() {
        let mut store = KittyStore::default();
        let first = parse(b"Ga=T,f=24,s=1,v=1,m=1;/w").unwrap();
        let (id, s1) = store.handle(first, 0, 0);
        assert_eq!((id, s1.as_str()), (0, "OK"));
        assert!(store.images.is_empty());
        let second = parse(b"Gm=0;AA").unwrap();
        let (_, s2) = store.handle(second, 0, 0);
        assert_eq!(s2, "OK");
        assert_eq!(store.images.len(), 1);
    }
}

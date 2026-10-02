//! kitty graphics protocol subset: `ESC _ G <keys>;<base64> ST`.
//!
//! Supported: `a=T` transmit+display, `a=t` transmit-only (payload held
//! under `i=` for later `a=p` puts), `a=p` put-from-id, `a=d` delete
//! (selectors `d=i|p|z|c|a`), `a=q` query, `q=` quiet; formats `f=100`
//! (PNG), `f=32` (RGBA) and `f=24` (RGB) with `s=`/`v=` pixel dims;
//! `m=` chunk assembly; `i=`/`I=` image ids, `p=` placement ids;
//! `c=`/`r=` cell spans, `z=` layering (negative below the text),
//! `x`/`y`/`w`/`h` source crops on both `a=T` and `a=p`; `t=d` inline,
//! `t=f` regular-file and `t=s` POSIX-shm mediums. Placement anchors
//! to the cursor row at put time and scrolls with the buffer.
//! Not supported: unicode placements (`u=`, `U=`/`U?` virtual
//! placements), `t=t` shared memory, `t=o` file-descriptor passing,
//! `a=f` frame animation, `C=`/`o=`/`H=`/`V=`/relative extents.

use std::cell::OnceCell;
use std::collections::BTreeMap;

use waterui_graphics::Registered;
use waterui_graphics::cherenkov::{Image, ImageData, Rgba8};

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
    // Commands like `a=d`/`a=q` carry no `;` payload.
    let (keys, data) = text.split_once(';').unwrap_or((text, ""));
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
    /// RGBA pixel data, registered with the scene engine on first draw.
    pub data: ImageData<Rgba8>,
    /// Engine-side registration, filled by the first recording that draws it.
    pub registered: OnceCell<Registered<Image<Rgba8>>>,
    /// Absolute line index (same convention as prompt marks).
    pub line: i64,
    pub col: usize,
    /// Display size in cells (0,0 = native pixel size).
    pub cols: u32,
    pub rows: u32,
    /// Z-index: negative draws below the text layer.
    pub z: i32,
    /// Placement id (`p=`); 0 = default placement.
    pub placement: u32,
    /// Source pixel size for the draw transform.
    pub px_w: u32,
    pub px_h: u32,
}

/// A transmitted image payload kept by id — `a=p` placements draw
/// from it without re-sending data.
struct StoredImage {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// Per-session image store + in-flight chunked transmission.
#[derive(Default)]
pub struct KittyStore {
    /// Placements (`a=T` or `a=p`) — what the renderer draws.
    pub images: Vec<KittyImage>,
    /// Transmitted payloads by image id (`a=t`/`a=T` populate it).
    data: BTreeMap<u32, StoredImage>,
    /// Bytes currently held in `data` — `image-storage-limit` accounting.
    data_bytes: usize,
    pending: Option<(BTreeMap<char, String>, String)>,
}

/// Outcome of handling one command: `(image id, status text)`.
pub type Handled = (u32, String);

impl KittyStore {
    /// Feed one parsed command; returns the `(id, status)` reply payload.
    /// `limit` is `image-storage-limit` — bytes the payload store may hold
    /// before an insert is refused with `ETOOBIG` (Ghostty default 320MB).
    pub fn handle(&mut self, cmd: KittyCmd, line: i64, col: usize, limit: usize) -> Handled {
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
                self.delete(&cmd, line, col);
                (cmd.id(), "OK".to_string())
            }
            "q" => (cmd.id(), "OK".to_string()),
            // `a=t` transmits only — kitty keeps the payload under `i=`
            // and displays nothing until a later `a=p`.
            "t" => match self
                .decode(&cmd)
                .and_then(|s| self.store_checked(cmd.id(), s, limit))
            {
                Ok(()) => (cmd.id(), "OK".to_string()),
                Err(e) => (cmd.id(), e),
            },
            "T" | "" => self.place(&cmd, line, col, limit),
            // `a=p` puts a previously transmitted image at the cursor —
            // the payload is empty; `i=` selects the stored image.
            "p" => self.put(&cmd, line, col),
            _ => (cmd.id(), "EINVAL:unsupported action".to_string()),
        }
    }

    /// `a=p` — display a stored image. `i=` (image id) is required;
    /// `p=` names the placement, `x`/`y`/`w`/`h` crop the stored
    /// pixels, `c`/`r`/`z` size and layer it.
    fn put(&mut self, cmd: &KittyCmd, line: i64, col: usize) -> Handled {
        let Some(stored) = self.data.get(&cmd.id()) else {
            return (cmd.id(), "ENOENT:image id".to_string());
        };
        let (px, w, h) = (stored.rgba.clone(), stored.w, stored.h);
        let (px, w, h) = match (cmd.num('w'), cmd.num('h')) {
            (Some(cw), Some(ch)) => {
                let (cx, cy) = (cmd.num('x').unwrap_or(0), cmd.num('y').unwrap_or(0));
                match crop_rgba(&px, w, h, cx, cy, cw, ch) {
                    Some(c) => c,
                    None => return (cmd.id(), "EINVAL:crop".to_string()),
                }
            }
            _ => (px, w, h),
        };
        self.push_image(cmd, line, col, px, w, h);
        (cmd.id(), "OK".to_string())
    }

    /// `a=d` delete: `d=` selects the target — a(ll), i(image id), p(placement
    /// under an id), z(z-index), c(placements intersecting the cursor cell).
    fn delete(&mut self, cmd: &KittyCmd, line: i64, col: usize) {
        match cmd.get('d').unwrap_or("a") {
            // Deleting by image id also drops the stored payload — `a=p`
            // has nothing left to place.
            "i" => {
                if let Some(id) = cmd.num('i') {
                    self.images.retain(|img| img.id != id);
                    if let Some(s) = self.data.remove(&id) {
                        self.data_bytes -= s.rgba.len();
                    }
                }
            }
            "p" => {
                if let (Some(id), Some(p)) = (cmd.num('i'), cmd.num('p')) {
                    self.images
                        .retain(|img| !(img.id == id && img.placement == p));
                }
            }
            "z" => {
                if let Some(z) = cmd.get('z').and_then(|v| v.parse::<i32>().ok()) {
                    self.images.retain(|img| img.z != z);
                }
            }
            "c" => self.images.retain(|img| {
                let rows = img.rows.max(1) as i64;
                let cols = img.cols.max(1) as usize;
                !(line >= img.line
                    && line < img.line + rows
                    && col >= img.col
                    && col < img.col + cols)
            }),
            // 'a' and any unknown selector: keep the pre-selector behavior —
            // `i=` narrows to one id, otherwise clear the whole store.
            _ => match cmd.num('i') {
                Some(id) => {
                    self.images.retain(|img| img.id != id);
                    if let Some(s) = self.data.remove(&id) {
                        self.data_bytes -= s.rgba.len();
                    }
                }
                None => {
                    self.images.clear();
                    self.data.clear();
                    self.data_bytes = 0;
                }
            },
        }
    }

    /// `a=T` (and bare transmits) — decode, store under `i=`, and place.
    fn place(&mut self, cmd: &KittyCmd, line: i64, col: usize, limit: usize) -> Handled {
        match self
            .decode(cmd)
            .and_then(|stored| self.store_checked(cmd.id(), stored, limit))
        {
            Ok(()) => {
                let stored = &self.data[&cmd.id()];
                let (px, w, h) = (stored.rgba.clone(), stored.w, stored.h);
                self.push_image(cmd, line, col, px, w, h);
                (cmd.id(), "OK".to_string())
            }
            Err(e) => (cmd.id(), e),
        }
    }

    /// Insert a decoded payload honoring `image-storage-limit`: a
    /// rejected store frees nothing and reports `ETOOBIG` like kitty.
    fn store_checked(&mut self, id: u32, stored: StoredImage, limit: usize) -> Result<(), String> {
        let replacing = self.data.get(&id).map(|s| s.rgba.len()).unwrap_or(0);
        if self.data_bytes - replacing + stored.rgba.len() > limit {
            return Err("ETOOBIG:image-storage-limit".to_string());
        }
        self.data_bytes -= replacing;
        self.data_bytes += stored.rgba.len();
        self.data.insert(id, stored);
        Ok(())
    }

    /// Payload → RGBA + source dims, honoring `t=` medium, `f=` format
    /// and the `x`/`y`/`w`/`h` source crop.
    fn decode(&mut self, cmd: &KittyCmd) -> Result<StoredImage, String> {
        let raw = match cmd.get('t').unwrap_or("d") {
            "d" => match b64_decode(&cmd.data) {
                Some(raw) => raw,
                None => return Err("EBADMSG:base64".to_string()),
            },
            // `t=f`: the payload is the base64 of the file's path.
            "f" => {
                let Some(path) = b64_decode(&cmd.data).and_then(|p| String::from_utf8(p).ok())
                else {
                    return Err("EBADMSG:path".to_string());
                };
                match std::fs::read(path.trim()) {
                    Ok(raw) => raw,
                    Err(e) => return Err(format!("ENOENT:{e}")),
                }
            }
            // `t=s`: payload is the base64 of a POSIX shm name (leading `/`).
            // A shm object is not a filesystem file — it must be opened with
            // shm_open and unlinked once read.
            "s" => {
                let Some(name) = b64_decode(&cmd.data).and_then(|p| String::from_utf8(p).ok())
                else {
                    return Err("EBADMSG:shm name".to_string());
                };
                let name = name.trim();
                if name.contains("..") || !name.starts_with('/') {
                    return Err("EINVAL:shm name".to_string());
                }
                shm_read(name)?
            }
            _ => return Err("EINVAL:unsupported medium".to_string()),
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
                            let mut out = Vec::with_capacity((w * h * 3) as usize);
                            for px in raw[..(w * h * 3) as usize].as_chunks::<3>().0 {
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
            return Err("EINVAL:decode".to_string());
        };
        // `x,y,w,h` source crop (kitty places a sub-rectangle).
        let (px, w, h) = match (cmd.num('w'), cmd.num('h')) {
            (Some(cw), Some(ch)) => {
                let (cx, cy) = (cmd.num('x').unwrap_or(0), cmd.num('y').unwrap_or(0));
                match crop_rgba(&px, w, h, cx, cy, cw, ch) {
                    Some(c) => c,
                    None => return Err("EINVAL:crop".to_string()),
                }
            }
            _ => (px, w, h),
        };
        Ok(StoredImage { rgba: px, w, h })
    }

    /// One placement — `id`/`p=` replace any existing placement of the
    /// same pair; `c`/`r`/`z` size and layer it.
    fn push_image(&mut self, cmd: &KittyCmd, line: i64, col: usize, px: Vec<u8>, w: u32, h: u32) {
        let data = ImageData::<Rgba8>::new(w, h, px).expect("kitty image buffer length");
        let id = cmd.id();
        let placement = cmd.num('p').unwrap_or(0);
        self.images
            .retain(|img| !(img.id == id && img.placement == placement));
        self.images.push(KittyImage {
            id,
            data,
            registered: OnceCell::new(),
            line,
            col,
            cols: cmd.num('c').unwrap_or(0),
            rows: cmd.num('r').unwrap_or(0),
            z: cmd
                .get('z')
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0),
            placement,
            px_w: w,
            px_h: h,
        });
    }
}

/// PNG → RGBA8. Kept small via the `png` crate.
///
/// `EXPAND | ALPHA | STRIP_16` normalizes palette and low-depth sources,
/// but the `png` crate keeps 8-bit grayscale sources as Gray/LA — those
/// are expanded to Rgba8 by hand so the caller can always assume a
/// 4-bytes-per-pixel buffer (`ImageData<Rgba8>` is declared Rgba8; a
/// shorter buffer aborts inside wgpu's `write_texture` validation).
pub(crate) fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().ok()?;
    let out_size = reader.output_buffer_size();
    if out_size == 0 {
        return None;
    }
    let color_type = reader.output_color_type().0;
    let mut buf = vec![0u8; out_size];
    let info = reader.next_frame(&mut buf).ok()?;
    let buf = &buf[..info.buffer_size()];
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = match color_type {
        png::ColorType::Rgba => buf.to_vec(),
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return None,
    };
    debug_assert_eq!(rgba.len(), w * h * 4);
    Some((rgba, info.width, info.height))
}

/// Slice an RGBA8 buffer to `(x, y, w, h)`; bounds-checked.
fn crop_rgba(
    px: &[u8],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    cw: u32,
    ch: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if cw == 0 || ch == 0 || x.checked_add(cw)? > w || y.checked_add(ch)? > h {
        return None;
    }
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in y..y + ch {
        let s = ((row * w + x) * 4) as usize;
        out.extend_from_slice(&px[s..s + (cw * 4) as usize]);
    }
    Some((out, cw, ch))
}

/// `t=s` payload read: `shm_open` the transmitted name read-only, take
/// its bytes, then `shm_unlink` as the protocol requires — the object
/// dies with the read even when the read itself fails. The name was
/// already validated (`/`-leading, no `..`) at the decode boundary.
/// `rustix::shm` only exists where shm_open does, which excludes
/// Windows and a few embedded targets.
#[cfg(kitty_shm)]
fn shm_read(name: &str) -> Result<Vec<u8>, String> {
    use rustix::{fs::Mode, shm};
    let fd =
        shm::open(name, shm::OFlags::RDONLY, Mode::empty()).map_err(|e| format!("ENOENT:{e}"))?;
    let mut file = std::fs::File::from(fd);
    let mut raw = Vec::new();
    let read = std::io::Read::read_to_end(&mut file, &mut raw).map_err(|e| format!("ENOENT:{e}"));
    let unlink = shm::unlink(name).map_err(|e| format!("ENOENT:{e}"));
    read?;
    unlink?;
    Ok(raw)
}

#[cfg(not(kitty_shm))]
fn shm_read(_name: &str) -> Result<Vec<u8>, String> {
    Err("EINVAL:unsupported medium".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn palette_png_decodes_to_rgba() {
        // ImageMagick writes 2-color PNGs as color type 3 (palette); EXPAND
        // must normalize them to RGBA8.
        const B64: &str = concat!(
            "iVBORw0KGgoAAAANSUhEUgAAAEAAAAAgAgMAAADf85YXAAAABGdBTUEAALGPC/xhBQAAACBj",
            "SFJNAAB6JgAAgIQAAPoAAACA6AAAdTAAAOpgAAA6mAAAF3CculE8AAAACVBMVEX/AAAAAP//",
            "//8Ul8VoAAAAAWJLR0QCZgt8ZAAAAAd0SU1FB+oJFwEWFctSpBgAAAAVSURBVCjPY2CAglAo",
            "YBgVGBVACAAAA1dVAUNGf7UAAAAldEVYdGRhdGU6Y3JlYXRlADIwMjYtMDktMjNUMDE6MjI6",
            "MjErMDA6MDA4hzkqAAAAJXRFWHRkYXRlOm1vZGlmeQAyMDI2LTA5LTIzVDAxOjIyOjIxKzAw",
            "OjAwSdqBlgAAAABJRU5ErkJggg=="
        );
        let b64 = b64_decode(B64).unwrap();
        let (px, w, h) = decode_png(&b64).unwrap();
        assert_eq!((w, h, px.len()), (64, 32, 64 * 32 * 4));
        // Left half red, right half blue.
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&px[(63 * 4)..(64 * 4)], &[0, 0, 255, 255]);
    }

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
        let (id, status) = store.handle(cmd, 5, 3, usize::MAX);
        assert_eq!((id, status.as_str()), (0, "OK"));
        assert_eq!(store.images.len(), 1);
        assert_eq!(store.images[0].line, 5);
        assert_eq!(store.images[0].col, 3);
    }

    #[test]
    fn chunked_assembly() {
        let mut store = KittyStore::default();
        let first = parse(b"Ga=T,f=24,s=1,v=1,m=1;/w").unwrap();
        let (id, s1) = store.handle(first, 0, 0, usize::MAX);
        assert_eq!((id, s1.as_str()), (0, "OK"));
        assert!(store.images.is_empty());
        let second = parse(b"Gm=0;AA").unwrap();
        let (_, s2) = store.handle(second, 0, 0, usize::MAX);
        assert_eq!(s2, "OK");
        assert_eq!(store.images.len(), 1);
    }

    #[test]
    fn crop_slices_source() {
        // 2x2 RGBA, distinct channels per pixel; crop the right column.
        let px: Vec<u8> = (0u8..16).collect();
        let (out, w, h) = crop_rgba(&px, 2, 2, 1, 0, 1, 2).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, vec![4, 5, 6, 7, 12, 13, 14, 15]);
        assert!(crop_rgba(&px, 2, 2, 1, 0, 2, 2).is_none()); // out of bounds
        assert!(crop_rgba(&px, 2, 2, 0, 0, 0, 2).is_none()); // zero width
    }

    #[test]
    fn z_index_parses_signed() {
        let mut store = KittyStore::default();
        let cmd = parse(b"Ga=T,f=24,s=1,v=1,z=-1;/wAA").unwrap();
        let (_, s) = store.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(s.as_str(), "OK");
        assert_eq!(store.images[0].z, -1);
    }

    #[test]
    fn crop_placement() {
        let mut store = KittyStore::default();
        // f=32 RGBA 2x2, crop to the left column (x=0,y=0,w=1,h=2).
        let cmd = parse(b"Ga=T,f=32,s=2,v=2,x=0,y=0,w=1,h=2;AAAAAAAAAAAAAAAAAAAAAA==").unwrap();
        let (_, s) = store.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(s.as_str(), "OK");
        assert_eq!((store.images[0].px_w, store.images[0].px_h), (1, 2));
    }

    #[test]
    fn storage_limit_refuses_oversize() {
        let mut s = KittyStore::default();
        // 2x2 RGBA = 16 bytes; limit 8 → first store refused ETOOBIG.
        let cmd = parse(b"Ga=t,f=32,s=2,v=2,i=9;AAAAAAAAAAAAAAAAAAAAAA==").unwrap();
        let (_, st) = s.handle(cmd, 0, 0, 8);
        assert!(st.starts_with("ETOOBIG"), "{st}");
        // Limit raised → same payload stores; a=d frees the budget back.
        let cmd = parse(b"Ga=t,f=32,s=2,v=2,i=9;AAAAAAAAAAAAAAAAAAAAAA==").unwrap();
        let (_, st) = s.handle(cmd, 0, 0, 64);
        assert_eq!(st, "OK");
        let cmd = parse(b"Ga=d,d=i,i=9").unwrap();
        s.handle(cmd, 0, 0, 64);
        assert_eq!(s.data_bytes, 0);
    }

    fn place_rgba(store: &mut KittyStore, keys: &str, line: i64, col: usize) {
        let raw = "Ga=T,f=32,s=2,v=2".to_string() + "," + keys + ";AAAAAAAAAAAAAAAAAAAAAA==";
        let cmd = parse(raw.as_bytes()).unwrap();
        assert_eq!(store.handle(cmd, line, col, usize::MAX).1.as_str(), "OK");
    }

    #[test]
    fn delete_selectors() {
        let mut s = KittyStore::default();
        place_rgba(&mut s, "i=7,z=-1,c=2,r=2", 10, 3);
        place_rgba(&mut s, "i=8,z=2", 20, 5);
        place_rgba(&mut s, "i=9", 30, 7);
        // d=z removes only the z=-1 image.
        let cmd = parse(b"Ga=d,d=z,z=-1").unwrap();
        s.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(s.images.len(), 2);
        // d=c removes placements intersecting the cursor cell.
        let cmd = parse(b"Ga=d,d=c").unwrap();
        s.handle(cmd, 20, 5, usize::MAX);
        assert_eq!(s.images.len(), 1);
        assert_eq!(s.images[0].id, 9);
        // d=i removes by id.
        let cmd = parse(b"Ga=d,d=i,i=9").unwrap();
        s.handle(cmd, 0, 0, usize::MAX);
        assert!(s.images.is_empty());
    }

    #[test]
    fn placement_id_and_delete_p() {
        let mut s = KittyStore::default();
        place_rgba(&mut s, "i=7,p=3", 0, 0);
        place_rgba(&mut s, "i=7,p=5", 0, 0);
        assert_eq!(s.images.len(), 2);
        let cmd = parse(b"Ga=d,d=p,i=7,p=3").unwrap();
        s.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(s.images.len(), 1);
        assert_eq!(s.images[0].placement, 5);
    }

    #[cfg(kitty_shm)]
    #[test]
    fn shm_medium() {
        use rustix::{fs::Mode, shm};
        use std::io::Write;
        let name = format!(
            "/hydroterm-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let fd = shm::open(
            &name,
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        std::fs::File::from(fd).write_all(b"pixels").unwrap();
        let mut s = KittyStore::default();
        use base64::Engine as _;
        let raw = format!(
            "Ga=T,f=32,s=1,v=1,t=s;{}",
            base64::engine::general_purpose::STANDARD.encode(name.as_bytes())
        );
        let cmd = parse(raw.as_bytes()).unwrap();
        let (_, status) = s.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(status.as_str(), "OK"); // "pixels" is ≥4 bytes → f=32 1x1
        assert_eq!(s.images.len(), 1);
        // The decode path unlinked the object after reading it.
        let err = shm::open(&name, shm::OFlags::RDONLY, Mode::empty()).unwrap_err();
        assert_eq!(err, rustix::io::Errno::NOENT);
    }

    #[cfg(not(kitty_shm))]
    #[test]
    fn shm_medium() {
        let mut s = KittyStore::default();
        let raw = b"Ga=T,f=32,s=1,v=1,t=s;L25hbWU=".to_vec();
        let cmd = parse(&raw).unwrap();
        let (_, status) = s.handle(cmd, 0, 0, usize::MAX);
        assert_eq!(status.as_str(), "EINVAL:unsupported medium");
    }
}

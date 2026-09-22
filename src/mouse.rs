//! Mouse reporting: X10 / SGR / UTF-8 encodings, click+drag+wheel coverage,
//! and the modifier bits terminals layer on top.

use alacritty_terminal::term::TermMode;
use keyboard_types::Modifiers;
use waterui_graphics::input::SurfacePointerButton;

/// Which button a report is about, in protocol terms.
#[derive(Debug, Clone, Copy)]
pub enum MouseAction {
    /// 0/1/2 — left, middle, right press.
    Press(u8),
    /// Release of any button.
    Release,
    /// Motion while a button is held (motion bit set).
    Drag(u8),
    /// Any-cell motion reporting.
    Motion,
    /// Wheel: 64 up, 65 down, 66 left, 67 right (bits already encoded).
    Wheel(u8),
}

/// Map a surface mouse button to the protocol button number.
pub fn press_button(button: SurfacePointerButton) -> u8 {
    match button {
        SurfacePointerButton::Primary => 0,
        SurfacePointerButton::Middle => 1,
        SurfacePointerButton::Secondary => 2,
        SurfacePointerButton::Back => 8,
        SurfacePointerButton::Forward => 9,
    }
}

/// Modifier bits (SGR): shift=4, alt=8, ctrl=16.
fn mod_bits(mods: Modifiers) -> u8 {
    mods.contains(Modifiers::SHIFT) as u8 * 4
        + (mods.contains(Modifiers::ALT) || mods.contains(Modifiers::META)) as u8 * 8
        + mods.contains(Modifiers::CONTROL) as u8 * 16
}

/// 1-based column/row used by the wire protocols.
#[derive(Debug, Clone, Copy)]
pub struct CellPos {
    pub col: usize,
    pub row: usize,
}

/// Encode one report for the given mode; `None` when the mode doesn't want
/// this kind of event.
pub fn encode(
    action: MouseAction,
    pos: CellPos,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let (code, release) = match action {
        MouseAction::Press(b) => (b, false),
        MouseAction::Release => (3u8, true),
        MouseAction::Drag(b) => (b + 32, false),
        MouseAction::Motion => (35u8, false),
        MouseAction::Wheel(b) => (b, false),
    };
    let code = code + mod_bits(mods);

    let x = (pos.col + 1) as u32;
    let y = (pos.row + 1) as u32;
    if mode.contains(TermMode::SGR_MOUSE) {
        let fin = if release { 'm' } else { 'M' };
        Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes())
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        // urxvt-style: values offset by 32, UTF-8 encoded.
        let enc = |n: u32| -> String { char::from_u32(n + 32).unwrap_or(' ').to_string() };
        Some(format!("\x1b[{}{}{}M", enc(code as u32), enc(x), enc(y)).into_bytes())
    } else {
        // X10/DEC: three bytes, each value + 32, capped at 255.
        if x > 223 || y > 223 {
            return None;
        }
        let out = vec![0x1b, b'[', b'M', code + 32, (x as u8) + 32, (y as u8) + 32];
        Some(out)
    }
}

/// Wheel delta → button number and repeat count (lines per click).
pub fn wheel_button(delta_y: f64) -> Option<(u8, usize)> {
    if delta_y == 0.0 {
        return None;
    }
    if delta_y > 0.0 {
        Some((64, delta_y.ceil().max(1.0) as usize))
    } else {
        Some((65, (-delta_y).ceil().max(1.0) as usize))
    }
}

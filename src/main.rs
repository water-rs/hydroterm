//! hydroterm — a GPU terminal emulator on WaterUI's hydrolysis backend.
//!
//! Feature surface (Ghostty-aligned): alacritty_terminal VT emulation,
//! true-color/256 palette, bold/italic/dim/underline variants/strikeout/
//! inverse, ligatures + emoji/CJK fallback, scrollback + scrollbar, mouse
//! selection (click/drag/word/line/block), copy/paste with bracketed paste,
//! mouse reporting (X10/SGR/UTF-8 + wheel/drag), cursor styles + blink,
//! IME preedit, OSC title, tabs, font-size chords, alternate scroll,
//! OSC8 hyperlinks, bell, kitty keyboard protocol, in-surface search,
//! `key = value` config with hot reload, named themes, OSC 133 prompt
//! marks + jump, OSC 7 cwd inheritance, bash shell integration.

// `#[allow]` targets water-rs/lints (dylint) lint names — unknown to stable
// rustc, which would warn on the attribute itself.
#![allow(unknown_lints)]

mod app;
mod config;
mod fonts;
mod keys;
mod kitty;
mod osctap;
mod mouse;
mod palette;
mod quickterm;
mod scene;
mod surface;
mod terminal;
mod theme;
mod xcursor;

use waterui::app::App;
use waterui::prelude::*;
use waterui::window::Window;
use waterui_core::layout::{Rect, Size};

/// `hydroterm [--config PATH] [-e|-- COMMAND...]`
fn cli() -> (Option<std::path::PathBuf>, Option<Vec<String>>) {
    let mut config_path = None;
    let mut command = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().map(std::path::PathBuf::from),
            "-e" | "--" => {
                command = Some(args.by_ref().collect::<Vec<String>>());
                break;
            }
            _ => {}
        }
    }
    if let Some(cmd) = &command && cmd.is_empty() {
        command = None;
    }
    (config_path, command)
}

fn main() {
    let (config_path, command) = cli();
    let state = app::AppState::new(config_path, command);
    let title = state.window_title.clone();
    // A background Color below full opacity flips hydrolysis into a
    // transparent winit window; at 1.0 the window stays opaque.
    let opacity = state.config(|c| c.background_opacity);
    let bg = state.config(|c| c.resolve_theme().background);
    let window = Window::new(
        title,
        state.window_state.clone(),
        {
            let state = state.clone();
            move || app::app_root(state.clone())
        },
    )
    .background(Color::srgb(bg.r, bg.g, bg.b).with_opacity(opacity));
    // Window geometry: `window-save-state` restores the persisted frame;
    // otherwise `window-width`/`window-height` adjust the seeded 800x600
    // (keeping whatever origin the framework chose — position before map
    // is dropped by the WM anyway, hydrolysis#105).
    let rect = if state.config(|c| c.window_save_state) {
        app::load_window_state()
    } else {
        None
    };
    if let Some(rect) = rect {
        window.frame.set(rect);
    } else {
        let (w, h) = state.config(|c| (c.window_width, c.window_height));
        if w > 0.0 || h > 0.0 {
            let frame = window.frame.get();
            let size = *frame.size();
            window.frame.set(Rect::new(
                frame.origin(),
                Size::new(
                    if w > 0.0 { w } else { size.width },
                    if h > 0.0 { h } else { size.height },
                ),
            ));
        }
    }
    *state.window_frame.borrow_mut() = Some(window.frame.clone());
    let app = App::new_with_windows([window], Environment::new());
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

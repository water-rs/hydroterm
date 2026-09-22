//! hydroterm — a GPU terminal emulator on WaterUI's hydrolysis backend.
//!
//! Feature surface (Ghostty-aligned): alacritty_terminal VT emulation,
//! true-color/256 palette, bold/italic/dim/underline variants/strikeout/
//! inverse, ligatures + emoji/CJK fallback, scrollback + scrollbar, mouse
//! selection (click/drag/word/line/block), copy/paste with bracketed paste,
//! mouse reporting (X10/SGR/UTF-8 + wheel/drag), cursor styles + blink,
//! IME preedit, OSC title, tabs, font-size chords, alternate scroll,
//! OSC8 hyperlinks, bell, kitty keyboard protocol, in-surface search.

// `#[allow]` targets water-rs/lints (dylint) lint names — unknown to stable
// rustc, which would warn on the attribute itself.
#![allow(unknown_lints)]

mod app;
mod fonts;
mod keys;
mod osctap;
mod mouse;
mod palette;
mod scene;
mod surface;
mod terminal;

use waterui::app::App;
use waterui::prelude::*;

fn main() {
    let state = app::AppState::new();
    let title = state.window_title.clone();
    let app = App::new(
        {
            let state = state.clone();
            move || app::tabs_view(state.clone())
        },
        Environment::new(),
    )
    .title(title);
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

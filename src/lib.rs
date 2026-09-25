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
mod mouse;
mod osctap;
mod palette;
mod quickterm;
mod scene;
mod surface;
mod terminal;
mod theme;
mod xcursor;

use waterui::app::App;
use waterui::theme::Theme;
use waterui::Plugin;
use waterui::prelude::*;
use waterui::window::Window;
use waterui::window::WindowState::Fullscreen;
use waterui::window::WindowStyle::{Borderless, Titled};
use waterui_core::layout::{Point, Rect, Size};

/// `hydroterm [--config PATH] [-e|-- COMMAND...] [+ACTION]`
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
            _ if arg.starts_with('+') => cli_action(&arg[1..], config_path.as_deref()),
            _ => {}
        }
    }
    if let Some(cmd) = &command && cmd.is_empty() {
        command = None;
    }
    (config_path, command)
}

/// Ghostty-style `+action` one-shots — print and exit.
fn cli_action(name: &str, config_path: Option<&std::path::Path>) -> ! {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let code = match name {
        "list-themes" => {
            for t in crate::theme::THEMES {
                let _ = writeln!(out, "{t}");
            }
            0
        }
        "list-actions" => {
            for a in crate::config::ACTION_NAMES {
                let _ = writeln!(out, "{a}");
            }
            0
        }
        "show-config" => {
            let path = config_path
                .map(std::path::PathBuf::from)
                .unwrap_or_else(crate::config::default_path);
            let (cfg, errs) = crate::config::AppConfig::load(&path);
            let _ = writeln!(out, "# {}\n{cfg:#?}", path.display());
            for e in errs {
                eprintln!("{e}");
            }
            0
        }
        _ => {
            eprintln!(
                "hydroterm: unknown action '+{name}' — have: +list-themes, +list-actions, +show-config"
            );
            1
        }
    };
    let _ = out.flush();
    std::process::exit(code);
}

/// The `App` every hydrolysis entry point builds: one custom window whose
/// state, style and geometry come from the loaded config. `hydroterm`'s own
/// binary calls this too; the `water`-generated backend's `main` receives it
/// through the `{{crate}}::app(env)` contract.
pub fn app(env: Environment) -> App {
    let mut env = env;
    let (config_path, command) = cli();
    let state = app::AppState::new(config_path, command);
    // water-rs/cli#188: the app gives the runtime its color scheme through
    // the Environment — a `Computed` driven by `window-theme`, so a config
    // reload flips the chrome scheme without a restart. On hydrolysis pins
    // that install a default scheme after `app()` this is overwritten
    // (framework defect, WATERUI_FEEDBACK #46).
    Plugin::install(
        Theme::new().color_scheme(state.window_scheme.clone()),
        &mut env,
    );
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
    // `window-decoration = false` maps the window borderless (Ghostty
    // `window-decoration`); the default keeps the titled frame.
    .style(if state.config(|c| c.window_decoration) {
        Titled
    } else {
        Borderless
    })
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
            let frame = window.frame.snapshot();
            let size = *frame.size();
            window.frame.set(Rect::new(
                frame.origin(),
                Size::new(
                    if w > 0.0 { w } else { size.width },
                    if h > 0.0 { h } else { size.height },
                ),
            ));
        }
        // `window-position-x`/`window-position-y`: launch position
        // (hydrolysis#123 carries window-position support; KWin may
        // still place the window).
        let (wx, wy) = state.config(|c| (c.window_x, c.window_y));
        if wx.is_some() || wy.is_some() {
            let frame = window.frame.snapshot();
            let origin = frame.origin();
            window.frame.set(Rect::new(
                Point::new(wx.unwrap_or(origin.x), wy.unwrap_or(origin.y)),
                *frame.size(),
            ));
        }
    }
    *state.window_frame.borrow_mut() = Some(window.frame.clone());
    // `window-fullscreen`: every window starts fullscreen (same binding
    // F11 toggles, so it can be toggled back out).
    if state.config(|c| c.window_fullscreen) {
        state.window_state.set(Fullscreen);
    }
    App::new_with_windows([window], env)
}

/// The `hydroterm` binary's entry: identical to the generated backend's
/// `main` — `app(env)` carries the `window-theme` scheme in the
/// environment; the style is `Material3::defaults()` on both paths.
pub fn run() {
    hydrolysis::run(
        app(Environment::new()),
        hydrolysis_m3::Material3::defaults(),
    );
}

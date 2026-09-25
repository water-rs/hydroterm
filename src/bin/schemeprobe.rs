//! Minimal reproduction for the cli#188 environment-scheme contract.
//!
//! The app installs `Theme::color_scheme` into the environment `app(env)`
//! builds; `hydrolysis::run` then calls `install_default_tokens` followed by
//! `Style::install_tokens`. This probe replays exactly that order and prints
//! the installed `ColorScheme` after each step — if the app's scheme survives
//! to `Material3`, a `dark` `window-theme` would drive dark chrome.
//!
//! Run: `cargo run --bin schemeprobe`

use hydrolysis::Style;
use nami::{Computed, Signal};
use waterui::Environment;
use waterui::Plugin;
use waterui::theme::{ColorScheme, Theme, current_color_scheme};

fn main() {
    let mut env = Environment::new();
    // Step 0 — what `hydroterm::app` does: a Dark scheme driven by config.
    Theme::new()
        .color_scheme(Computed::constant(ColorScheme::Dark))
        .install(&mut env);
    println!(
        "after app(env):   {:?}",
        current_color_scheme(&env).snapshot()
    );

    // Step 1 — what `hydrolysis::run` does next (src/theme.rs).
    hydrolysis::theme::install_default_tokens(&mut env);
    println!(
        "after defaults:   {:?}",
        current_color_scheme(&env).snapshot()
    );

    // Step 2 — the generated backend's `Material3::defaults()` binds.
    hydrolysis_m3::Material3::defaults().install_tokens(&mut env);
    println!(
        "after material3:  {:?}",
        current_color_scheme(&env).snapshot()
    );
}

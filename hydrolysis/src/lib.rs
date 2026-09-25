//! Hydrolysis web entry point for hydroterm.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
use waterui::env::Environment;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn start() {
    let env = waterui::configure_environment!(Environment::new());
    let app = hydroterm::app(env);
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}
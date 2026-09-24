//! Minimal reproduction for WATERUI_FEEDBACK #36: a `when` gate and a
//! `text` leaf driven off the SAME `Option` binding paint one frame as
//! an empty container — nami's synchronous `WatcherManager::notify`
//! clears the leaf's content at `set(None)`, but the `when` node's
//! unmount only lands in the next Dynamic patch pass, so the frame
//! painted in between is a content-sized `Surface` card with no text.
//!
//! Shape — exactly hydroterm's resize badge (src/app.rs):
//!   `when(label.is_some(), || text(label.unwrap_or_default()).background(Surface))`
//! A task toggles `label` between `Some("80×24")` and `None` at ~11 Hz;
//! an empty Surface card flashes in the Some→None transition window.
//!
//! Root cause cites:
//!   nami-core 0.3.3 `src/watcher.rs` — `WatcherManager::notify` drains
//!     watchers synchronously inside `set`, so the text leaf's content
//!     patch reaches the renderer immediately;
//!   hydrolysis `src/runner/window.rs` — `Dynamic` subtree patches are
//!     applied on the next refresh pass (`apply pending Dynamic
//!     patches`), so the `when` gate's unmount lags the content clear
//!     by one frame.
//!
//! Evidence: r20_b1_5.png (badge with text) → r20_b1_6.png (same bbox,
//! Surface fill only, zero text pixels) → next frame fully unmounted.
use nami::Binding;
use waterui::Environment;
use waterui::app::App;
use waterui::prelude::*;
use waterui::theme::color::{Foreground, Surface};
use waterui::widget::condition::when;
use waterui::window::{Window, WindowState};

fn main() {
    use std::cell::Cell;
    use std::rc::Rc;
    use waterui::task::{sleep, spawn_local};

    let label = Binding::container(Some(Str::from("80×24")));
    let spawned = Rc::new(Cell::new(false));
    let window = Window::new("whenprobe", Binding::container(WindowState::Normal), move || {
        let label = label.clone();
        if !spawned.replace(true) {
            let toggler = label.clone();
            spawn_local(async move {
                for i in 0u64.. {
                    sleep(std::time::Duration::from_millis(90)).await;
                    if i % 2 == 0 {
                        toggler.set_from(Some(Str::from("80×24")));
                    } else {
                        toggler.set_from(None::<Str>);
                    }
                }
            })
            .detach();
        }
        let badge = when(label.is_some(), move || {
            text(label.unwrap_or_default().computed())
                .foreground(Foreground)
                .padding_horizontal(10.0)
                .padding_vertical(4.0)
                .background(Surface)
        });
        vstack((text("watch the bottom edge — an empty card flashes"), badge))
    });
    let app = App::new_with_windows([window], Environment::new());
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

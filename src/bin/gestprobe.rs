//! Minimal reproduction for WATERUI_FEEDBACK #33: `.gesture` targets are
//! re-registered with a FRESH recognizer on every scene flush, so an
//! in-flight drag dies the moment anything else repaints.
//!
//! Shape: [pane | divider | pane] where the divider carries
//! `.gesture(DragGesture)`. A binding flips every ~500ms and drives a
//! text view — forcing `clear_targets` + re-registration mid-drag.
//!
//! Expected: dragging the divider prints `[probe] phase=Updated ...`
//! continuously. Actual: Started fires (and even that is flaky) but no
//! Updated ever arrives once the first repaint lands — the recognizer
//! that saw PointerDown was replaced by one that did not.
use nami::Binding;
use waterui::app::App;
use waterui::layout::frame::Frame;
use waterui::prelude::*;
use waterui::window::{Window, WindowState};
use waterui_core::extract::Use;
use waterui_graphics::color::{BorderColor, Color, Srgb};

fn main() {
    use waterui::cursor::CursorStyle;
    use waterui::gesture::{DragEvent, DragGesture};
    use waterui::task::{sleep, spawn_local};

    // A signal that invalidates twice a second — every flip re-emits the
    // scene, which clears and re-registers every gesture target.
    let tick = Binding::container(Str::from("tick 0"));
    let window = Window::new(
        "gestprobe",
        Binding::container(WindowState::Normal),
        move || {
            let ticker = tick.clone();
            spawn_local(async move {
                for i in 1u64.. {
                    sleep(std::time::Duration::from_millis(500)).await;
                    ticker.set_from(Str::from(format!("tick {i}")));
                }
            })
            .detach();
            let divider = Frame::new(Color::new(BorderColor))
                .width(1.0)
                .max_height(f32::INFINITY);
            let divider = Frame::new(divider)
                .width(7.0)
                .max_height(f32::INFINITY)
                .cursor(CursorStyle::ResizeLeftRight)
                .gesture(
                    DragGesture::new(0.0),
                    move |event: Option<Use<DragEvent>>| {
                        eprintln!("[probe] fired present={}", event.is_some());
                        if let Some(e) = event {
                            eprintln!(
                                "[probe] phase={:?} t=({:.1},{:.1})",
                                e.0.phase, e.0.translation.x, e.0.translation.y
                            );
                        }
                    },
                )
                .anyview();
            // Invalidate-driven repaint: the tick count re-renders every 500ms.
            let clock = text(tick.clone()).anyview();
            let pane = |c: Srgb| Frame::new(Color::new(c)).opacity(1.0).anyview();
            vstack((
                Frame::new(Color::new(Srgb::from_hex("#888888"))).height(30.0),
                clock,
                hstack((
                    Frame::new(pane(Srgb::from_hex("#2196F3"))).width(396.0),
                    divider,
                    Frame::new(pane(Srgb::from_hex("#F44336"))).width(396.0),
                ))
                .spacing(0.0),
            ))
            .spacing(0.0)
        },
    );
    let app = App::new_with_windows([window], Environment::new());
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

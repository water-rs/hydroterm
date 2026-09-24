//! Minimal reproduction: a `.gesture(DragGesture)` handle sitting between two
//! embedded `SceneView` panes starves the moment the pointer crosses onto a
//! scene, because `handle_embedded_pointer_move` runs before the gesture
//! engine in `handle_pointer_move_inner` (hydrolysis
//! `src/renderer/input/hit_test.rs`) and returns `true` for every move that
//! lands on an embedded input target — so `gesture_engine.handle_pointer_move`
//! never sees them.
//!
//! Left/right panes are `SceneView`s whose content wants input events
//! (`wants_input_events() == true`), the divider is a 20pt drag zone.
//!
//! Expected: dragging across a pane prints `phase=Updated` continuously.
//! Actual: `Started` may fire while the pointer stays inside the 20pt zone,
//! then the event stream stops as soon as it crosses the scene boundary —
//! the moves are delivered to the scene (`[pane] input: PointerMove`) instead.
//! Dragging vertically inside the zone works, which rules out anything but
//! the move-routing order.
use kurbo::{BezPath, Rect, Shape};
use nami::Binding;
use peniko::{Brush, Fill};
use waterui::app::App;
use waterui::layout::frame::Frame;
use waterui::prelude::*;
use waterui::window::{Window, WindowState};
use waterui_graphics::color::{BorderColor, Color};
use waterui_graphics::input::SurfaceInputEvent;
use waterui_graphics::scene2d::Scene2D;
use waterui_graphics::scene_view::{SceneContent, SceneView};
use waterui_core::extract::Use;

struct Pane([f32; 4]);

impl SceneContent for Pane {
    fn build_scene(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) -> bool {
        let shape: BezPath = Rect::new(0.0, 0.0, f64::from(width), f64::from(height)).to_path(0.0);
        scene.fill(
            Fill::NonZero,
            kurbo::Affine::IDENTITY,
            &Brush::Solid(peniko::Color::new(self.0)),
            None,
            &shape,
        );
        false
    }

    fn wants_input_events(&self) -> bool {
        true
    }

    fn input(&mut self, event: &SurfaceInputEvent) {
        if matches!(event, SurfaceInputEvent::PointerMove { .. }) {
            eprintln!("[pane] input: PointerMove");
        }
    }
}

fn main() {
    use waterui::cursor::CursorStyle;
    use waterui::gesture::{DragEvent, DragGesture};

    let window = Window::new("dragstarve", Binding::container(WindowState::Normal), move || {
        let divider = Frame::new(Color::new(BorderColor))
            .width(1.0)
            .max_height(f32::INFINITY);
        let divider = Frame::new(divider)
            .width(20.0)
            .max_height(f32::INFINITY)
            .cursor(CursorStyle::ResizeLeftRight)
            .gesture(DragGesture::new(0.0), move |event: Option<
                Use<DragEvent>,
            >| {
                if let Some(e) = event {
                    eprintln!(
                        "[divider] phase={:?} t=({:.1},{:.1})",
                        e.0.phase, e.0.translation.x, e.0.translation.y
                    );
                }
            })
            .anyview();
        hstack((
            Frame::new(SceneView::new(Pane([0.13, 0.59, 0.95, 1.0]))).width(390.0),
            divider,
            Frame::new(SceneView::new(Pane([0.96, 0.26, 0.21, 1.0]))).width(390.0),
        ))
        .spacing(0.0)
    });
    let app = App::new_with_windows([window], Environment::new());
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

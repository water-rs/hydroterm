//! Probe: does `stack::Axis` installed by HStack reach children through
//! wrapper views (Frame / .cursor / .gesture / .state / AnyView collect)?
//! Each probe prints the Axis it sees when its body() runs.
use nami::Binding;
use waterui::app::App;
use waterui::component::stack::Axis;
use waterui::layout::frame::Frame;
use waterui::prelude::*;
use waterui::window::{Window, WindowState};
use waterui_graphics::color::{Color, Srgb};
use waterui_core::extract::Use;

struct AxisProbe(&'static str);
impl View for AxisProbe {
    fn body(self, env: &Environment) -> impl View {
        eprintln!("[axis-probe:{}] {:?}", self.0, env.get::<Axis>());
        Frame::new(Color::new(Srgb::from_hex("#00AA00"))).width(4.0)
    }
}

fn main() {
    use waterui::cursor::CursorStyle;
    use waterui::gesture::{DragEvent, DragGesture};
    let sizes = Binding::container(vec![1.0f32, 1.0]);
    let window = Window::new("axisprobe", Binding::container(WindowState::Normal), move || {
        let pane = |c: Srgb| Frame::new(Color::new(c)).opacity(1.0).anyview();
        let wrapped = Frame::new(AxisProbe("in-frame+cursor+gesture"))
            .cursor(CursorStyle::ResizeLeftRight)
            .state(&sizes)
            .gesture(DragGesture::new(0.0), move |_e: Option<Use<DragEvent>>| {})
            .anyview();
        // Vec<AnyView> collect — the exact shape hydroterm's split loop uses.
        let views: Vec<AnyView> = vec![
            pane(Srgb::from_hex("#2196F3")),
            AxisProbe("direct").anyview(),
            wrapped,
            pane(Srgb::from_hex("#F44336")),
        ];
        views.into_iter().collect::<HStack<(Vec<AnyView>,)>>().spacing(0.0)
    });
    let app = App::new_with_windows([window], Environment::new());
    hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
}

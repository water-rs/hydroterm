//! The `hydroterm` binary — everything lives in the library so the
//! `water`-generated hydrolysis backend can drive `hydroterm::app(env)`
//! directly.

fn main() {
    hydroterm::run();
}

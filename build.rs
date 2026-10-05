fn main() {
    // `rustix::shm` exists exactly where POSIX shm_open does.
    cfg_aliases::cfg_aliases! {
        kitty_shm: { not(any(
            windows,
            target_os = "android",
            target_os = "espidf",
            target_os = "horizon",
            target_os = "vita",
            target_os = "wasi"
        )) },
    }
}

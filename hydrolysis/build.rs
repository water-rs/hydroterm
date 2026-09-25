//! Embeds the Windows icon resource when targeting Windows.
//!
//! `app-icon.ico` is staged next to this crate by the water CLI before the
//! build; the executable's taskbar and Explorer icon come from it.

fn main() {
    // This crate is generated outside the application crate root, so its own
    // `CARGO_MANIFEST_DIR` has no `i18n/`. Hand the application's translation
    // directory to `catalog!`/`text!` through `WATERUI_I18N_DIR` so runtime
    // `text("...")` lookups resolve against the app's catalog here too. The
    // directory is watched so adding, editing or removing a locale file
    // rebuilds this crate with the new catalog.
    let i18n_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("i18n");
    println!("cargo:rustc-env=WATERUI_I18N_DIR={}", i18n_dir.display());
    if i18n_dir.is_dir() {
        println!("cargo:rerun-if-changed={}", i18n_dir.display());
    }

    println!("cargo:rerun-if-changed=app-icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    assert!(
        std::path::Path::new("app-icon.ico").exists(),
        "app-icon.ico is missing; build through the water CLI so the app icon is staged first"
    );
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("app-icon.ico");
    resource
        .compile()
        .expect("failed to embed the Windows icon resource");
}
//! Driven Tauri shell entry point.
//!
//! Per `design/SPEC.md` s14, plugin order matters in Tauri v2 - register
//! `tauri-plugin-single-instance` first (M5 wires it in), then deep-link.
//! For M0 we ship the bare minimum that builds and opens a window.

mod i18n;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();
    i18n::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .run(tauri::generate_context!())
        .expect("error while running Driven Tauri application");
}

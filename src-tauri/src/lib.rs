//! Driven Tauri shell entry point (SPEC s11-s17).
//!
//! Owns the boot path: plugin wiring in the SPEC s14 order
//! (single-instance FIRST, then deep-link, then autostart + notification),
//! a `.setup()` that runs migrations, assembles + spawns the per-account
//! orchestrators, manages the [`AppState`], builds the tray, and installs the
//! panic hook, plus the SPEC s11.3 sync IPC command registration.

mod app_state;
mod assembly;
mod commands;
mod crypto_provider_impl;
mod elevation;
mod events;
mod i18n;
mod migrations;
mod panic_hook;
mod tray;

use std::path::PathBuf;

use tauri::Manager;

pub use app_state::{AccountHandle, AppState, RemoteMode};

/// Resolve the SQLite state-DB path under the OS config dir
/// (`<config_dir>/driven/state.db`, SPEC s2).
///
/// TODO(M5): derive from `app.path().app_config_dir()` inside `.setup()`
/// instead of this placeholder (which exists so the boot path compiles).
fn state_db_path() -> PathBuf {
    todo!("M5: <app_config_dir>/driven/state.db via app.path().app_config_dir()")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();
    i18n::init();
    // SPEC s17: install the crash-dump panic hook before anything can panic.
    panic_hook::install();

    tauri::Builder::default()
        // SPEC s14: single-instance MUST be registered FIRST so deep-link can
        // hook its second-launch callback (forwarding URLs + argv to the
        // primary instance). The callback focuses/surfaces the existing window.
        .plugin(tauri_plugin_single_instance::init(|_app, _argv, _cwd| {
            // TODO(M5): show_window(app, "main", Route::default()) +
            // handle_argv(app, argv) (parse --minimized / --restore <path>).
        }))
        // SPEC s14: deep-link SECOND so it hooks the single-instance callback.
        .plugin(tauri_plugin_deep_link::init())
        // SPEC s13: autostart (LaunchAgent on macOS; registry/.desktop
        // elsewhere) with the --minimized arg so login start boots to tray.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let handle = app.handle().clone();
            // Boot path (SPEC s11): migrations -> assemble + spawn
            // orchestrators -> manage AppState -> build tray. Async work runs
            // on the Tauri async runtime; failures abort startup.
            tauri::async_runtime::block_on(async move {
                let db_path = state_db_path();
                let state = migrations::run(&db_path).await?;
                let app_state = assembly::build_and_spawn(&handle, state).await?;
                handle.manage(app_state);
                tray::build(&handle)?;
                Ok::<(), anyhow::Error>(())
            })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::sync::sync_now,
            commands::sync::pause_sync,
            commands::sync::resume_sync,
            commands::sync::get_sync_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Driven Tauri application");
}

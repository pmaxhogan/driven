//! Driven Tauri shell entry point (SPEC s11-s17).
//!
//! Owns the boot path: plugin wiring in the SPEC s14 order
//! (single-instance FIRST, then deep-link, then autostart + notification),
//! a `.setup()` that runs migrations, assembles + spawns the per-account
//! orchestrators, manages the [`AppState`], builds the tray, wires the
//! deep-link `on_open_url` callback, and shows-or-hides the main window based
//! on the `--minimized` flag, plus the SPEC s11.3 sync IPC command
//! registration and the clean-shutdown path on quit (ROADMAP M5: "Quit
//! cleanly shuts down the runtime, no orphaned tokio tasks").

// The `rust_i18n::i18n!` macro MUST be invoked at the crate root: it generates
// `crate::_rust_i18n_t`, which every `rust_i18n::t!` call site (tray, OS
// notifications) resolves against. Invoking it inside a submodule would place
// the helper at `crate::<module>::_rust_i18n_t` and break those call sites.
// The `locales` path is relative to `CARGO_MANIFEST_DIR` (src-tauri/).
rust_i18n::i18n!("locales", fallback = "en-US");

mod app_state;
mod assembly;
mod commands;
mod crypto_provider_impl;
// The elevation module is the complete M5-shipped "run elevated on login" /
// "restart elevated" public API (ROADMAP M3.5 deferred to M5). Its callers are
// the Settings IPC commands, which land in M6 (ROADMAP M6 "IPC commands per
// SPEC s11.1/s11.2/s11.6 fully wired"); M5's IPC surface is sync-only. The
// module is therefore reachable-but-uncalled until M6, so allow dead_code here
// rather than registering an M6-scope settings command early.
#[allow(dead_code)]
mod elevation;
mod events;
mod i18n;
mod migrations;
mod panic_hook;
mod tray;

use std::path::PathBuf;

use tauri::{Manager, RunEvent, WindowEvent};
use tauri_plugin_deep_link::DeepLinkExt;

pub use app_state::{AccountHandle, AppState, RemoteMode};

/// CLI flag (SPEC s13): boot straight to the tray with no visible window.
/// Passed by the autostart launcher so login start does not pop a window.
const ARG_MINIMIZED: &str = "--minimized";

/// CLI flag (DESIGN s4.1): quit a running instance. Reachable only via the
/// tray menu or this flag; a second launch carrying it asks the primary
/// instance (via the single-instance callback) to exit.
const ARG_QUIT: &str = "--quit";

/// The main window label, matching `tauri.conf.json` `app.windows[].label`
/// (SPEC s20). The window is declared there with `visible: false`, so it
/// exists hidden at boot and we show it for a normal (non-`--minimized`)
/// launch.
const MAIN_WINDOW: &str = "main";

/// `<config_dir>/driven/state.db` (SPEC s2), resolved from Tauri's
/// `app_config_dir()` (`config_dir() + identifier`). `app.driven` is the
/// `tauri.conf.json` identifier, so this is `<config_dir>/app.driven/...`;
/// the `driven/` segment keeps the state DB grouped with the logs the panic
/// hook + diagnostic bundle use.
fn state_db_path(app: &tauri::AppHandle) -> anyhow::Result<PathBuf> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|e| anyhow::anyhow!("resolve app_config_dir: {e}"))?;
    Ok(config_dir.join("driven").join("state.db"))
}

/// Show + focus the main window (a normal launch, a tray/dock click, or a
/// second-launch surface). No-op if the window is not present.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// `true` if `argv` carries the `--minimized` boot flag (SPEC s13).
fn argv_has_minimized(argv: &[String]) -> bool {
    argv.iter().any(|a| a == ARG_MINIMIZED)
}

/// `true` if `argv` carries the `--quit` flag (DESIGN s4.1).
fn argv_has_quit(argv: &[String]) -> bool {
    argv.iter().any(|a| a == ARG_QUIT)
}

/// Handle a deep-link URL forwarded to the primary instance. The window is
/// surfaced so the user sees the result; route-specific handling
/// (`driven://restore/...` etc.) is an M6+ concern - the M5 contract is that
/// a deep link wakes and shows the running app rather than spawning a
/// duplicate (SPEC s14).
fn handle_deep_link(app: &tauri::AppHandle, url: &str) {
    tracing::info!(target: "driven::app", url, "deep link opened");
    show_main_window(app);
}

/// Apply a second-launch invocation forwarded by the single-instance plugin
/// (SPEC s14): `--quit` exits the primary; otherwise we surface the existing
/// window unless the relaunch itself was `--minimized`. The deep-link plugin
/// hooks this same callback to forward URLs as argv on Windows/Linux.
fn handle_second_launch(app: &tauri::AppHandle, argv: &[String]) {
    if argv_has_quit(argv) {
        tracing::info!(target: "driven::app", "second launch requested quit");
        app.exit(0);
        return;
    }
    if argv_has_minimized(argv) {
        // A login-start relaunch while already running: nothing to surface.
        tracing::debug!(target: "driven::app", "second launch was --minimized; staying in tray");
        return;
    }
    show_main_window(app);
}

/// GRACEFULLY shut down every per-account task set on an explicit Quit
/// (R-P1-1 / R3-P1-1, ROADMAP M5 "no orphaned tokio tasks"; DESIGN s5.10.2
/// in-flight drain). For each account [`AccountHandle::shutdown`] signals the
/// orchestrator plus the watcher/event bridges, then await-with-budget and
/// abort-and-await EVERY tracked task (run loop, watcher bridge, event bridge,
/// power poller) so the process exits with zero orphaned tasks while still
/// giving an in-flight backup cycle a chance to finish rather than being killed
/// mid-upload.
///
/// Runs on the Tauri async runtime via `block_on` because the Tauri event-loop
/// callback (`RunEvent`) is synchronous.
///
/// R3-P1-1 (concurrency + no outer cancellation): the per-account drains run
/// CONCURRENTLY via [`futures::future::join_all`] - NOT serially - so two slow
/// accounts (each run loop up to [`app_state::RUN_LOOP_DRAIN_TIMEOUT`], each
/// poller the short abort budget) wind down in parallel rather than summing
/// their budgets. Critically there is NO outer `tokio::time::timeout` wrapping
/// the sweep: `AccountHandle::shutdown` is cancellation-UNSAFE (it has already
/// TAKEN each `JoinHandle` out of its slot, so dropping the future mid-drain
/// would DETACH - orphan - the in-flight aborted task). Each `drain_or_abort`
/// already self-bounds (await up to its budget, then `abort()` AND await the
/// aborted handle), so every per-account drain completes on its own; we let them
/// all finish instead of racing an outer cancellation that could orphan a task.
fn shutdown_orchestrators(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // Signal every orchestrator to stop AFTER its current cycle up front, so the
    // concurrent drains below see the stop flag already set (each account's
    // in-flight cycle winds down in parallel instead of one-at-a-time).
    for (account_id, handle) in state.accounts() {
        tracing::info!(target: "driven::app", account_id = %account_id, "signalling graceful shutdown on quit");
        handle.orchestrator.shutdown();
    }
    tauri::async_runtime::block_on(async move {
        // R3-P1-1: drive ALL per-account shutdowns concurrently. Each
        // `handle.shutdown()` self-bounds its per-task drains and aborts-and-
        // awaits anything that overruns, so no outer timeout is needed (and an
        // outer timeout would risk dropping a cancellation-unsafe drain mid-abort
        // -> an orphaned task). `join_all` returns only once EVERY account's
        // every task is finished.
        let drains = state.accounts().map(|(account_id, handle)| async move {
            handle.shutdown().await;
            tracing::info!(target: "driven::app", account_id = %account_id, "all per-account tasks shut down (no orphans)");
        });
        futures::future::join_all(drains).await;
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();
    // i18n must initialise before any tray/notification string is built;
    // keep this ahead of the builder (and ahead of the panic hook, which only
    // emits ASCII).
    i18n::init();
    // SPEC s17: install the crash-dump panic hook before anything can panic,
    // so a panic during plugin init / `.setup()` / assembly is captured too.
    panic_hook::install();

    let build_result = tauri::Builder::default()
        // SPEC s14: single-instance MUST be registered FIRST so deep-link can
        // hook its second-launch callback (forwarding URLs + argv to the
        // primary instance). The callback surfaces the existing window and
        // applies the forwarded argv (`--quit` / `--minimized` / a deep-link
        // URL passed as an arg on Windows + Linux).
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            handle_second_launch(app, &argv);
        }))
        // SPEC s14: deep-link SECOND so it hooks the single-instance callback.
        .plugin(tauri_plugin_deep_link::init())
        // SPEC s13: autostart (LaunchAgent on macOS; registry/.desktop
        // elsewhere) with the --minimized arg so login start boots to tray.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![ARG_MINIMIZED]),
        ))
        // SPEC s11.7 / M5: OS notifications (first-sync-done, error states).
        .plugin(tauri_plugin_notification::init())
        // V5-P1-1 / DESIGN s8.1: closing the main window HIDES it to the tray;
        // it does NOT quit the app or stop sync. The app keeps running in the
        // background; Quit is reachable only via the tray menu / `--quit`.
        .on_window_event(|window, event| {
            if window.label() == MAIN_WINDOW {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    // Prevent the close, hide to tray (sync keeps running).
                    api.prevent_close();
                    if let Err(err) = window.hide() {
                        tracing::warn!(target: "driven::app", %err, "hide-to-tray on window close failed");
                    } else {
                        tracing::debug!(target: "driven::app", "main window close hidden to tray (sync continues)");
                    }
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            // Boot path (SPEC s11): migrations -> assemble + spawn
            // orchestrators -> manage AppState -> build tray. Async work runs
            // on the Tauri async runtime; failures abort startup (and are
            // captured by the panic hook only if they panic - here they
            // propagate as a `setup` error and Tauri reports them).
            tauri::async_runtime::block_on(async move {
                let db_path = state_db_path(&handle)?;
                let state = migrations::run(&db_path).await?;
                let app_state = assembly::build_and_spawn(&handle, state).await?;
                handle.manage(app_state);
                tray::build(&handle)?;
                Ok::<(), anyhow::Error>(())
            })?;

            // SPEC s14: deep-link URLs arrive via this callback (NOT argv
            // parsing) - on macOS via the Apple event, on Windows/Linux via
            // the single-instance argv forwarding, transparently.
            let dl_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    handle_deep_link(&dl_handle, url.as_str());
                }
            });

            // C5-P2-3: drain any COLD-START deep link (a URL that launched the
            // app, before `on_open_url` was wired). The scheme is declared in
            // tauri.conf.json `plugins.deep-link.desktop.schemes`. Best-effort:
            // a plugin error or the no-URL case is logged, never fatal.
            match app.deep_link().get_current() {
                Ok(Some(urls)) => {
                    for url in urls {
                        handle_deep_link(app.handle(), url.as_str());
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(target: "driven::app", %err, "deep_link().get_current() cold-start drain failed");
                }
            }

            // SPEC s13 / s20: the main window is declared hidden in
            // tauri.conf.json. Show it for a normal launch; keep it hidden
            // (tray-only) when started with --minimized (e.g. from autostart
            // at login). `std::env::args` is the primary-process argv; the
            // second-launch argv is handled by the single-instance callback.
            let argv: Vec<String> = std::env::args().collect();
            if argv_has_quit(&argv) {
                // A first launch carrying --quit (no primary to forward to):
                // there is nothing running to quit, so honour it by exiting.
                app.handle().exit(0);
            } else if !argv_has_minimized(&argv) {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::sync::sync_now,
            commands::sync::pause_sync,
            commands::sync::resume_sync,
            commands::sync::get_sync_status,
        ])
        .build(tauri::generate_context!());

    // No `expect()` at the boundary (the workspace bans `unwrap`/`expect` in
    // non-test code): on a build failure, log it and exit non-zero rather than
    // panic. The panic hook would catch a panic here too, but a clean exit is
    // the right shape for an unrecoverable startup error.
    let app = match build_result {
        Ok(app) => app,
        Err(err) => {
            tracing::error!(target: "driven::app", %err, "failed to build Driven Tauri application");
            std::process::exit(1);
        }
    };

    // Drive the event loop ourselves so an EXPLICIT quit drains the orchestrator
    // run loops gracefully (DESIGN s5.10.2) and an INCIDENTAL last-window-close
    // does NOT kill the background sync (V5-P1-1, DESIGN s8.1).
    //
    // `RunEvent::ExitRequested.code`:
    //   - `Some(_)` => an explicit exit (`app.exit(code)` from the tray Quit /
    //     `--quit`). Drain + let the process exit.
    //   - `None`    => an incidental exit (the last window was closed). Since the
    //     app is a background tray daemon, `prevent_exit()` keeps it alive so
    //     sync survives. (The window-close handler already hid the window, but
    //     this guards the path where the platform still raises ExitRequested.)
    app.run(|app_handle, event| {
        if let RunEvent::ExitRequested { code, api, .. } = &event {
            if code.is_none() {
                tracing::debug!(target: "driven::app", "incidental exit (last window closed); staying alive in tray");
                api.prevent_exit();
            } else {
                tracing::info!(target: "driven::app", "explicit quit; draining orchestrators");
                shutdown_orchestrators(app_handle);
            }
        }
    });
}

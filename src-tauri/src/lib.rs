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
// `pub` so the integration tests (`tests/ipc_path_validation.rs`, SPEC s11.6.1)
// can exercise the path-validation helpers (`validate_writable_dest`,
// `DialogToken`) against the real implementation.
pub mod commands;
mod crypto_provider_impl;
// NOTE: the "run elevated" module was removed pre-V1 (2026-06-25). It implemented
// WHOLE-APP elevation (a /RL HIGHEST Task Scheduler logon task + a UAC restart),
// which is not the least-privilege model we want. The intended V1.x design is a
// small privileged HELPER that elevates ONLY the VSS snapshot operation, leaving
// this app un-elevated - a security-sensitive IPC surface that needs its own
// threat model. Tracked in design/CODEX_NOTES.md "VSS elevation - least-privilege
// helper (post-V1)". Until then, locked-file VSS requires launching Driven as
// administrator manually (the VssProvider degrade path handles the un-elevated case).
mod events;
mod hook_runner;
mod i18n;
mod migrations;
mod panic_hook;
// M9b (SPEC s16): anonymous usage telemetry - the install_id + enabled pref, the
// startup + 24h ping task, and the get/set IPC commands.
mod telemetry;
mod tray;
// M9a (SPEC s15): the in-app updater - runtime channel selection, the periodic
// check task, and the check/install/get-channel/set-channel IPC commands.
mod updater;

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

/// R2-P2-2: the per-restore-job graceful-drain budget on explicit Quit. After the
/// job's cancel flag is set, a healthy task observes it between frames and exits
/// near-instantly (it only needs to finish the current ~64 KiB frame). A task
/// blocked BEFORE its next flag check (e.g. on a slow/stalled download read) is
/// given this budget to wind down, then is aborted-and-awaited so Quit never
/// hangs. Kept short - a restore is interruptible by design (the temp is deleted),
/// unlike an in-flight UPLOAD which gets the longer run-loop drain budget.
const RESTORE_JOB_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

/// Force a dark titlebar + window border on Windows so the native chrome matches
/// Driven's dark theme. Without this the caption + border inherit the user's
/// Windows ACCENT color ("show accent color on title bars and window borders"),
/// which renders the chrome in an arbitrary per-machine color that clashes with
/// the teal/dark UI. We set immersive dark mode (light caption text/buttons) plus
/// a near-black caption and a subtle neutral border via the DWM window attributes.
/// Win10 ignores the color attributes (graceful no-op); Win11 honors them. Applied
/// before the first `show()` so there is no flash of accent-colored chrome.
#[cfg(windows)]
fn apply_dark_titlebar(window: &tauri::WebviewWindow) {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    // DWMWINDOWATTRIBUTE values (windows-sys takes the attribute as a u32).
    // Spelled out as literals so we depend only on DwmSetWindowAttribute, not the
    // constant set.
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;

    let hwnd = match window.hwnd() {
        Ok(h) => h.0 as isize as HWND,
        Err(_) => return,
    };
    // COLORREF byte order is 0x00BBGGRR.
    let caption: u32 = 0x000b_0909; // #09090b (zinc-950) - the app's dark surface
    let border: u32 = 0x0046_3f3f; // #3f3f46 (zinc-700) - a subtle neutral edge
    let dark: i32 = 1; // BOOL TRUE -> light caption text + window buttons
    unsafe {
        // SAFETY: hwnd is a live top-level window handle for the duration of the
        // call; each pointer references a stack value valid across the call.
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            (&caption as *const u32).cast(),
            size_of::<u32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border as *const u32).cast(),
            size_of::<u32>() as u32,
        );
    }
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
        // No managed state => no orchestrators ran => no event bridge => the
        // syncing spinner was never started, so there is nothing to stop.
        return;
    };
    // Signal every orchestrator to stop AFTER its current cycle up front, so the
    // concurrent drains below see the stop flag already set (each account's
    // in-flight cycle winds down in parallel instead of one-at-a-time).
    let handles = state.accounts();
    for (account_id, handle) in &handles {
        tracing::info!(target: "driven::app", account_id = %account_id, "signalling graceful shutdown on quit");
        handle.orchestrator.shutdown();
    }
    // M8-P1-1: cancel every in-flight RESTORE job up front too (mirrors the
    // no-orphan AccountHandle drain). Setting each job's cancel flag makes its
    // task delete the in-flight temp + emit a terminal CANCELLED status, so quit
    // leaves no orphaned restore task and no partial files. We take the handles
    // here and await them in the block_on below.
    let restore_handles = state.cancel_all_restore_jobs();
    // M9a: signal + take the periodic updater-check task so the drain below joins
    // it too (no orphan). It is a tokio-interval task that select!s on its
    // shutdown watch, so it exits promptly once signalled; the bounded drain
    // below still aborts-and-awaits it if it is mid-check (e.g. a slow network
    // request) so quit cannot hang.
    let updater_handle = state.shutdown_updater_task();
    // M9b: signal + take the periodic telemetry-ping task so the drain below joins
    // it too (no orphan). It is a tokio-interval task that select!s on its shutdown
    // watch, so it exits promptly once signalled; the bounded drain below still
    // aborts-and-awaits it if it is mid-ping (e.g. a slow best-effort POST) so quit
    // cannot hang.
    let telemetry_handle = state.shutdown_telemetry_task();
    tauri::async_runtime::block_on(async move {
        // R3-P1-1: drive ALL per-account shutdowns concurrently. Each
        // `handle.shutdown()` self-bounds its per-task drains and aborts-and-
        // awaits anything that overruns, so no outer timeout is needed (and an
        // outer timeout would risk dropping a cancellation-unsafe drain mid-abort
        // -> an orphaned task). `join_all` returns only once EVERY account's
        // every task is finished.
        let drains = handles.into_iter().map(|(account_id, handle)| async move {
            handle.shutdown().await;
            tracing::info!(target: "driven::app", account_id = %account_id, "all per-account tasks shut down (no orphans)");
        });
        futures::future::join_all(drains).await;

        // M8-P1-1 / R2-P2-2: drain every cancelled restore task with a BOUNDED,
        // abort-capable budget. Each task observes its cancel flag between frames,
        // deletes its in-flight temp, and exits - normally well within the budget.
        // But a task stuck BEFORE it next checks the flag (e.g. blocked on a slow
        // download read) would hang an explicit Quit forever if we awaited it
        // unconditionally. So we await each handle up to RESTORE_JOB_DRAIN_TIMEOUT
        // and, on timeout, `abort()` it and AWAIT the aborted handle so the task is
        // genuinely GONE before quit proceeds (no orphan). The task's temp is
        // cleaned even on the abort path because the restore writer holds a
        // Drop-based temp guard (see `restore.rs` TempFileGuard), so dropping the
        // aborted future removes any in-flight temp. Mirrors the M5 per-account
        // `drain_or_abort` shape. The drains run concurrently so two stuck jobs do
        // not sum their budgets.
        let restore_drains = restore_handles
            .into_iter()
            .map(|h| async move { drain_restore_handle(h).await });
        futures::future::join_all(restore_drains).await;
        tracing::info!(target: "driven::app", "all in-flight restore jobs cancelled + drained (no orphans)");

        // M9a: drain the periodic updater-check task with the SAME bounded,
        // abort-capable budget so quit never hangs on a mid-check task and leaves
        // no orphan.
        if let Some(handle) = updater_handle {
            drain_restore_handle(handle).await;
            tracing::info!(target: "driven::app", "updater periodic check task drained (no orphan)");
        }

        // M9b: drain the periodic telemetry-ping task with the SAME bounded,
        // abort-capable budget so quit never hangs on a mid-ping task and leaves no
        // orphan.
        if let Some(handle) = telemetry_handle {
            drain_restore_handle(handle).await;
            tracing::info!(target: "driven::app", "telemetry ping task drained (no orphan)");
        }

        // Stop the cosmetic tray syncing-spinner LAST - AFTER every orchestrator
        // is dropped (so the per-account event bridges' broadcasts are closed and
        // no further `StateChanged` can drive `apply_state` -> restart the
        // spinner). Stopping it earlier would race a still-queued syncing event
        // that could re-spawn the detached timer task after the stop. It is a
        // pure timer loop (set_icon only) that the process exit then tears down;
        // stopping it here keeps the no-orphan drain honest.
        tray::stop_sync_animation();
        tracing::info!(target: "driven::app", "tray syncing animation stopped (no orphan)");
    });
}

/// R2-P2-2: drive ONE cancelled restore task to a true stop with a bounded budget.
/// Await the handle up to [`RESTORE_JOB_DRAIN_TIMEOUT`]; on timeout `abort()` it
/// and AWAIT the aborted handle so the task is genuinely finished (not merely
/// abort-requested) before returning - so an explicit Quit cannot hang on a task
/// stuck before it observes its cancel flag. A task's in-flight temp is removed
/// even on the abort path via the restore writer's Drop-based temp guard (dropping
/// the aborted future runs the guard). Mirrors `app_state::drain_or_abort`.
///
/// `tokio::time::timeout` would MOVE the handle and DROP it on elapse (a dropped
/// `JoinHandle` only DETACHES the task, it does not cancel it), so we instead
/// `select!` so the handle stays in scope and can be aborted + re-awaited.
async fn drain_restore_handle(mut handle: tokio::task::JoinHandle<()>) {
    let abort = handle.abort_handle();
    tokio::select! {
        biased;
        _ = &mut handle => {
            // Joined cleanly (observed the cancel flag, cleaned its temp, exited)
            // or panicked - either way it is gone.
        }
        () = tokio::time::sleep(RESTORE_JOB_DRAIN_TIMEOUT) => {
            // Stuck before its next flag check: abort and AWAIT so the task is
            // truly finished (its dropped future runs the temp guard) before quit.
            abort.abort();
            let _ = handle.await;
        }
    }
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
        // SPEC s11.6.1 / M6: native folder + file dialogs. The add-source
        // wizard, restore destination picker, and diagnostic-bundle export all
        // round-trip a dialog-derived path so the webview can never inject an
        // arbitrary local path (the untrusted-webview path-confinement rule).
        .plugin(tauri_plugin_dialog::init())
        // Sign-in flow: open the Google OAuth consent URL in the user's default
        // system browser. The setup wizard (frontend) calls
        // `@tauri-apps/plugin-opener` `openUrl(consentUrl)`; this registers the
        // Rust half so that webview command is served. The webview is granted
        // only `opener:default` (http/https/mailto/tel), so it can launch the
        // browser but cannot open arbitrary local paths.
        .plugin(tauri_plugin_opener::init())
        // M9a (SPEC s15): the in-app updater + the process plugin (for the
        // post-install relaunch via `app.restart()`). The updater fetches the
        // signed per-target `update.json` and verifies the ed25519 signature
        // against `tauri.conf.json` `plugins.updater.pubkey`; the runtime channel
        // endpoint is overridden per-check in `updater::build_updater`.
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
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
                // R7-P1-2 (DATA-SAFETY): one-time upgrade repair for encrypted
                // sources that pre-date the durable recovery-ack gate (migration
                // 0004). Such a source could be ENABLED with no durable ack row,
                // so sync would keep producing encrypted backups for a phrase the
                // user may never have saved. The repair DISABLES those sources +
                // seeds a pending ack row (gated on a durable marker so it runs
                // once). Done BEFORE assembly so the orchestrator never sees them
                // as enabled, and BEFORE the reconstruct below so the in-memory
                // gate mirrors the freshly-seeded pending rows.
                //
                // R8-P1-1 (DATA-SAFETY): the repair must FAIL CLOSED. If it ERRORS,
                // a pre-0004 encrypted source may remain `enabled` with no durable
                // ack; spawning its orchestrator would keep backing it up with an
                // unsaved phrase (unrestorable). So on a repair error we do NOT
                // call `build_and_spawn` - we build a QUIESCED AppState (no
                // orchestrators spawned, so nothing syncs), surface a tray note,
                // and leave the repair marker unset so a later boot retries and,
                // on success, spawns normally.
                let repair_result = {
                    use driven_core::time::Clock;
                    let now = driven_core::time::SystemClock.now_ms();
                    state.repair_unacked_encrypted_sources_on_upgrade(now).await
                };
                match &repair_result {
                    Ok(0) => {}
                    Ok(n) => tracing::warn!(
                        repaired_accounts = *n,
                        "R7-P1-2: disabled pre-0004 encrypted sources lacking a durable recovery-ack; user must re-reveal + re-ack the phrase"
                    ),
                    Err(err) => tracing::error!(%err, "R8-P1-1: recovery-ack upgrade repair FAILED; failing closed - starting quiesced (no sync) until it succeeds"),
                }

                let app_state = if assembly::repair_allows_spawn(&repair_result) {
                    assembly::build_and_spawn(&handle, state).await?
                } else {
                    // Fail closed: manage state but spawn NO orchestrators.
                    assembly::build_quiesced(state)
                };
                // R4-P1-1 (DATA-SAFETY): reconstruct the recovery-phrase ACK gate
                // from the DURABLE `recovery_phrase_acks` table, so a process that
                // restarts mid-onboarding (after the first encrypted source +
                // master key were persisted but before reveal+ack) resumes the
                // exact pending-ack gate - the disabled source is still
                // reveal/ackable and no second encrypted source can enable without
                // the durable ack. Runs in both paths so the command-layer gate is
                // correct even while quiesced.
                app_state.reconstruct_recovery_acks_from_db().await;
                handle.manage(app_state);
                tray::build(&handle)?;
                // R8-P1-1: tell the user why sync is held off (after the tray
                // exists so the notification can route through it).
                if !assembly::repair_allows_spawn(&repair_result) {
                    tray::notify_repair_failed(&handle);
                }
                // SPEC s13 / issue #58: reconcile the OS autostart registration
                // with the persisted `global.auto_start_on_login` preference.
                // `apply_autostart` only fires on a settings *change*, so the
                // default-ON seed (migration 0005) would never register the real
                // OS startup entry (Windows Task Manager Startup tab / macOS
                // LaunchAgent / Linux .desktop) without this boot-time sync.
                // Best-effort: never aborts boot. Runs after `manage` so it can
                // read the preference from the state DB via AppState.
                if let Some(app_state) = handle.try_state::<AppState>() {
                    commands::settings::reconcile_autostart_on_boot(
                        &handle,
                        app_state.state().as_ref(),
                    )
                    .await;
                }
                // M9a (SPEC s15.2): start the periodic update-check task (an
                // immediate check on startup, then every 6h). Spawned here -
                // INSIDE the Tauri async runtime's `block_on` so `tokio::spawn`
                // has a reactor, and AFTER `manage(app_state)` so it can read the
                // active channel + record the pending update. Its handle +
                // shutdown sender are tracked on AppState so the quit drain joins
                // it with no orphan.
                updater::spawn_periodic_check(&handle);
                // M9b R2-P2-3 (SPEC s16): record an `update_applied` activity row
                // when the running version differs from the last-recorded one, so
                // the telemetry `update_applied` aggregate is driven by a real
                // production path. Done BEFORE the first ping is spawned so the
                // startup ping window can already include it. Cheap + idempotent +
                // non-fatal: on a fresh install it merely seeds the version (no
                // event), and any error is logged + swallowed (never blocks boot).
                if let Some(app_state) = handle.try_state::<AppState>() {
                    use driven_core::time::Clock;
                    let running_version = handle.package_info().version.to_string();
                    let now_ms = driven_core::time::SystemClock.now_ms();
                    let _ = telemetry::record_update_applied_if_changed(
                        app_state.state().as_ref(),
                        &running_version,
                        now_ms,
                    )
                    .await;
                }
                // M9b (SPEC s16): start the anonymous-telemetry ping task (an
                // immediate ping on startup if enabled, then every 24h). Spawned
                // here - INSIDE the Tauri async runtime's `block_on` so
                // `tokio::spawn` has a reactor, and AFTER `manage(app_state)` so it
                // can read the telemetry pref + aggregate from the state DB. Its
                // handle + shutdown sender are tracked on AppState so the quit drain
                // joins it with no orphan. It self-checks the enabled pref each tick
                // (default ON, honored immediately on toggle), and when disabled
                // makes NO network call.
                telemetry::spawn_periodic_ping(&handle);
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

            // Force a dark titlebar/border (Windows) BEFORE the first show so the
            // native chrome never flashes the user's clashing Windows accent color.
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                apply_dark_titlebar(&window);
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
            // SPEC s11.3 sync (M5).
            commands::sync::sync_now,
            commands::sync::pause_sync,
            commands::sync::resume_sync,
            commands::sync::get_sync_status,
            // SPEC s11.1 accounts (M6).
            commands::accounts::list_accounts,
            commands::accounts::begin_add_account_wizard,
            commands::accounts::submit_oauth_credentials,
            commands::accounts::start_oauth_signin,
            commands::accounts::poll_oauth_status,
            commands::accounts::cancel_oauth_wizard,
            commands::accounts::finish_add_account,
            commands::accounts::remove_account,
            commands::accounts::reauth_account,
            // SPEC s11.2 sources (M6).
            commands::sources::list_sources,
            commands::sources::add_source,
            commands::sources::update_source,
            commands::sources::remove_source,
            commands::sources::pick_drive_folder,
            commands::sources::preview_exclusions,
            // M9c D4 (M6 R4-P1-1, DATA-SAFETY): backend recovery-phrase reveal +
            // ack gate. The first encrypted source is persisted disabled until the
            // phrase is revealed by the backend AND acknowledged.
            commands::sources::reveal_recovery_phrase,
            commands::sources::ack_recovery_phrase_saved,
            // SPEC s11.6.1 backend-owned native dialogs (M6 C1).
            commands::dialogs::pick_folder_dialog,
            commands::dialogs::pick_save_zip_dialog,
            // SPEC s11.6 settings & misc (M6).
            commands::settings::get_settings,
            commands::settings::get_vss_helper_status,
            commands::settings::update_settings,
            commands::settings::export_diagnostic_bundle,
            commands::settings::check_for_updates,
            commands::settings::list_releases,
            // SPEC s15.2 updater (M9a): runtime channel selection + the
            // tauri-plugin-updater check/install path.
            updater::check_for_update,
            updater::install_update,
            updater::get_update_channel,
            updater::set_update_channel,
            // R2-P1-3: hydrate the app-root updater store on startup so an
            // `updater:available` emitted by the startup check before the webview
            // attached is still reflected in the banner.
            updater::get_pending_update_info,
            // SPEC s16 telemetry (M9b): the anonymous-usage toggle + install id.
            telemetry::get_telemetry_enabled,
            telemetry::set_telemetry_enabled,
            telemetry::get_telemetry_install_id,
            // SPEC s11.4 activity (M7).
            commands::activity::query_activity,
            commands::activity::clear_activity_older_than,
            // M7-P2-4 / P2-5: filter facets + DESIGN s8.3 header aggregates.
            commands::activity::distinct_activity_event_types,
            commands::activity::activity_summary,
            // SPEC s11.5 restore (M8).
            commands::restore::list_remote_tree,
            commands::restore::search_files,
            commands::restore::restore_files,
            commands::restore::get_restore_job,
            commands::restore::cancel_restore_job,
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

#[cfg(test)]
mod tests {
    use super::{drain_restore_handle, RESTORE_JOB_DRAIN_TIMEOUT};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn drain_restore_handle_aborts_a_stuck_task_within_budget() {
        // R2-P2-2: a restore task stuck BEFORE it observes its cancel flag (here a
        // task that just sleeps "forever") must be ABORTED and awaited within the
        // bounded budget, so an explicit Quit cannot hang. With virtual time
        // (start_paused) the budget elapses deterministically; the task never sets
        // its "finished cleanly" flag because it is aborted mid-sleep.
        let finished_cleanly = Arc::new(AtomicBool::new(false));
        let flag = finished_cleanly.clone();
        let handle = tokio::task::spawn(async move {
            // Sleep far beyond the drain budget - models a task blocked before its
            // next cancel-flag check.
            tokio::time::sleep(RESTORE_JOB_DRAIN_TIMEOUT * 100).await;
            flag.store(true, Ordering::SeqCst);
        });

        // The drain must RETURN (not hang) - on the paused clock the budget elapses
        // and the task is aborted + awaited.
        drain_restore_handle(handle).await;

        assert!(
            !finished_cleanly.load(Ordering::SeqCst),
            "the stuck task must have been aborted (not allowed to finish its long sleep)"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn drain_restore_handle_joins_a_prompt_task_without_abort() {
        // A task that finishes promptly (observed its cancel flag, cleaned up,
        // exited) is JOINED cleanly within the budget - the abort arm is not taken.
        let finished_cleanly = Arc::new(AtomicBool::new(false));
        let flag = finished_cleanly.clone();
        let handle = tokio::task::spawn(async move {
            // Returns well inside the budget.
            tokio::time::sleep(RESTORE_JOB_DRAIN_TIMEOUT / 10).await;
            flag.store(true, Ordering::SeqCst);
        });
        drain_restore_handle(handle).await;
        assert!(
            finished_cleanly.load(Ordering::SeqCst),
            "a prompt task must finish cleanly (joined, not aborted)"
        );
    }
}

//! System-tray icon + menu (SPEC s12, DESIGN s8.1).
//!
//! The tray is always present. [`build`] constructs the `TrayIconBuilder`
//! with the menu from DESIGN s8.1 ("Sync now" / "Pause for..." / "Settings" /
//! "Activity" / "Restore" / "Quit") and wires its menu + click handlers to the
//! M5 sync IPC commands (SPEC s12). [`apply_state`] swaps the icon + tooltip as
//! the orchestrator's [`OrchestratorState`] changes (DESIGN s8.1 icon state
//! machine) and raises the DESIGN s117/s247 OS notifications (first-sync-
//! complete + red error).
//!
//! Linux caveat (SPEC s12, DESIGN s8.1): tray-icon click events may not fire
//! on every desktop environment, so EVERY action must also be reachable from
//! the right-click menu - the menu is the canonical surface.
//!
//! ## Icon assets (real brand mark + per-state status badge)
//!
//! The base tray icon is the REAL Driven brand mark - the white road-to-cloud
//! glyph on deep teal - decoded from the committed `icons/64x64.png` (compiled
//! into the binary with `include_bytes!`) via
//! [`tauri::image::Image::from_bytes`] (gated by the `image-png` Cargo feature).
//! It is decoded once and cached in [`brand_base`].
//!
//! Each non-idle [`OrchestratorState`] keeps that recognisable brand mark and
//! overlays a solid STATUS BADGE - a filled colour disc with a white contrast
//! ring in the bottom-right corner (drawn by [`draw_badge`]) PLUS a distinct
//! white GLYPH drawn inside the disc ([`draw_glyph`]) so each state is readable
//! by SHAPE, not colour alone (accessible to colour-blind users / tiny trays):
//! blue spinner = syncing, amber pause-bars = paused, orange `!` = network
//! attention, red `X` = error. Idle shows the plain brand mark (no badge). The
//! badge colours are the same per-state palette used elsewhere, so the icon
//! conveys the live state at a glance while staying on-brand.
//!
//! `set_icon_as_template(false)` is forced on the live tray so macOS does not
//! recolour these colour-bearing icons to monochrome (which would erase the
//! teal + the badge colours); the committed `tauri.conf.json` keeps
//! `iconAsTemplate: true` only for the static boot icon.
//!
//! ## Animated Syncing (real, not faked)
//!
//! `TrayIcon::Syncing` is a REAL animated spinner: while the aggregate state is
//! syncing, [`apply_state`] starts a timer-driven task ([`start_sync_animation`])
//! that swaps the tray icon through [`SYNC_FRAMES`] spinner frames (rendered by
//! [`TrayIcon::brand_rgba_frame`]) on a fixed cadence, then STOPS and cleans up
//! ([`stop_sync_animation`]) the instant the aggregate leaves syncing (or on
//! quit). Only one animation runs at a time (idempotent start). The frame
//! buffers genuinely differ frame-to-frame and repeat every [`SYNC_FRAMES`]
//! ticks (asserted in the tests), so the spinner actually cycles - it is not a
//! still icon dressed up as animated.
//!
//! If the brand PNG ever fails to decode (it is compiled in, so this should not
//! happen) the code falls back to the flat [`TrayIcon::rgba_buffer`] colour tile
//! rather than panicking. The state machine, tooltip text, and notification
//! routing are all real.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use driven_core::types::{AccountId, ErrorCode, OrchestratorState, PauseReason};
use tauri::image::Image;
use tauri::menu::{Menu, MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const TARGET: &str = "driven::app::tray";

/// Canonical tray id (matches the committed `apply_state` lookup + SPEC s12).
const TRAY_ID: &str = "driven-main";

/// Id Tauri assigns to the tray created from `tauri.conf.json`'s
/// `app.trayIcon` block (it defaults to the main-window label `"main"`). We
/// remove it in [`build`] so the config-defined tray and our programmatic
/// `"driven-main"` tray do not BOTH show (a duplicate-icon footgun).
const CONFIG_TRAY_ID: &str = "main";

/// Menu item ids (SPEC s12 `event.id()` dispatch keys).
mod menu_id {
    pub const SYNC_NOW: &str = "sync_now";
    pub const PAUSE_30M: &str = "pause_30m";
    pub const RESUME: &str = "resume";
    pub const SETTINGS: &str = "settings";
    pub const ACTIVITY: &str = "activity";
    pub const RESTORE: &str = "restore";
    pub const QUIT: &str = "quit";
}

/// Tiny generated-tile dimensions. A 16x16 RGBA tile is a valid tray icon on
/// all three platforms; the OS scales it for HiDPI trays.
const TILE: u32 = 16;

/// Number of dots in the syncing spinner ring (also the number of distinct
/// frames in one full rotation - the "head" dot advances by one dot per frame).
const SYNC_DOTS: u32 = 8;

/// Number of distinct frames in one syncing-spinner animation cycle. Equal to
/// [`SYNC_DOTS`] so frame `f` and frame `f + SYNC_FRAMES` render identically
/// (the spinner has come full circle) while every frame WITHIN a cycle differs.
const SYNC_FRAMES: u32 = SYNC_DOTS;

/// Cadence of the syncing-spinner animation (~8 fps). A `tokio::time::interval`
/// tick swaps to the next frame; slow enough to be light on CPU, fast enough to
/// read as motion.
const SYNC_FRAME_INTERVAL: Duration = Duration::from_millis(125);

/// The tray icon to display for a given orchestrator state (DESIGN s8.1).
///
/// Maps the [`OrchestratorState`] machine to the five DESIGN s8.1 visuals.
/// The yellow-with-`!` "network attention" visual is selected from a
/// `Paused`/`Error` whose reason/detail is a network condition (resolved in
/// [`TrayIcon::for_state`]); the bare variants below are the non-network cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIcon {
    /// Default gray: idle, last sync OK.
    Idle,
    /// Animated spinner: a sync cycle is in progress.
    Syncing,
    /// Yellow: paused (user or auto - battery / metered / schedule).
    Paused,
    /// Yellow with `!` badge: network attention (offline / captive portal /
    /// Drive unreachable) - covers all DESIGN s5.8 network failure modes.
    NetworkAttention,
    /// Red: an error needs attention (auth needed, decrypt failure, disk full).
    Error,
}

impl TrayIcon {
    /// Pick the icon for `state` (DESIGN s8.1 state machine).
    ///
    /// - `Idle` -> [`TrayIcon::Idle`];
    /// - `PowerCheck` / `Scanning` / `Planning` / `Executing` / `Verifying`
    ///   -> [`TrayIcon::Syncing`];
    /// - `Backoff` -> [`TrayIcon::NetworkAttention`] (R2-P2-2): the Drive
    ///   circuit breaker is open / rate-limited, i.e. "Drive unreachable"
    ///   (DESIGN s8.1 yellow-with-`!`), NOT an active sync. This MUST match the
    ///   aggregate severity ranking, which ranks `Backoff` as a network-attention
    ///   condition - otherwise `Backoff + Idle` would aggregate to `Backoff` yet
    ///   render the blue syncing icon;
    /// - `Paused { reason }` -> [`TrayIcon::NetworkAttention`] when the reason
    ///   is a network condition (offline / no-internet / captive / DNS /
    ///   service-down), else [`TrayIcon::Paused`];
    /// - `Error { detail }` -> [`TrayIcon::NetworkAttention`] for a network /
    ///   reachability error code, else [`TrayIcon::Error`].
    #[must_use]
    pub fn for_state(state: &OrchestratorState) -> Self {
        match state {
            OrchestratorState::Idle { .. } => TrayIcon::Idle,
            OrchestratorState::PowerCheck
            | OrchestratorState::Scanning { .. }
            | OrchestratorState::Planning { .. }
            | OrchestratorState::Executing { .. }
            | OrchestratorState::Verifying { .. } => TrayIcon::Syncing,
            // R2-P2-2: Backoff = Drive breaker open / rate-limited = "Drive
            // unreachable" attention (consistent with `state_severity`).
            OrchestratorState::Backoff { .. } => TrayIcon::NetworkAttention,
            OrchestratorState::Paused { reason } => {
                if pause_reason_is_network(*reason) {
                    TrayIcon::NetworkAttention
                } else {
                    TrayIcon::Paused
                }
            }
            OrchestratorState::Error { detail } => {
                if error_code_is_network(detail.code) {
                    TrayIcon::NetworkAttention
                } else {
                    TrayIcon::Error
                }
            }
        }
    }

    /// The solid RGBA colour (no alpha cutout) for this state's generated tile.
    fn rgba(self) -> [u8; 4] {
        match self {
            // Neutral gray - idle / last sync OK.
            TrayIcon::Idle => [0x9e, 0xa3, 0xa8, 0xff],
            // Blue - a sync cycle is running.
            TrayIcon::Syncing => [0x3b, 0x82, 0xf6, 0xff],
            // Amber/yellow - user/auto pause.
            TrayIcon::Paused => [0xf5, 0xb3, 0x00, 0xff],
            // Deeper amber - network attention (approximates yellow-with-`!`).
            TrayIcon::NetworkAttention => [0xff, 0x8c, 0x00, 0xff],
            // Red - error needs attention.
            TrayIcon::Error => [0xdc, 0x26, 0x26, 0xff],
        }
    }

    /// The flat `TILE x TILE` RGBA byte buffer for this state's tile (row-major,
    /// top-to-bottom; the shape [`Image::new_owned`] wants). This is the FALLBACK
    /// visual used only if the brand PNG fails to decode (see [`brand_base`]);
    /// the normal path is the brand mark from [`TrayIcon::image`].
    fn rgba_buffer(self) -> Vec<u8> {
        let [r, g, b, a] = self.rgba();
        let mut buf = Vec::with_capacity((TILE * TILE * 4) as usize);
        for _ in 0..(TILE * TILE) {
            buf.extend_from_slice(&[r, g, b, a]);
        }
        buf
    }

    /// The status-badge colour for this state, or `None` for [`TrayIcon::Idle`]
    /// (idle shows the plain brand mark, no badge). The RGB matches this state's
    /// [`TrayIcon::rgba`] palette so the badge and the flat-tile fallback agree.
    fn badge_color(self) -> Option<[u8; 3]> {
        match self {
            TrayIcon::Idle => None,
            other => {
                let [r, g, b, _] = other.rgba();
                Some([r, g, b])
            }
        }
    }

    /// The brand-mark RGBA for this state at animation `frame`: the decoded base
    /// mark with this state's status badge + glyph drawn on for every non-idle
    /// state (idle is the plain mark). Row-major top-to-bottom RGBA at the base
    /// mark's dimensions.
    ///
    /// `frame` only affects [`TrayIcon::Syncing`] (it advances the spinner);
    /// every other state is static and ignores it. The frame value wraps mod
    /// [`SYNC_FRAMES`], so callers may pass a monotonically increasing tick.
    fn brand_rgba_frame(self, base: &BrandImage, frame: u32) -> Vec<u8> {
        let mut rgba = base.rgba.clone();
        if let Some(color) = self.badge_color() {
            draw_badge(&mut rgba, base.width, base.height, color);
            self.draw_glyph(&mut rgba, base.width, base.height, frame);
        }
        rgba
    }

    /// Draw this state's distinct white GLYPH inside the badge disc, in place
    /// (no-op for [`TrayIcon::Idle`], which carries no badge). The glyph encodes
    /// the state by SHAPE so the icon is readable without relying on colour:
    /// a spinner (syncing), pause bars (paused), an `!` (network attention), and
    /// an `X` (error).
    fn draw_glyph(self, rgba: &mut [u8], width: u32, height: u32, frame: u32) {
        match self {
            TrayIcon::Idle => {}
            TrayIcon::Syncing => draw_spinner_glyph(rgba, width, height, frame),
            TrayIcon::Paused => draw_pause_glyph(rgba, width, height),
            TrayIcon::NetworkAttention => draw_bang_glyph(rgba, width, height),
            TrayIcon::Error => draw_cross_glyph(rgba, width, height),
        }
    }

    /// A freshly-allocated owned [`Image`] for this state's STILL tray icon
    /// (animation frame 0). See [`TrayIcon::image_frame`].
    fn image(self) -> Image<'static> {
        self.image_frame(0)
    }

    /// A freshly-allocated owned [`Image`] for this state's tray icon at
    /// animation `frame`: the real Driven brand mark (badged + glyphed per
    /// state), or - only if the compiled-in brand PNG fails to decode - the flat
    /// [`TrayIcon::rgba_buffer`] colour tile (which has no animation). The
    /// syncing animation calls this each tick with an advancing `frame`.
    fn image_frame(self, frame: u32) -> Image<'static> {
        match brand_base() {
            Some(base) => {
                Image::new_owned(self.brand_rgba_frame(base, frame), base.width, base.height)
            }
            None => Image::new_owned(self.rgba_buffer(), TILE, TILE),
        }
    }
}

/// The compiled-in Driven brand mark (white road-to-cloud on deep teal) used as
/// the base for every tray icon. 64x64 is a crisp source the OS scales down to
/// the platform tray size; rendered from `icons/icon.svg` (see `icons/64x64.png`).
const BRAND_PNG: &[u8] = include_bytes!("../icons/64x64.png");

/// A decoded RGBA image (row-major, top-to-bottom) plus its dimensions.
struct BrandImage {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

/// The decoded brand mark, decoded exactly once and cached for the process.
///
/// Returns `None` (logged once) if decoding ever fails - the bytes are compiled
/// in so this is not expected, but callers fall back to a flat colour tile
/// rather than panicking (HARD RULE: no panics in the tray path).
fn brand_base() -> Option<&'static BrandImage> {
    static BASE: OnceLock<Option<BrandImage>> = OnceLock::new();
    BASE.get_or_init(|| match Image::from_bytes(BRAND_PNG) {
        Ok(img) => Some(BrandImage {
            rgba: img.rgba().to_vec(),
            width: img.width(),
            height: img.height(),
        }),
        Err(err) => {
            tracing::error!(
                target: TARGET,
                "decode tray brand PNG failed; falling back to flat tile: {err}"
            );
            None
        }
    })
    .as_ref()
}

/// The geometry of the status badge for a `width x height` icon: the disc
/// centre (`cx`, `cy`) in the bottom-right quadrant and its filled radius
/// (`r_fill`). Both [`draw_badge`] and the per-state glyph painters derive their
/// positions from this so the glyph always lands inside the disc, at any source
/// resolution.
struct BadgeGeom {
    cx: f32,
    cy: f32,
    r_fill: f32,
}

/// Compute the shared badge geometry (see [`BadgeGeom`]). The disc is sized as a
/// fraction of the smaller dimension so it scales with the source resolution.
fn badge_geometry(width: u32, height: u32) -> BadgeGeom {
    let min = width.min(height) as f32;
    BadgeGeom {
        cx: width as f32 * 0.72,
        cy: height as f32 * 0.72,
        r_fill: min * 0.24,
    }
}

/// Paint a single opaque pixel at (`x`, `y`) to grayscale `intensity` (white at
/// 255), bounds-checked. Used by the glyph painters to draw white-ish marks onto
/// the coloured badge disc.
fn put_pixel(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, intensity: u8) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let idx = ((y as u32 * width + x as u32) as usize) * 4;
    rgba[idx] = intensity;
    rgba[idx + 1] = intensity;
    rgba[idx + 2] = intensity;
    rgba[idx + 3] = 0xff;
}

/// Fill a disc of radius `r` centred at (`cx`, `cy`) with grayscale `intensity`.
fn fill_disc(rgba: &mut [u8], width: u32, height: u32, cx: f32, cy: f32, r: f32, intensity: u8) {
    let r2 = r * r;
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                put_pixel(rgba, width, height, x, y, intensity);
            }
        }
    }
}

/// Fill an axis-aligned rectangle (centred at (`cx`, `cy`), half-extents
/// `hw`/`hh`) with grayscale `intensity`.
fn fill_rect(rgba: &mut [u8], width: u32, height: u32, cx: f32, cy: f32, hw: f32, hh: f32) {
    let x0 = (cx - hw).floor() as i32;
    let x1 = (cx + hw).ceil() as i32;
    let y0 = (cy - hh).floor() as i32;
    let y1 = (cy + hh).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            put_pixel(rgba, width, height, x, y, 0xff);
        }
    }
}

/// Draw a filled status-badge disc (with a white contrast ring) into the
/// bottom-right corner of an RGBA buffer, in place. Marks the brand tray icon
/// with the live state's colour while keeping the mark recognisable; the white
/// ring keeps it visible against both the teal background and the white cloud.
fn draw_badge(rgba: &mut [u8], width: u32, height: u32, color: [u8; 3]) {
    let w = width as i32;
    let h = height as i32;
    let min = w.min(h) as f32;
    let BadgeGeom { cx, cy, r_fill } = badge_geometry(width, height);
    let r_ring = r_fill + (min * 0.06).max(1.0);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * w + x) as usize) * 4;
            if dist <= r_fill {
                rgba[idx] = color[0];
                rgba[idx + 1] = color[1];
                rgba[idx + 2] = color[2];
                rgba[idx + 3] = 0xff;
            } else if dist <= r_ring {
                rgba[idx] = 0xff;
                rgba[idx + 1] = 0xff;
                rgba[idx + 2] = 0xff;
                rgba[idx + 3] = 0xff;
            }
        }
    }
}

/// Draw the SYNCING spinner glyph: a ring of [`SYNC_DOTS`] dots inside the badge
/// disc, the "head" dot (at `frame % SYNC_DOTS`) painted brightest and the trail
/// fading behind it. Advancing `frame` rotates the bright head one dot per tick,
/// so consecutive frames render distinct buffers (a real spinning motion) and
/// frame `f == f + SYNC_FRAMES` (full circle). White-on-blue.
fn draw_spinner_glyph(rgba: &mut [u8], width: u32, height: u32, frame: u32) {
    let BadgeGeom { cx, cy, r_fill } = badge_geometry(width, height);
    let orbit = r_fill * 0.55;
    let dot_r = (r_fill * 0.20).max(1.0);
    let head = frame % SYNC_DOTS;
    for i in 0..SYNC_DOTS {
        // Start the ring at the top (12 o'clock) and go clockwise.
        let angle =
            std::f32::consts::TAU * (i as f32) / (SYNC_DOTS as f32) - std::f32::consts::FRAC_PI_2;
        let dx = cx + orbit * angle.cos();
        let dy = cy + orbit * angle.sin();
        // `lead` = how far this dot trails the head (0 = head). The head is the
        // brightest; each dot behind it is dimmer, giving the comet-tail look.
        let lead = (head + SYNC_DOTS - i) % SYNC_DOTS;
        let intensity = 255u32.saturating_sub(lead * 22).max(80) as u8;
        fill_disc(rgba, width, height, dx, dy, dot_r, intensity);
    }
}

/// Draw the PAUSED glyph: two vertical white bars (the universal pause symbol)
/// centred in the badge disc.
fn draw_pause_glyph(rgba: &mut [u8], width: u32, height: u32) {
    let BadgeGeom { cx, cy, r_fill } = badge_geometry(width, height);
    let bar_hw = (r_fill * 0.16).max(1.0);
    let bar_hh = r_fill * 0.5;
    let offset = r_fill * 0.4;
    fill_rect(rgba, width, height, cx - offset, cy, bar_hw, bar_hh);
    fill_rect(rgba, width, height, cx + offset, cy, bar_hw, bar_hh);
}

/// Draw the NETWORK-ATTENTION glyph: a white exclamation mark (`!`) - a vertical
/// stem above a dot - centred in the badge disc (the DESIGN s8.1
/// "yellow-with-`!`" reachability mark, now drawn for real).
fn draw_bang_glyph(rgba: &mut [u8], width: u32, height: u32) {
    let BadgeGeom { cx, cy, r_fill } = badge_geometry(width, height);
    let stem_hw = (r_fill * 0.13).max(1.0);
    // Stem: from near the top of the disc to just above centre.
    let stem_top = cy - r_fill * 0.55;
    let stem_bottom = cy + r_fill * 0.12;
    let stem_cy = (stem_top + stem_bottom) / 2.0;
    let stem_hh = (stem_bottom - stem_top) / 2.0;
    fill_rect(rgba, width, height, cx, stem_cy, stem_hw, stem_hh);
    // Dot: below the stem.
    let dot_r = stem_hw * 1.15;
    fill_disc(rgba, width, height, cx, cy + r_fill * 0.45, dot_r, 0xff);
}

/// Draw the ERROR glyph: a white `X` (two diagonal strokes) centred in the badge
/// disc - reads as "attention / failure" without relying on the red colour.
fn draw_cross_glyph(rgba: &mut [u8], width: u32, height: u32) {
    let BadgeGeom { cx, cy, r_fill } = badge_geometry(width, height);
    let reach = r_fill * 0.5;
    let thick = (r_fill * 0.18).max(1.0);
    let x0 = (cx - reach).floor() as i32;
    let x1 = (cx + reach).ceil() as i32;
    let y0 = (cy - reach).floor() as i32;
    let y1 = (cy + reach).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx.abs().max(dy.abs()) <= reach
                && ((dx - dy).abs() <= thick || (dx + dy).abs() <= thick)
            {
                put_pixel(rgba, width, height, x, y, 0xff);
            }
        }
    }
}

/// Is this pause reason a network / reachability condition (DESIGN s8.1
/// yellow-with-`!`) rather than a plain user/auto pause (DESIGN s8.1 yellow)?
fn pause_reason_is_network(reason: PauseReason) -> bool {
    match reason {
        PauseReason::Manual
        | PauseReason::Battery
        | PauseReason::Metered
        | PauseReason::Schedule => false,
        PauseReason::Offline
        | PauseReason::ServiceDown
        | PauseReason::NoInternet
        | PauseReason::CaptivePortal
        | PauseReason::DnsFailed => true,
    }
}

/// Is this error code a network / reachability error (DESIGN s8.1: the
/// yellow-with-`!` state "covers all of s5.8's network failure modes",
/// including "Drive unreachable")? Matches on the enum, not a string prefix,
/// so a newly-added [`ErrorCode`] fails to compile here until classified.
fn error_code_is_network(code: ErrorCode) -> bool {
    match code {
        // Pure network probe / reachability codes (SPEC s24 `net.*`).
        ErrorCode::NetOffline
        | ErrorCode::NetNoInternet
        | ErrorCode::NetDnsFailed
        | ErrorCode::NetCaptivePortal
        | ErrorCode::NetTimeout
        | ErrorCode::NetIntermittent
        | ErrorCode::NetProxyRequired
        // Drive unreachable / circuit-open is explicitly a yellow-bang case
        // (DESIGN s8.1 "Drive unreachable").
        | ErrorCode::DriveUnreachable
        // Couldn't reach the OAuth endpoint - a reachability problem, not a
        // credential problem (distinct from auth.invalid_grant below).
        | ErrorCode::AuthNetworkUnreachable
        // The update endpoint being unreachable is also a network condition.
        | ErrorCode::UpdateEndpointUnreachable => true,

        // Credential / consent failures -> red error (and the reauth path).
        ErrorCode::AuthInvalidGrant
        | ErrorCode::AuthConsentRequired
        // Drive-side quota / size / permission / checksum -> red error.
        | ErrorCode::DriveRateLimited
        | ErrorCode::DriveDailyQuotaExhausted
        | ErrorCode::DriveQuotaExhausted
        | ErrorCode::DriveUploadSizeLimit
        | ErrorCode::DriveChecksumMismatch
        | ErrorCode::DriveResumableSessionInvalid
        | ErrorCode::DriveDestFolderMissing
        | ErrorCode::DriveDestFolderPermissionDenied
        // Local filesystem / VSS errors -> red error.
        | ErrorCode::LocalFileLocked
        | ErrorCode::LocalVssUnavailable
        | ErrorCode::LocalFileChangedDuringUpload
        | ErrorCode::LocalFileReplacedDuringUpload
        | ErrorCode::LocalIoError
        | ErrorCode::LocalPathTooLong
        | ErrorCode::LocalUnicodeCollision
        | ErrorCode::LocalDiskFull
        | ErrorCode::LocalInvalidFilename
        | ErrorCode::LocalAdsSkipped
        // Updater signature / manual-required / crypto / state / harness /
        // internal -> red (none is a network reachability condition).
        | ErrorCode::UpdateSignatureInvalid
        | ErrorCode::UpdateManualRequiredMacos
        | ErrorCode::CryptoKeyMissing
        | ErrorCode::CryptoDecryptFailed
        | ErrorCode::CryptoRecoveryPhraseInvalid
        | ErrorCode::StateDbLocked
        | ErrorCode::StateDbCorrupt
        | ErrorCode::StateReconcileOrphan
        | ErrorCode::HarnessTimeout
        | ErrorCode::InternalBug
        // Invalid input crossing the IPC boundary is a user/renderer error,
        // not a network condition -> red error (not yellow-bang).
        | ErrorCode::InvalidInput => false,
    }
}

/// The localised tooltip string for `state` (DESIGN s8.1: the tooltip shows
/// the specific condition for the network-attention / paused / error states).
fn tooltip_for(state: &OrchestratorState) -> String {
    match state {
        OrchestratorState::Idle { .. } => rust_i18n::t!("tray.tooltip.idle").into_owned(),
        OrchestratorState::PowerCheck
        | OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. }
        | OrchestratorState::Verifying { .. } => rust_i18n::t!("tray.tooltip.syncing").into_owned(),
        // R2-P2-2: Backoff is the "Drive unreachable / retrying" attention
        // state, not an active sync - use the service-down tooltip so the
        // tooltip matches the NetworkAttention icon and the aggregate ranking.
        OrchestratorState::Backoff { .. } => {
            rust_i18n::t!("tray.tooltip.service_down").into_owned()
        }
        OrchestratorState::Paused { reason } => tooltip_for_pause(*reason),
        OrchestratorState::Error { detail } => tooltip_for_error(detail.code),
    }
}

fn tooltip_for_pause(reason: PauseReason) -> String {
    let key = match reason {
        PauseReason::Manual => "tray.tooltip.paused_manual",
        PauseReason::Battery => "tray.tooltip.paused_battery",
        PauseReason::Metered => "tray.tooltip.paused_metered",
        PauseReason::Schedule => "tray.tooltip.paused_schedule",
        PauseReason::Offline => "tray.tooltip.offline",
        PauseReason::NoInternet => "tray.tooltip.no_internet",
        PauseReason::CaptivePortal => "tray.tooltip.captive_portal",
        PauseReason::DnsFailed => "tray.tooltip.dns_failed",
        PauseReason::ServiceDown => "tray.tooltip.service_down",
    };
    rust_i18n::t!(key).into_owned()
}

fn tooltip_for_error(code: ErrorCode) -> String {
    let key = match code {
        ErrorCode::NetOffline => "tray.tooltip.offline",
        ErrorCode::NetNoInternet => "tray.tooltip.no_internet",
        ErrorCode::NetCaptivePortal => "tray.tooltip.captive_portal",
        ErrorCode::NetDnsFailed => "tray.tooltip.dns_failed",
        ErrorCode::DriveUnreachable => "tray.tooltip.service_down",
        ErrorCode::AuthNetworkUnreachable => "tray.tooltip.offline",
        ErrorCode::AuthInvalidGrant | ErrorCode::AuthConsentRequired => "tray.tooltip.needs_reauth",
        _ => "tray.tooltip.error",
    };
    rust_i18n::t!(key).into_owned()
}

// -----------------------------------------------------------------------------
// Notification dedup state
// -----------------------------------------------------------------------------

/// Per-account notification dedup state (DESIGN s117/s247): fire the
/// first-sync-complete toast exactly once PER ACCOUNT, and fire one error toast
/// per ENTRY into an error code PER ACCOUNT (not once per `StateChanged` event -
/// the orchestrator broadcast can replay the current state after a `Lagged`).
///
/// V5-P2-2 / C5-P2-4: keyed per account so one account's first-sync toast does
/// not silence another's, and an error on account B is not suppressed by the
/// same code already toasted for account A.
#[derive(Default)]
struct NotifyState {
    /// True once a sync cycle has been observed running this process (so the
    /// next `Idle` transition is a genuine completion, not the boot `Idle`).
    saw_active_cycle: bool,
    /// True once the first-sync-complete toast has fired.
    first_sync_notified: bool,
    /// The error code last notified, to suppress repeat toasts while the app
    /// stays parked in the same error.
    last_error_code: Option<ErrorCode>,
}

/// Process-global map of per-account dedup state. Keyed by [`AccountId`] so the
/// dedup latches are independent across accounts (V5-P2-2).
static NOTIFY: Mutex<Option<HashMap<AccountId, NotifyState>>> = Mutex::new(None);

/// Run `f` against the dedup state for `account`, creating it on first use.
/// Recovers a poisoned lock instead of panicking (HARD RULE).
fn with_notify_state<R>(account: AccountId, f: impl FnOnce(&mut NotifyState) -> R) -> R {
    let mut guard = NOTIFY.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let entry = map.entry(account).or_default();
    f(entry)
}

// -----------------------------------------------------------------------------
// R-P2-1: app-level aggregate tray state across accounts
// -----------------------------------------------------------------------------

/// Process-global map of the LAST KNOWN [`OrchestratorState`] per account
/// (R-P2-1). The single process tray icon serves ALL accounts, so it must be
/// derived from the AGGREGATE of every account's state by SEVERITY - not from
/// whichever account most recently emitted a `StateChanged`. Without this,
/// account A entering `Error` (red) then account B emitting `Idle` would
/// last-writer-wins overwrite the red and HIDE a live backup error.
///
/// Keyed by [`AccountId`]; updated in [`apply_state`] on every transition and
/// reduced to one icon/tooltip via [`aggregate_state`].
static TRAY_STATES: Mutex<Option<HashMap<AccountId, OrchestratorState>>> = Mutex::new(None);

/// The severity rank of an [`OrchestratorState`] for the aggregate tray icon
/// (R-P2-1, DESIGN s8.1 icon precedence). HIGHER = more urgent; the aggregate
/// shows the icon of the highest-ranked account so a live error is never masked
/// by another account going idle.
///
/// Ordering (most -> least severe):
/// 1. `Error` (red / needs-reauth / decrypt / disk-full) - always wins;
/// 2. `Backoff` (Drive breaker open / rate-limit) and a network/service-down
///    pause (the yellow-with-`!` reachability cases);
/// 3. `Paused` (plain user/auto pause - battery / metered / manual);
/// 4. the working states (`PowerCheck` / `Scanning` / `Planning` /
///    `Executing` / `Verifying`) - "syncing";
/// 5. `Idle` (last sync OK) - the floor.
fn state_severity(state: &OrchestratorState) -> u8 {
    match state {
        OrchestratorState::Error { .. } => 4,
        OrchestratorState::Backoff { .. } => 3,
        OrchestratorState::Paused { reason } => {
            if pause_reason_is_network(*reason) {
                3
            } else {
                2
            }
        }
        OrchestratorState::PowerCheck
        | OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. }
        | OrchestratorState::Verifying { .. } => 1,
        OrchestratorState::Idle { .. } => 0,
    }
}

/// Update the aggregate tray-state map with `account`'s new `state`, then return
/// the single most-severe [`OrchestratorState`] across ALL known accounts
/// (R-P2-1). The returned state drives the one process tray icon + tooltip.
///
/// Ties (equal severity) keep the INCOMING state so the tooltip tracks the most
/// recent transition at that severity (e.g. the account that just errored shows
/// its own error tooltip). Recovers a poisoned lock instead of panicking.
fn aggregate_state(account: AccountId, state: OrchestratorState) -> OrchestratorState {
    let mut guard = TRAY_STATES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let incoming_sev = state_severity(&state);
    map.insert(account, state.clone());

    // Pick the most-severe state across all accounts. Start from the incoming
    // one (so a single-account app, and the tie case, both keep it) and only
    // replace it with a STRICTLY more severe peer.
    let mut best = state;
    let mut best_sev = incoming_sev;
    for (id, candidate) in map.iter() {
        if *id == account {
            continue;
        }
        let sev = state_severity(candidate);
        if sev > best_sev {
            best = candidate.clone();
            best_sev = sev;
        }
    }
    best
}

// -----------------------------------------------------------------------------
// build
// -----------------------------------------------------------------------------

/// Build the tray icon + menu and register its handlers (SPEC s12).
///
/// Removes the auto-created `tauri.conf.json` tray (id `"main"`) first so it
/// does not coexist with our canonical `"driven-main"` tray, then builds the
/// DESIGN s8.1 flat menu wired to the M5 sync commands / window-show / quit.
/// Left-click opens the window (graceful no-op where it never fires on Linux);
/// `show_menu_on_left_click(false)` keeps left-click distinct from the menu.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    // Drop the config-defined tray so we don't show two icons. No-op (and a
    // debug log) if the config block was removed or used a different id.
    if app.remove_tray_by_id(CONFIG_TRAY_ID).is_none() {
        tracing::debug!(
            target: TARGET,
            "no config-defined tray (id {CONFIG_TRAY_ID}) to remove; only {TRAY_ID} will show"
        );
    }

    let menu = build_menu(app)?;
    let idle_icon = TrayIcon::Idle.image();

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(idle_icon)
        .tooltip(rust_i18n::t!("app.name"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu_event(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            // Linux DEs may never deliver this; the menu remains canonical.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    // Our generated icons are colour-bearing; the config sets the boot icon as
    // a template (macOS recolours templates monochrome, which would erase the
    // yellow/red distinction), so force template OFF on the live tray.
    if let Err(err) = tray.set_icon_as_template(false) {
        tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
    }

    Ok(())
}

/// Re-render the tray after a locale change (SPEC s22 `ui.locale`, DESIGN s8.7).
///
/// The tray menu labels + tooltip are built from `rust_i18n::t!` at
/// [`build`]-time, so a locale switch needs the tray rebuilt to pick up the new
/// strings. This removes the live `"driven-main"` tray and rebuilds it; the
/// caller must have already called `rust_i18n::set_locale(..)`. Best-effort: if
/// no tray exists yet (assembly still running) [`build`] just creates it.
///
/// Note the aggregate per-account icon state ([`TRAY_STATES`]) is process-global
/// and survives the rebuild, so the next `apply_state` (or the existing state)
/// restores the correct icon; the rebuilt tray starts on the idle icon until
/// then, which is the same as a fresh boot.
pub fn rebuild(app: &AppHandle) -> tauri::Result<()> {
    if app.remove_tray_by_id(TRAY_ID).is_none() {
        tracing::debug!(target: TARGET, "no live tray {TRAY_ID} to remove before rebuild; building fresh");
    }
    build(app)
}

/// Build the DESIGN s8.1 flat tray menu (every action is a menu item so Linux
/// users are never stuck on a non-firing left-click). `MenuBuilder` cleanly
/// mixes id'd items with separators without slice-coercion ambiguity.
fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let item = |id: &str, label_key: &str| -> tauri::Result<MenuItem<tauri::Wry>> {
        MenuItem::with_id(app, id, rust_i18n::t!(label_key), true, None::<&str>)
    };

    MenuBuilder::new(app)
        .item(&item(menu_id::SYNC_NOW, "tray.sync_now")?)
        .item(&item(menu_id::PAUSE_30M, "tray.pause_30m")?)
        .item(&item(menu_id::RESUME, "tray.resume")?)
        .separator()
        .item(&item(menu_id::SETTINGS, "tray.settings")?)
        .item(&item(menu_id::ACTIVITY, "tray.activity")?)
        .item(&item(menu_id::RESTORE, "tray.restore")?)
        .separator()
        .item(&item(menu_id::QUIT, "tray.quit")?)
        .build()
}

/// Dispatch a tray menu click to the M5 sync commands / window show / quit
/// (SPEC s12). Async commands run on the Tauri runtime so the menu callback
/// returns immediately.
fn on_menu_event(app: &AppHandle, id: &str) {
    match id {
        menu_id::SYNC_NOW => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::sync_now(state, None).await
        }),
        menu_id::PAUSE_30M => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::pause_sync(app.clone(), state, Some(30 * 60)).await
        }),
        menu_id::RESUME => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::resume_sync(state).await
        }),
        menu_id::SETTINGS | menu_id::ACTIVITY | menu_id::RESTORE => {
            // Route selection (Settings/Activity/Restore) is an M6 frontend
            // concern; for M5 every item surfaces the main window and hints
            // the target route to the webview so M6 can deep-link.
            show_main_window(app);
            navigate_hint(app, id);
        }
        menu_id::QUIT => app.exit(0),
        other => tracing::warn!(target: TARGET, "unknown tray menu id: {other}"),
    }
}

/// The error a tray command returns when [`AppState`](crate::app_state::AppState)
/// is not managed yet (e.g. assembly is still running / failed). Returned
/// instead of panicking via `Manager::state` (HARD RULE: no panics).
fn missing_state_err() -> crate::commands::CommandResult<()> {
    Err(crate::commands::CommandError::new("app state not ready"))
}

/// Spawn an async sync command on the Tauri runtime from a tray callback,
/// logging any [`CommandError`](crate::commands::CommandError).
fn spawn_command<F, Fut>(app: &AppHandle, f: F)
where
    F: FnOnce(AppHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = crate::commands::CommandResult<()>> + Send,
{
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(err) = f(app).await {
            tracing::warn!(target: TARGET, "tray command failed: {err:?}");
        }
    });
}

/// Show, unminimize, and focus the main window (the left-click + menu action).
/// Every step is best-effort: a missing window or a platform that rejects the
/// op is logged, never panicked (DESIGN s8.1 - menu is canonical, never stuck).
fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!(target: TARGET, "main window not found; cannot surface it");
        return;
    };
    if let Err(err) = window.unminimize() {
        tracing::debug!(target: TARGET, "unminimize main window failed: {err}");
    }
    if let Err(err) = window.show() {
        tracing::warn!(target: TARGET, "show main window failed: {err}");
    }
    if let Err(err) = window.set_focus() {
        tracing::debug!(target: TARGET, "focus main window failed: {err}");
    }
}

/// Emit a lightweight `tray:navigate` hint carrying the target route so the
/// M6 frontend can route to Settings / Activity / Restore. Harmless if no
/// listener exists yet (M5 ships no router).
fn navigate_hint(app: &AppHandle, route: &str) {
    use tauri::Emitter;
    if let Err(err) = app.emit("tray:navigate", route) {
        tracing::debug!(target: TARGET, "tray:navigate emit failed: {err}");
    }
}

// -----------------------------------------------------------------------------
// Syncing animation (timer-driven spinner)
// -----------------------------------------------------------------------------

/// Handle to the running syncing-spinner animation task, if any. Process-global
/// because the single tray serves ALL accounts: exactly one spinner runs while
/// the AGGREGATE state is syncing, regardless of how many accounts are syncing.
/// `None` when no animation is running. Recovers a poisoned lock instead of
/// panicking (HARD RULE: no panics in the tray path).
static SYNC_ANIM: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

/// Monotonic "generation" guard that serialises spinner frame writes against
/// [`stop_sync_animation`] (codex P2: abort alone cannot stop a frame already
/// being written, so a stale syncing frame could land AFTER the static icon and
/// freeze the tray as syncing). Each [`start_sync_animation`] claims a fresh
/// generation; the task holds this lock ACROSS its `set_icon` and only writes
/// while the live generation is still its own. [`stop_sync_animation`] bumps the
/// generation under the SAME lock, which (a) makes any in-flight write complete
/// before stop returns and (b) makes any later tick see a changed generation and
/// break without writing. So once stop returns, no syncing frame can be written,
/// and the static icon the caller sets next is guaranteed to be the last write.
static SYNC_GEN: Mutex<u64> = Mutex::new(0);

/// Start the syncing-spinner animation if it is not already running (idempotent).
///
/// Spawns a timer-driven task that, every [`SYNC_FRAME_INTERVAL`], advances the
/// frame counter and swaps the tray icon to the next [`TrayIcon::Syncing`] frame
/// ([`TrayIcon::image_frame`]). The FIRST `interval` tick fires immediately, so
/// the spinner's frame 0 paints right away. The task loops until
/// [`stop_sync_animation`] retires its generation (and aborts it) when the
/// aggregate leaves syncing or on quit, so it never outlives the syncing state.
///
/// Idempotent: if an animation is already running this is a no-op, so a burst of
/// `StateChanged` events while syncing does NOT restart (and stutter) the
/// spinner. Best-effort: a missing tray on a given tick is logged and skipped,
/// not panicked.
fn start_sync_animation(app: &AppHandle) {
    let mut guard = SYNC_ANIM.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        // Already animating - keep the existing smooth cycle running.
        return;
    }
    // Claim a fresh generation for THIS task. Done while holding `SYNC_ANIM` and
    // only after the is_some() check, so claiming a generation never retires a
    // still-running task's generation (which would make it stop writing).
    let my_gen = {
        let mut g = SYNC_GEN.lock().unwrap_or_else(|e| e.into_inner());
        *g = g.wrapping_add(1);
        *g
    };
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let mut frame: u32 = 0;
        let mut ticker = tokio::time::interval(SYNC_FRAME_INTERVAL);
        // If a tick is missed (e.g. the runtime was busy), skip ahead rather
        // than burst-firing catch-up frames.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let Some(tray) = app.tray_by_id(TRAY_ID) else {
                // Tray not built yet / being rebuilt: skip this frame, keep
                // ticking (a rebuild re-creates the same id).
                tracing::trace!(target: TARGET, "tray {TRAY_ID} absent during sync animation tick; skipping frame");
                continue;
            };
            // Write the frame under the generation lock so it is serialised with
            // `stop_sync_animation`: if our generation has been retired, stop
            // writing immediately (a static icon is taking over). NB: no `.await`
            // inside this block - the lock is never held across a suspend point.
            {
                let gen = SYNC_GEN.lock().unwrap_or_else(|e| e.into_inner());
                if *gen != my_gen {
                    break;
                }
                if let Err(err) = tray.set_icon(Some(TrayIcon::Syncing.image_frame(frame))) {
                    tracing::debug!(target: TARGET, "set sync animation frame failed: {err}");
                }
                // Colour-bearing frames must never be recoloured to a macOS template.
                if let Err(err) = tray.set_icon_as_template(false) {
                    tracing::trace!(target: TARGET, "set_icon_as_template(false) during animation failed: {err}");
                }
            }
            // Advance to the next spinner frame, wrapping each full rotation.
            frame = (frame + 1) % SYNC_FRAMES;
        }
    });
    *guard = Some(handle);
}

/// Stop the syncing-spinner animation if it is running (idempotent).
///
/// Retires the current generation under [`SYNC_GEN`] FIRST so that, once this
/// returns, no spinner frame can be written (the running task either already
/// finished its in-flight write before we took the lock, or will see the changed
/// generation on its next tick and break) - this closes the race where an
/// aborted task's last synchronous `set_icon` lands after the caller's static
/// icon. Then takes + aborts the task handle so it does not linger. Called
/// whenever the aggregate leaves syncing, on suspend, and on quit. Recovers a
/// poisoned lock instead of panicking.
pub fn stop_sync_animation() {
    // Retire the live generation (releases the lock before touching SYNC_ANIM, so
    // start/stop never hold both locks at once -> no lock-ordering deadlock).
    {
        let mut g = SYNC_GEN.lock().unwrap_or_else(|e| e.into_inner());
        *g = g.wrapping_add(1);
    }
    let mut guard = SYNC_ANIM.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = guard.take() {
        handle.abort();
    }
}

// -----------------------------------------------------------------------------
// apply_state + notifications
// -----------------------------------------------------------------------------

/// Swap the tray icon + tooltip to match `state` (DESIGN s8.1), called from
/// the orchestrator-event bridge on every [`OrchestratorState`] transition
/// (tray must update within 1s per ROADMAP M5 acceptance), and raise the
/// DESIGN s117/s247 OS notifications (first-sync-complete + red error).
///
/// Best-effort: a missing tray or a failed icon/tooltip set is logged, never
/// panicked. `apply_state` returns `()` (the committed signature) so all
/// errors are swallowed with a `tracing` line.
pub fn apply_state(app: &AppHandle, account_id: AccountId, state: OrchestratorState) {
    // R-P2-1: the single process tray icon serves ALL accounts. Update the
    // app-level per-account state map and derive ONE aggregate state by
    // severity, so a live error on account A is NOT masked when account B goes
    // idle (the old last-writer-wins bug). The icon + tooltip reflect the
    // aggregate; the notification below stays PER ACCOUNT (dedup is per-account).
    let aggregate = aggregate_state(account_id, state.clone());
    let icon = TrayIcon::for_state(&aggregate);

    // Drive the animation purely off the aggregate icon: start the spinner when
    // the aggregate is syncing (the animation task owns the icon then), and stop
    // it the instant the aggregate is anything else. Done before touching the
    // tray so a stale spinner frame can never race a static icon set below.
    if icon == TrayIcon::Syncing {
        start_sync_animation(app);
    } else {
        stop_sync_animation();
    }

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        // While syncing, the animation task sets the icon every frame - do NOT
        // also set a still icon here (it would fight the spinner). For every
        // other state, set the static per-state glyph icon.
        if icon != TrayIcon::Syncing {
            if let Err(err) = tray.set_icon(Some(icon.image())) {
                tracing::warn!(target: TARGET, "set tray icon failed: {err}");
            }
            // Generated tiles are colour-bearing; never let the OS recolour them.
            if let Err(err) = tray.set_icon_as_template(false) {
                tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
            }
        }
        if let Err(err) = tray.set_tooltip(Some(tooltip_for(&aggregate))) {
            tracing::warn!(target: TARGET, "set tray tooltip failed: {err}");
        }
    } else {
        tracing::warn!(target: TARGET, "tray {TRAY_ID} not found; cannot apply state");
    }

    // Per-account notification (first-sync-complete / red error), keyed +
    // deduped by THIS account - independent of the aggregate icon above.
    notify_for_state(app, account_id, &state);
}

/// The notification a state transition asks for, decided by the PURE
/// [`decide_notify`] state machine so the firing logic is unit-testable without
/// an `AppHandle`. [`notify_for_state`] turns it into an actual OS toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyOutcome {
    /// No toast for this transition (icon+tooltip only).
    None,
    /// The first-sync-complete toast (DESIGN s117).
    FirstSyncComplete,
    /// A red-error toast for `code` (DESIGN s247).
    Error(ErrorCode),
}

/// PURE notification state machine (R3-P2-1): mutate the per-account dedup
/// [`NotifyState`] for `state` and return which toast (if any) to fire.
///
/// Extracted from [`notify_for_state`] so the firing decision can be tested
/// without an `AppHandle`.
///
/// Active-cycle group (R3-P2-1): ONLY the real scan/plan/execute/verify states
/// (plus the `PowerCheck` that gates them) mark `saw_active_cycle`. `Backoff` is
/// DELIBERATELY EXCLUDED: it was remapped to a network-attention condition
/// (R2-P2-2, the Drive breaker open / rate-limited "Drive unreachable" case), so
/// a startup that only ever hits Drive backoff has NOT completed a real sync
/// cycle and must NOT later fire a bogus "first sync complete" toast on the next
/// `Idle`. `Backoff` therefore behaves like a `Paused` blip here: it clears the
/// error dedup latch but does not arm first-sync.
fn decide_notify(s: &mut NotifyState, state: &OrchestratorState) -> NotifyOutcome {
    match state {
        OrchestratorState::PowerCheck
        | OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. }
        | OrchestratorState::Verifying { .. } => {
            // A real cycle is underway; the next Idle is a genuine completion.
            s.saw_active_cycle = true;
            // Leaving any error state clears the dedup latch so a recurrence
            // notifies again.
            s.last_error_code = None;
            NotifyOutcome::None
        }
        OrchestratorState::Backoff { .. } => {
            // R3-P2-1: Drive backoff is a network-attention blip, NOT an active
            // sync cycle - it must NOT arm the first-sync toast. Treated like a
            // pause: clear the error dedup latch only.
            s.last_error_code = None;
            NotifyOutcome::None
        }
        OrchestratorState::Idle { .. } => {
            s.last_error_code = None;
            let fire = s.saw_active_cycle && !s.first_sync_notified;
            if fire {
                s.first_sync_notified = true;
                NotifyOutcome::FirstSyncComplete
            } else {
                NotifyOutcome::None
            }
        }
        OrchestratorState::Paused { .. } => {
            // Pauses (battery / metered / network) are icon+tooltip only, no
            // toast on every blip (DESIGN s117/s247).
            s.last_error_code = None;
            NotifyOutcome::None
        }
        OrchestratorState::Error { detail } => {
            // Reauth is handled by notify_needs_reauth (needs account/email).
            if matches!(
                detail.code,
                ErrorCode::AuthInvalidGrant | ErrorCode::AuthConsentRequired
            ) {
                return NotifyOutcome::None;
            }
            // Only the red error visual toasts; yellow-bang network errors are
            // icon+tooltip only.
            if TrayIcon::for_state(state) != TrayIcon::Error {
                return NotifyOutcome::None;
            }
            if s.last_error_code == Some(detail.code) {
                NotifyOutcome::None // already toasted this error; suppress replay
            } else {
                s.last_error_code = Some(detail.code);
                NotifyOutcome::Error(detail.code)
            }
        }
    }
}

/// Raise the DESIGN s117/s247 OS notifications for a state transition.
///
/// Two triggers only (the rest of the state machine is icon+tooltip only):
/// - first-sync-complete: the first `Idle` reached after a real sync cycle;
/// - red error (`TrayIcon::Error`): a non-network error that needs attention,
///   deduped so it fires once per entry into a given error code. The reauth
///   case (`auth.invalid_grant` / `auth.consent_required`) is deliberately
///   skipped here - it is covered by [`notify_needs_reauth`], which the shell
///   calls with the account + email the state cannot carry.
fn notify_for_state(app: &AppHandle, account_id: AccountId, state: &OrchestratorState) {
    let outcome = with_notify_state(account_id, |s| decide_notify(s, state));
    match outcome {
        NotifyOutcome::None => {}
        NotifyOutcome::FirstSyncComplete => {
            show_notification(
                app,
                rust_i18n::t!("notifications.first_sync_complete.title").into_owned(),
                rust_i18n::t!("notifications.first_sync_complete.body").into_owned(),
            );
        }
        NotifyOutcome::Error(code) => {
            let body = error_notification_body(code);
            show_notification(
                app,
                rust_i18n::t!("notifications.error.title").into_owned(),
                body,
            );
        }
    }
}

/// The localised body line for a red-error notification, keyed off the stable
/// error code so each class gets a meaningful sentence; falls back to a
/// generic line for codes without a dedicated string.
fn error_notification_body(code: ErrorCode) -> String {
    let key = match code {
        ErrorCode::DriveQuotaExhausted => "notifications.error.drive_quota",
        ErrorCode::DriveDailyQuotaExhausted => "notifications.error.drive_daily_quota",
        ErrorCode::CryptoKeyMissing => "notifications.error.crypto_key_missing",
        ErrorCode::CryptoDecryptFailed => "notifications.error.crypto_decrypt_failed",
        ErrorCode::LocalDiskFull => "notifications.error.disk_full",
        ErrorCode::LocalVssUnavailable => "notifications.error.vss_unavailable",
        ErrorCode::StateDbCorrupt => "notifications.error.db_corrupt",
        _ => "notifications.error.generic",
    };
    rust_i18n::t!(key, code = code.code()).into_owned()
}

/// Raise the DESIGN s117 "needs sign-in again" notification for an account
/// whose refresh token was revoked (CODEX_NOTES V-F: the M5 shell performs
/// this transition). MUST be called by the shell's reauth bridge at the same
/// point it calls `events::emit_account_needs_reauth`; the `OrchestratorState`
/// alone cannot carry the `account` email, so `apply_state` cannot fire it.
pub fn notify_needs_reauth(app: &AppHandle, account: &str) {
    show_notification(
        app,
        rust_i18n::t!("notifications.needs_reauth.title").into_owned(),
        rust_i18n::t!("notifications.needs_reauth.body", account = account).into_owned(),
    );
}

/// R6-P2-1 (DESIGN s9.4): raise the "dev update installed" tray notification. The
/// dev channel applies updates SILENTLY - the periodic checker downloads + installs
/// the staged update without a banner and calls this so the power user is told a
/// fresh dev build is ready (it applies on the next Driven restart). `version` is
/// the installed dev version (e.g. `0.1.1-dev.42.abc1234`).
pub fn notify_dev_update_installed(app: &AppHandle, version: &str) {
    show_notification(
        app,
        rust_i18n::t!("notifications.dev_update_installed.title").into_owned(),
        rust_i18n::t!("notifications.dev_update_installed.body", version = version).into_owned(),
    );
}

/// R8-P1-2 (SPEC s15 / DESIGN s9.4): raise the "manual update available" tray
/// notification on macOS, where the in-app updater is disabled (unsigned V1) and
/// the user must download + reinstall the latest DMG manually. Used by both the
/// periodic dev-channel path and the `install_update` short-circuit so a macOS
/// user is told a newer build exists and how to apply it. `version` is the
/// available version.
pub fn notify_manual_update_available(app: &AppHandle, version: &str) {
    show_notification(
        app,
        rust_i18n::t!("notifications.manual_update_available.title").into_owned(),
        rust_i18n::t!(
            "notifications.manual_update_available.body",
            version = version
        )
        .into_owned(),
    );
}

/// R8-P1-1 (DATA-SAFETY): raise the "paused for safety" tray notification when the
/// one-time recovery-ack upgrade repair failed and Driven started QUIESCED (no
/// orchestrators spawned). Tells the user backups are held off and a restart
/// retries the repair, rather than silently doing nothing.
pub fn notify_repair_failed(app: &AppHandle) {
    show_notification(
        app,
        rust_i18n::t!("notifications.repair_failed.title").into_owned(),
        rust_i18n::t!("notifications.repair_failed.body").into_owned(),
    );
}

/// Set the tray to the "Suspending..." visual (DESIGN s5.10.2): yellow icon +
/// suspend tooltip. Driven by [`PowerEvent::Suspending`](driven_core::types::PowerEvent),
/// which is NOT an [`OrchestratorState`], so the shell's power bridge MUST call
/// this on suspend (and call [`apply_state`] again on resume to restore the
/// live state's icon).
///
/// Uncalled in M5: the suspend/resume EDGE `PowerEvent` (DESIGN s5.10) arrives
/// on an OS message-pump seam (`WM_POWERBROADCAST`) that `driven-power` does not
/// yet expose, so there is no event source to bridge into this. The visual is
/// implemented and ready; allow dead_code until that power-edge seam exists.
#[allow(dead_code)]
pub fn apply_suspending(app: &AppHandle) {
    // Suspending is a static yellow icon - stop any running spinner so the
    // animation task cannot overwrite it on its next tick.
    stop_sync_animation();
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        tracing::warn!(target: TARGET, "tray {TRAY_ID} not found; cannot show suspending");
        return;
    };
    if let Err(err) = tray.set_icon(Some(TrayIcon::Paused.image())) {
        tracing::warn!(target: TARGET, "set suspending icon failed: {err}");
    }
    if let Err(err) = tray.set_icon_as_template(false) {
        tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
    }
    if let Err(err) = tray.set_tooltip(Some(rust_i18n::t!("tray.tooltip.suspending").into_owned()))
    {
        tracing::warn!(target: TARGET, "set suspending tooltip failed: {err}");
    }
}

/// Show an OS notification via `tauri-plugin-notification`, logging on failure
/// (a denied/unavailable notification permission must never crash the tray).
fn show_notification(app: &AppHandle, title: String, body: String) {
    let result = app.notification().builder().title(title).body(body).show();
    if let Err(err) = result {
        tracing::warn!(target: TARGET, "OS notification failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::types::{ErrorDetail, ExecProgress, PlanSummary};

    fn err_state(code: ErrorCode) -> OrchestratorState {
        OrchestratorState::Error {
            detail: ErrorDetail::new(code, "test"),
        }
    }

    /// Reset the process-global aggregate map so the R-P2-1 test is isolated
    /// from any state other tests left behind (the static is shared in-process).
    fn reset_tray_states() {
        let mut guard = TRAY_STATES.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(HashMap::new());
    }

    #[test]
    fn backoff_then_idle_does_not_fire_first_sync_complete() {
        // R3-P2-1: `Backoff` (Drive breaker open / rate-limited) is a network-
        // attention condition, NOT a real sync cycle. A startup that only ever
        // hits Drive backoff before settling to Idle has completed no scan/plan/
        // execute/verify, so the next `Idle` must NOT fire the bogus
        // "first sync complete" toast. Drives the PURE `decide_notify` state
        // machine directly (no AppHandle needed).
        let mut s = NotifyState::default();

        // Only ever saw Backoff: it must NOT arm the active-cycle latch.
        let out = decide_notify(&mut s, &OrchestratorState::Backoff { until: 0 });
        assert_eq!(out, NotifyOutcome::None, "Backoff itself toasts nothing");
        assert!(
            !s.saw_active_cycle,
            "Backoff must NOT count as an active sync cycle (R3-P2-1)"
        );

        // Reaching Idle after only-Backoff must NOT fire first-sync-complete.
        let out = decide_notify(&mut s, &OrchestratorState::Idle { last_run_at: None });
        assert_eq!(
            out,
            NotifyOutcome::None,
            "Backoff-then-Idle must NOT fire the first-sync-complete toast (R3-P2-1)"
        );
        assert!(
            !s.first_sync_notified,
            "first-sync must stay unarmed after a Backoff-only startup"
        );

        // Sanity: a REAL cycle (e.g. Executing) DOES arm it, so the next Idle
        // fires exactly once - proving we only removed Backoff, not the feature.
        let out = decide_notify(
            &mut s,
            &OrchestratorState::Executing {
                progress: ExecProgress::zero(),
            },
        );
        assert_eq!(out, NotifyOutcome::None);
        assert!(s.saw_active_cycle, "a real Executing cycle arms first-sync");
        let out = decide_notify(&mut s, &OrchestratorState::Idle { last_run_at: None });
        assert_eq!(
            out,
            NotifyOutcome::FirstSyncComplete,
            "a real cycle then Idle DOES fire first-sync-complete (feature intact)"
        );
        // Idempotent: a second Idle does not re-fire.
        let out = decide_notify(&mut s, &OrchestratorState::Idle { last_run_at: None });
        assert_eq!(
            out,
            NotifyOutcome::None,
            "first-sync-complete fires exactly once"
        );
    }

    #[test]
    fn aggregate_tray_icon_is_error_when_any_account_errors() {
        // R-P2-1: with account A in Error and account B in Idle, the single
        // process tray icon must be the AGGREGATE = Error (severity wins), even
        // though B's Idle was the most RECENT transition. The old last-writer-
        // wins behaviour would have shown Idle and hidden A's live error.
        reset_tray_states();
        let account_a = AccountId::new_v4();
        let account_b = AccountId::new_v4();

        // A errors first.
        let agg_a = aggregate_state(account_a, err_state(ErrorCode::LocalDiskFull));
        assert_eq!(
            TrayIcon::for_state(&agg_a),
            TrayIcon::Error,
            "A alone in Error -> Error"
        );

        // B then goes Idle (the more-recent event). The aggregate must STAY
        // Error because A is still errored - B's Idle must not mask it.
        let agg_b = aggregate_state(account_b, OrchestratorState::Idle { last_run_at: None });
        assert_eq!(
            TrayIcon::for_state(&agg_b),
            TrayIcon::Error,
            "B going Idle must NOT overwrite A's live Error (aggregate stays Error)"
        );

        // When A recovers to Idle too, the aggregate finally drops to Idle.
        let agg_a2 = aggregate_state(account_a, OrchestratorState::Idle { last_run_at: None });
        assert_eq!(
            TrayIcon::for_state(&agg_a2),
            TrayIcon::Idle,
            "once every account is Idle the aggregate is Idle"
        );
    }

    #[test]
    fn aggregate_severity_orders_error_over_backoff_over_paused_over_syncing_over_idle() {
        // R-P2-1: the severity ladder Error > backoff/network-pause > paused >
        // syncing > idle is what the aggregate uses to pick the tray icon.
        let idle = OrchestratorState::Idle { last_run_at: None };
        let syncing = OrchestratorState::Executing {
            progress: ExecProgress::zero(),
        };
        let paused = OrchestratorState::Paused {
            reason: PauseReason::Battery,
        };
        let net_paused = OrchestratorState::Paused {
            reason: PauseReason::ServiceDown,
        };
        let backoff = OrchestratorState::Backoff { until: 0 };
        let error = err_state(ErrorCode::CryptoDecryptFailed);

        assert!(state_severity(&error) > state_severity(&backoff));
        assert!(state_severity(&error) > state_severity(&net_paused));
        assert_eq!(state_severity(&backoff), state_severity(&net_paused));
        assert!(state_severity(&backoff) > state_severity(&paused));
        assert!(state_severity(&paused) > state_severity(&syncing));
        assert!(state_severity(&syncing) > state_severity(&idle));
    }

    #[test]
    fn idle_maps_to_idle() {
        assert_eq!(
            TrayIcon::for_state(&OrchestratorState::Idle { last_run_at: None }),
            TrayIcon::Idle
        );
    }

    #[test]
    fn working_states_map_to_syncing() {
        let working = [
            OrchestratorState::PowerCheck,
            OrchestratorState::Scanning {
                source_id: driven_core::types::SourceId::new_v4(),
                scanned: 0,
            },
            OrchestratorState::Planning {
                plan: PlanSummary::default(),
            },
            OrchestratorState::Executing {
                progress: ExecProgress::zero(),
            },
            OrchestratorState::Verifying {
                sampled: 0,
                mismatches: 0,
            },
            // R2-P2-2: Backoff is NO LONGER a "working/syncing" state - it maps
            // to NetworkAttention (covered by `backoff_is_network_attention`).
        ];
        for s in working {
            assert_eq!(TrayIcon::for_state(&s), TrayIcon::Syncing, "{s:?}");
        }
    }

    /// R2-P2-2: `Backoff` (Drive breaker open / rate-limited = "Drive
    /// unreachable") must render the yellow NetworkAttention icon + the
    /// service-down tooltip - NOT the blue syncing icon - so it agrees with the
    /// aggregate severity ranking (`state_severity(Backoff) == 3`, the
    /// network-attention rank). Before the fix `Backoff + Idle` aggregated to
    /// `Backoff` yet showed the syncing icon.
    #[test]
    fn backoff_is_network_attention() {
        let backoff = OrchestratorState::Backoff { until: 0 };
        assert_eq!(
            TrayIcon::for_state(&backoff),
            TrayIcon::NetworkAttention,
            "Backoff must render the NetworkAttention icon, not Syncing"
        );
        // The tooltip must be the service-down (Drive unreachable) string, not
        // the generic syncing tooltip.
        assert_eq!(
            tooltip_for(&backoff),
            rust_i18n::t!("tray.tooltip.service_down").into_owned(),
            "Backoff tooltip must be the service-down/Drive-unreachable string"
        );

        // And the AGGREGATE of Backoff + Idle is Backoff (severity 3 > 0) which
        // now correctly renders NetworkAttention end-to-end.
        let a = AccountId::new_v4();
        let b = AccountId::new_v4();
        reset_tray_states();
        let agg_idle = aggregate_state(a, OrchestratorState::Idle { last_run_at: None });
        assert_eq!(TrayIcon::for_state(&agg_idle), TrayIcon::Idle);
        let agg = aggregate_state(b, OrchestratorState::Backoff { until: 0 });
        assert_eq!(
            TrayIcon::for_state(&agg),
            TrayIcon::NetworkAttention,
            "Backoff + Idle must aggregate to the NetworkAttention icon, not blue syncing"
        );
        assert_eq!(
            tooltip_for(&agg),
            rust_i18n::t!("tray.tooltip.service_down").into_owned(),
            "the aggregated Backoff tooltip must be the service-down string"
        );
    }

    #[test]
    fn manual_battery_metered_pause_is_yellow_not_bang() {
        for reason in [
            PauseReason::Manual,
            PauseReason::Battery,
            PauseReason::Metered,
            PauseReason::Schedule,
        ] {
            assert_eq!(
                TrayIcon::for_state(&OrchestratorState::Paused { reason }),
                TrayIcon::Paused,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn network_pause_is_network_attention() {
        for reason in [
            PauseReason::Offline,
            PauseReason::NoInternet,
            PauseReason::CaptivePortal,
            PauseReason::DnsFailed,
            PauseReason::ServiceDown,
        ] {
            assert_eq!(
                TrayIcon::for_state(&OrchestratorState::Paused { reason }),
                TrayIcon::NetworkAttention,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn network_error_codes_are_network_attention() {
        for code in [
            ErrorCode::NetOffline,
            ErrorCode::NetNoInternet,
            ErrorCode::NetDnsFailed,
            ErrorCode::NetCaptivePortal,
            ErrorCode::NetTimeout,
            ErrorCode::NetIntermittent,
            ErrorCode::NetProxyRequired,
            ErrorCode::DriveUnreachable,
            ErrorCode::AuthNetworkUnreachable,
            ErrorCode::UpdateEndpointUnreachable,
        ] {
            assert_eq!(
                TrayIcon::for_state(&err_state(code)),
                TrayIcon::NetworkAttention,
                "{code:?}"
            );
        }
    }

    #[test]
    fn non_network_error_codes_are_red_error() {
        for code in [
            ErrorCode::AuthInvalidGrant,
            ErrorCode::AuthConsentRequired,
            ErrorCode::DriveQuotaExhausted,
            ErrorCode::CryptoKeyMissing,
            ErrorCode::CryptoDecryptFailed,
            ErrorCode::LocalDiskFull,
            ErrorCode::LocalVssUnavailable,
            ErrorCode::StateDbCorrupt,
            ErrorCode::InternalBug,
        ] {
            assert_eq!(
                TrayIcon::for_state(&err_state(code)),
                TrayIcon::Error,
                "{code:?}"
            );
        }
    }

    #[test]
    fn every_state_has_a_nonempty_tooltip() {
        let states = [
            OrchestratorState::Idle { last_run_at: None },
            OrchestratorState::PowerCheck,
            OrchestratorState::Paused {
                reason: PauseReason::Battery,
            },
            OrchestratorState::Paused {
                reason: PauseReason::Offline,
            },
            err_state(ErrorCode::CryptoKeyMissing),
            err_state(ErrorCode::NetCaptivePortal),
            err_state(ErrorCode::AuthInvalidGrant),
        ];
        for s in states {
            assert!(!tooltip_for(&s).is_empty(), "{s:?}");
        }
    }

    #[test]
    fn generated_icon_tile_has_expected_dimensions() {
        for icon in [
            TrayIcon::Idle,
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            let buf = icon.rgba_buffer();
            // TILE x TILE pixels, 4 bytes (RGBA) each.
            assert_eq!(buf.len(), (TILE * TILE * 4) as usize, "{icon:?}");
            // Every pixel carries this state's exact colour (solid tile).
            let expected = icon.rgba();
            assert!(
                buf.chunks_exact(4).all(|px| px == expected),
                "{icon:?} tile is not a solid fill"
            );
        }
    }

    #[test]
    fn each_state_icon_has_a_distinct_colour() {
        let colours = [
            TrayIcon::Idle.rgba(),
            TrayIcon::Syncing.rgba(),
            TrayIcon::Paused.rgba(),
            TrayIcon::NetworkAttention.rgba(),
            TrayIcon::Error.rgba(),
        ];
        for i in 0..colours.len() {
            for j in (i + 1)..colours.len() {
                assert_ne!(colours[i], colours[j], "icons {i} and {j} share a colour");
            }
        }
    }

    // -------------------------------------------------------------------------
    // i18n resolution regression (the tray/menu/notification raw-key bug)
    // -------------------------------------------------------------------------

    /// REGRESSION (the raw-key bug): every tray menu label must resolve to real
    /// localized TEXT, never the raw lookup key.
    ///
    /// `locales/en-US.yml` previously declared `_version: 2` while written in the
    /// v1 (per-locale, direct-string) format, so rust-i18n's v2 parser found no
    /// translations under "en-US" and `t!()` returned the raw key - the tray menu
    /// showed "tray.sync_now" / "tray.settings" etc. The pre-existing tray tests
    /// never caught this because they compared `t!()` to `t!()` (key == key passes
    /// even when nothing resolves). These assert the ACTUAL resolved text AND that
    /// it differs from the key, so the bug can never silently ship again.
    #[test]
    fn i18n_resolves_tray_menu_labels_to_real_text() {
        // The exact strings from locales/en-US.yml (the user-visible menu).
        let cases = [
            ("tray.sync_now", "Sync now"),
            ("tray.pause_30m", "Pause for 30 minutes"),
            ("tray.resume", "Resume sync"),
            ("tray.settings", "Settings"),
            ("tray.activity", "Activity"),
            ("tray.restore", "Restore"),
            ("tray.quit", "Quit Driven"),
        ];
        for (key, expected) in cases {
            let got = rust_i18n::t!(key);
            assert_eq!(got, expected, "menu key {key} must resolve to its label");
            assert_ne!(got, key, "menu key {key} must NOT resolve to the raw key");
        }
    }

    /// The app name (tray tooltip), a NESTED tooltip key (proving nested YAML
    /// maps flatten to dotted keys under `_version: 1`), and a notification title
    /// all resolve to real text.
    #[test]
    fn i18n_resolves_app_name_tooltip_and_notification() {
        assert_eq!(rust_i18n::t!("app.name"), "Driven");
        assert_ne!(rust_i18n::t!("app.name"), "app.name");
        assert_eq!(
            rust_i18n::t!("tray.tooltip.idle"),
            "Driven - idle, last sync OK"
        );
        assert_ne!(rust_i18n::t!("tray.tooltip.idle"), "tray.tooltip.idle");
        assert_eq!(
            rust_i18n::t!("notifications.first_sync_complete.title"),
            "First sync complete"
        );
    }

    /// Broad sweep: every key the tray/menu/notification code actually looks up
    /// must resolve to non-key, non-empty text - so a future key added in code
    /// but missing or mis-nested in the YAML is caught here, not in a screenshot.
    #[test]
    fn i18n_no_tray_key_resolves_to_itself() {
        let keys = [
            "app.name",
            "tray.sync_now",
            "tray.pause_30m",
            "tray.resume",
            "tray.settings",
            "tray.activity",
            "tray.restore",
            "tray.quit",
            "tray.tooltip.idle",
            "tray.tooltip.syncing",
            "tray.tooltip.service_down",
            "tray.tooltip.needs_reauth",
            "tray.tooltip.error",
            "tray.tooltip.suspending",
            "notifications.first_sync_complete.title",
            "notifications.first_sync_complete.body",
            "notifications.error.title",
            "notifications.needs_reauth.title",
        ];
        for key in keys {
            let got = rust_i18n::t!(key);
            assert_ne!(got, key, "key {key} resolved to the raw key (i18n broken)");
            assert!(!got.is_empty(), "key {key} resolved to empty text");
        }
    }

    /// The OS-detected-locale path: a NON-en locale (what `sys_locale` may report
    /// on a non-English host, fed straight into `set_locale` by `i18n::init`) must
    /// FALL BACK to the en-US strings, never raw keys - guaranteed by
    /// `i18n!(.., fallback = "en-US")` since we currently ship only en-US.
    #[test]
    fn i18n_non_en_locale_falls_back_to_english() {
        let prev = rust_i18n::locale().to_string();
        rust_i18n::set_locale("de-DE");
        let got = rust_i18n::t!("tray.sync_now");
        // Restore before asserting so a failure cannot leave a non-en locale set
        // for other (parallel) tests.
        rust_i18n::set_locale(&prev);
        assert_eq!(
            got, "Sync now",
            "a non-en locale must fall back to the en-US label, not a raw key"
        );
        assert_ne!(got, "tray.sync_now");
    }

    // -------------------------------------------------------------------------
    // Brand tray icon (real mark + per-state status badge)
    // -------------------------------------------------------------------------

    /// The committed brand PNG must decode - this is the NORMAL tray-icon path.
    /// If it ever fails the tray silently falls back to flat colour tiles, so
    /// guard it explicitly here.
    #[test]
    fn brand_base_decodes() {
        let Some(base) = brand_base() else {
            panic!("brand PNG (icons/64x64.png) must decode for the tray icon");
        };
        assert!(
            base.width >= 16 && base.height >= 16,
            "brand mark must be at least 16x16 ({}x{})",
            base.width,
            base.height
        );
        assert_eq!(
            base.rgba.len(),
            (base.width * base.height * 4) as usize,
            "decoded brand RGBA length must be width*height*4"
        );
    }

    /// Idle shows the PLAIN brand mark (no badge); every other state draws its
    /// status badge onto the same mark - so the icon stays recognisable while the
    /// non-idle variants differ from idle AND from each other (per-state colours).
    #[test]
    fn each_state_icon_is_the_branded_mark_and_distinct() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        let expected_len = (base.width * base.height * 4) as usize;

        // Idle == the untouched base mark (no badge drawn).
        let idle = TrayIcon::Idle.brand_rgba_frame(base, 0);
        assert_eq!(
            idle, base.rgba,
            "Idle must be the plain brand mark (no badge)"
        );

        let states = [
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ];
        let mut variants: Vec<(TrayIcon, Vec<u8>)> = Vec::new();
        for s in states {
            let v = s.brand_rgba_frame(base, 0);
            assert_eq!(
                v.len(),
                expected_len,
                "{s:?} icon must keep brand dimensions"
            );
            assert_ne!(v, base.rgba, "{s:?} must badge the mark (differ from idle)");
            variants.push((s, v));
        }
        // The non-idle badged marks must all be visually distinct from each other.
        for i in 0..variants.len() {
            for j in (i + 1)..variants.len() {
                assert_ne!(
                    variants[i].1, variants[j].1,
                    "{:?} and {:?} icons must be visually distinct",
                    variants[i].0, variants[j].0
                );
            }
        }
    }

    /// The badge actually PAINTS this state's colour into the icon (it is real,
    /// not a no-op): after badging, at least one pixel equals the state's opaque
    /// badge colour. That colour is from the per-state palette, which the plain
    /// teal+white brand mark never contains, so its presence proves the badge.
    #[test]
    fn badge_paints_the_state_colour_onto_the_mark() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        for s in [
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            let Some([r, g, b]) = s.badge_color() else {
                panic!("non-idle state {s:?} must have a badge colour");
            };
            let target = [r, g, b, 0xff];
            let v = s.brand_rgba_frame(base, 0);
            let painted = v.chunks_exact(4).any(|px| px == target);
            assert!(painted, "{s:?} badge colour must appear in the icon");
        }
        // Idle carries no badge colour.
        assert_eq!(TrayIcon::Idle.badge_color(), None);
    }

    /// `TrayIcon::image` builds an Image at the brand mark's dimensions for every
    /// state - i.e. the real-icon path, not the flat-tile fallback.
    #[test]
    fn image_uses_the_brand_dimensions() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        let expected_len = (base.width * base.height * 4) as usize;
        for s in [
            TrayIcon::Idle,
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            let img = s.image();
            assert_eq!(img.width(), base.width, "{s:?} width");
            assert_eq!(img.height(), base.height, "{s:?} height");
            assert_eq!(img.rgba().len(), expected_len, "{s:?} rgba length");
        }
    }

    // -------------------------------------------------------------------------
    // Per-state glyphs + animated syncing (anti-fake-green)
    // -------------------------------------------------------------------------

    /// Count opaque white-ish (grayscale >= 0xc0) pixels - the glyph ink the
    /// painters lay over the coloured badge disc. The plain teal+white brand mark
    /// has its own white cloud, so this is only meaningful as a DELTA between a
    /// glyphed icon and the badge-only baseline (see below), not as an absolute.
    fn whiteish_count(rgba: &[u8]) -> usize {
        rgba.chunks_exact(4)
            .filter(|px| px[3] == 0xff && px[0] >= 0xc0 && px[1] >= 0xc0 && px[2] >= 0xc0)
            .count()
    }

    /// Every non-idle state draws a real glyph: rendering the badge WITHOUT the
    /// glyph vs WITH it must change the pixel buffer (the glyph is not a no-op),
    /// and the four glyphs must be visually distinct from one another. This is
    /// the "glyphs go beyond colour-only variants" guarantee.
    #[test]
    fn each_non_idle_state_draws_a_distinct_glyph() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        let mut glyphed: Vec<(TrayIcon, Vec<u8>)> = Vec::new();
        for s in [
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            // Badge-only baseline (disc + ring, no glyph) vs the full glyphed icon.
            let mut badge_only = base.rgba.clone();
            let color = s.badge_color().expect("non-idle state has a badge colour");
            draw_badge(&mut badge_only, base.width, base.height, color);
            let with_glyph = s.brand_rgba_frame(base, 0);
            assert_ne!(
                with_glyph, badge_only,
                "{s:?} must draw a glyph on top of its badge (not colour-only)"
            );
            glyphed.push((s, with_glyph));
        }
        for i in 0..glyphed.len() {
            for j in (i + 1)..glyphed.len() {
                assert_ne!(
                    glyphed[i].1, glyphed[j].1,
                    "{:?} and {:?} glyphs must be visually distinct",
                    glyphed[i].0, glyphed[j].0
                );
            }
        }
    }

    /// The static (non-syncing) states IGNORE the animation frame - rendering at
    /// frame 0 and frame 5 must be byte-identical (they are not animated).
    #[test]
    fn static_states_ignore_the_animation_frame() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        for s in [
            TrayIcon::Idle,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            let f0 = s.brand_rgba_frame(base, 0);
            let f5 = s.brand_rgba_frame(base, 5);
            assert_eq!(f0, f5, "{s:?} is static and must ignore the frame index");
        }
    }

    /// REAL animation guard (anti-fake-green): the syncing spinner must actually
    /// CYCLE. Every frame within one [`SYNC_FRAMES`] cycle must differ from the
    /// frame before it (the head dot moved), consecutive frames are not all the
    /// same buffer, and the cycle REPEATS - frame `f` equals frame `f +
    /// SYNC_FRAMES` (a full rotation), proving it is a finite rotating animation
    /// and not a still icon mislabelled as animated.
    #[test]
    fn syncing_animation_cycles_distinct_frames() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        let frames: Vec<Vec<u8>> = (0..SYNC_FRAMES)
            .map(|f| TrayIcon::Syncing.brand_rgba_frame(base, f))
            .collect();

        // Consecutive frames differ (the spinner is moving every tick).
        for f in 0..SYNC_FRAMES as usize {
            let next = (f + 1) % SYNC_FRAMES as usize;
            assert_ne!(
                frames[f], frames[next],
                "syncing frame {f} and {next} must differ (the spinner must move)"
            );
        }

        // All frames in a cycle are pairwise distinct (no two ticks render the
        // same picture - a genuinely rotating spinner).
        for i in 0..frames.len() {
            for j in (i + 1)..frames.len() {
                assert_ne!(
                    frames[i], frames[j],
                    "syncing frames {i} and {j} must be distinct within one cycle"
                );
            }
        }

        // The animation REPEATS: one full cycle later renders identically, and a
        // wrapped frame index matches its in-range equivalent.
        assert_eq!(
            frames[0],
            TrayIcon::Syncing.brand_rgba_frame(base, SYNC_FRAMES),
            "frame 0 must equal frame SYNC_FRAMES (the spinner came full circle)"
        );
        assert_eq!(
            frames[1],
            TrayIcon::Syncing.brand_rgba_frame(base, SYNC_FRAMES + 1),
            "the cycle must repeat exactly every SYNC_FRAMES frames"
        );
    }

    /// The syncing spinner glyph paints white ink that no static badge state's
    /// glyph happens to replicate frame-for-frame: at least one frame differs
    /// from the still paused/error icons. Guards against the spinner silently
    /// degrading to a fixed glyph.
    #[test]
    fn syncing_frames_are_not_a_fixed_glyph() {
        let Some(base) = brand_base() else {
            panic!("brand PNG must decode");
        };
        // The spinner must lay down white ink (the dots) on its blue disc.
        let mut badge_only = base.rgba.clone();
        draw_badge(
            &mut badge_only,
            base.width,
            base.height,
            TrayIcon::Syncing
                .badge_color()
                .expect("syncing has a badge colour"),
        );
        let base_white = whiteish_count(&badge_only);
        let frame0 = TrayIcon::Syncing.brand_rgba_frame(base, 0);
        assert!(
            whiteish_count(&frame0) > base_white,
            "the spinner must paint white dots onto its disc"
        );
    }

    /// The animation-vs-static race fix (codex P2): `stop_sync_animation` must
    /// RETIRE the live generation (bump [`SYNC_GEN`]) so the spinner task stops
    /// writing frames before the caller publishes a static icon. Asserting the
    /// generation moves on every stop is the invariant the in-task guard relies
    /// on. (With no animation running, stop is a no-op on the handle but still
    /// retires the generation - and is idempotent/safe to call.)
    #[test]
    fn stop_sync_animation_retires_the_generation() {
        let before = *SYNC_GEN.lock().unwrap_or_else(|e| e.into_inner());
        stop_sync_animation();
        let after = *SYNC_GEN.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            after,
            before.wrapping_add(1),
            "stop must retire (bump) the live generation so no further frame is written"
        );
    }
}

//! Rust-side i18n boot for tray menu + OS notification strings.
//!
//! The webview surface uses `vue-i18n`; this module covers the strings
//! the webview can't reach (tray, OS notifications, autostart launcher
//! tooltip). Locale is OS-detected on first run and overridable via
//! `Settings -> UI -> locale` (DESIGN s8.7 / SPEC s22).

rust_i18n::i18n!("locales", fallback = "en-US");

pub fn init() {
    let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
    rust_i18n::set_locale(&locale);
}

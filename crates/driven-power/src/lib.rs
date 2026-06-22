//! `driven-power` — battery / AC / metered-network / sleep-wake detection.
//!
//! Exposes a `PowerSource` trait plus per-OS implementations (Windows
//! `GetSystemPowerStatus` + `WM_POWERBROADCAST`, macOS
//! `IOPMCopyAssertionsByType` + `NSWorkspace` sleep/wake notifications,
//! Linux `/sys/class/power_supply` + systemd-logind DBus). A
//! `FakePowerSource` lives in `driven-test-fixtures` so tests can push
//! state transitions.

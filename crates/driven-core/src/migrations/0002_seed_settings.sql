-- SPEC s22 default settings. INSERT OR IGNORE so re-running this migration
-- on an existing DB never clobbers a user override. The `telemetry.install_id`
-- is generated inline via `randomblob(16)` so each first-boot machine gets a
-- fresh id without any host code involvement.
--
-- Note: the SPEC s22 `windows` key is Windows-only and is seeded at runtime by
-- the platform crate, not here.

INSERT OR IGNORE INTO settings (key, value) VALUES (
  'global',
  '{
    "auto_start_on_login": false,
    "default_concurrent_uploads": null,
    "bandwidth_cap_mbps": null,
    "skip_on_battery": true,
    "skip_on_metered": true,
    "scan_interval_secs": 600,
    "deep_verify_interval_secs": 604800,
    "io_priority": "low",
    "log_level": "info"
  }'
);

INSERT OR IGNORE INTO settings (key, value) VALUES (
  'telemetry',
  json_object(
    'enabled', json('true'),
    'install_id', lower(hex(randomblob(16))),
    'endpoint', 'https://driven.maxhogan.dev/telemetry/v1/ping'
  )
);

INSERT OR IGNORE INTO settings (key, value) VALUES (
  'updater',
  '{
    "channel": "stable",
    "check_interval_secs": 21600
  }'
);

INSERT OR IGNORE INTO settings (key, value) VALUES (
  'ui',
  '{
    "tray_left_click_opens": "activity",
    "locale": "en-US",
    "color_mode": "system"
  }'
);

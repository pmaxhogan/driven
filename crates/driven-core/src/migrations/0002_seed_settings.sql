-- SPEC s22 default settings. INSERT OR IGNORE so re-running this migration
-- on an existing DB never clobbers a user override. The `telemetry.install_id`
-- is generated inline as a canonical UUID v4 (SPEC s16 / M9b P1-3) so each
-- first-boot machine gets a fresh id without any host code involvement, and the
-- id passes the public Worker's UUID-v4 validation. It is built from
-- `hex(randomblob(16))` (32 random hex chars) with the version nibble forced to
-- `4` and the variant nibble forced into `8|9|a|b` (RFC 4122). `ensure_install_id`
-- in telemetry.rs additionally REPLACES any empty / legacy non-UUID-v4 value once,
-- so pre-M9b DBs (which seeded a bare `hex(randomblob(16))`) are healed on startup.
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
    -- Canonical UUID v4: 8-4-4-4-12 lowercase hex, version nibble '4', variant
    -- nibble in {8,9,a,b}. Built from 32 random hex chars (hex(randomblob(16)))
    -- with positions 13 (version) and 17 (variant) overridden.
    'install_id', (
      WITH r(h) AS (SELECT lower(hex(randomblob(16))))
      SELECT
        substr(h, 1, 8) || '-' ||
        substr(h, 9, 4) || '-' ||
        '4' || substr(h, 14, 3) || '-' ||
        substr('89ab', (abs(random()) % 4) + 1, 1) || substr(h, 18, 3) || '-' ||
        substr(h, 21, 12)
      FROM r
    ),
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

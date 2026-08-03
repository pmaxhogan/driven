import { describe, it, expect } from "vitest";

import { i18n } from "../i18n";
import { activityEventLabel } from "../stores/activityEventLabel";

// R1-P2-3: the Activity table localizes the raw `eventType` code via t() with a
// safe fallback for unknown types. These tests exercise the real en-US locale
// through the shared `activityEventLabel` helper.
const t = i18n.global.t as (key: string) => string;
const te = i18n.global.te as (key: string) => boolean;

function label(eventType: string): string {
  return activityEventLabel(eventType, t, te);
}

describe("activityEventLabel (R1-P2-3)", () => {
  it("localizes curated activity event types from activity.events", () => {
    // The new R1-P1-1 success rows must have curated labels.
    expect(label("upload_done")).toBe("Uploaded");
    expect(label("trash_done")).toBe("Removed");
    // Plus the documented vocabulary.
    expect(label("scan_done")).toBe("Scan complete");
    expect(label("paused")).toBe("Paused");
    expect(label("local.unicode_collision")).toBe("Name collision");
    // Every backend-emitted event type must be humanized - not shown as the raw
    // snake_case code (the inconsistency: deep_verify_done / update_applied used
    // to render raw in the table + filter while upload_done showed "Uploaded").
    expect(label("deep_verify_done")).toBe("Deep verify complete");
    expect(label("update_applied")).toBe("App updated");
    // The 2.2.0 bundling path emits `bundle_upload` (orchestrator's single Info
    // row per committed bundle); it rendered as the raw snake_case code in the
    // event-type filter dropdown and the table until this label landed.
    expect(label("bundle_upload")).toBe("Uploaded (bundled)");
    // Pre/post backup hook rows (`hook.pre` / `hook.post`) were the other two
    // backend-emitted types with no curated label.
    expect(label("hook.pre")).toBe("Pre-backup hook");
    expect(label("hook.post")).toBe("Post-backup hook");
    // The run-completion row the orchestrator writes when a cycle's ops all
    // succeeded (the feed used to trail off after the last per-file row).
    expect(label("backup_done")).toBe("Backup complete");
    // The remote-existence audit's summary row. Its `file_count` carries how
    // many files were re-queued, so the label must read as an OUTCOME rather
    // than as "an audit ran" - the row is written only when damage was found.
    expect(label("remote_audit_done")).toBe("Missing backups re-queued");
  });

  it("falls back to errors.<code>.short for error/skip code event types", () => {
    // A Failed / Skipped row carries a SPEC s24 error code as its event type;
    // those are localized via the shared error labels.
    expect(label("drive.checksum_mismatch")).toBe("Verification failed");
    expect(label("local.file_locked")).toBe("File in use");
    // A permission / Full-Disk-Access denial is its own skip code, and must NOT
    // read as "File in use" (nothing is holding the file) or as a disk error.
    expect(label("local.permission_denied")).toBe("Permission needed");
    // The self-healing stale-drive_file_id skip: a warn row whose event type is
    // this code, so the Activity table must have a label for it rather than
    // showing the raw dotted string.
    expect(label("drive.remote_file_missing")).toBe("Remote copy missing");
    // A failed restore drill writes its SPEC s24 code as the event type. This is
    // the single most important row this feed can carry - "a file you believe is
    // backed up would not come back" - so it must never render as the raw dotted
    // string. Nothing else catches a missing key here: eslint's no-unused-keys
    // flags a key with no consumer, never a consumer with no key, and vue-i18n
    // only warns and renders the path.
    expect(label("restore.drill_failed")).toBe("Restore drill failed");
  });

  it("safely falls back to the raw code for an unknown event type", () => {
    // A forward-compatible / unknown code renders verbatim, never blank or a
    // thrown error.
    expect(label("future.unknown_code")).toBe("future.unknown_code");
    expect(label("")).toBe("");
  });
});

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
  });

  it("falls back to errors.<code>.short for error/skip code event types", () => {
    // A Failed / Skipped row carries a SPEC s24 error code as its event type;
    // those are localized via the shared error labels.
    expect(label("drive.checksum_mismatch")).toBe("Verification failed");
    expect(label("local.file_locked")).toBe("File in use");
  });

  it("safely falls back to the raw code for an unknown event type", () => {
    // A forward-compatible / unknown code renders verbatim, never blank or a
    // thrown error.
    expect(label("future.unknown_code")).toBe("future.unknown_code");
    expect(label("")).toBe("");
  });
});

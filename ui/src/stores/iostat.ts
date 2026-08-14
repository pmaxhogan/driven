import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onSyncIoThroughput } from "../ipc/events";
import type { IoSample } from "../ipc/types";

/**
 * Live disk/network throughput store (2026-08-14 follow-up). The backend
 * samples the app-global IO counters once a second; this store seeds from the
 * `io_throughput_series` trailing ring and then folds each `sync:io_throughput`
 * event, so the Activity dashboard's split graphs move in REAL TIME - including
 * during the reconcile-phase resume that is invisible to the activity-log-backed
 * series (it writes no activity rows until it completes).
 *
 * The backend suppresses events while fully idle (one trailing zero, then
 * silence), so an idle app costs nothing here; the charts simply hold their
 * decayed-to-zero tail.
 */

/** How many trailing samples the charts plot (2 minutes at the 1s cadence). */
export const IO_WINDOW_SAMPLES = 120;

/** Samples averaged for the headline rate (a 5s window: fast enough to feel
 * live, wide enough not to flicker between wire-chunk acks). */
export const IO_HEADLINE_SAMPLES = 5;

/** Read a finite non-negative number, defaulting to 0 (untrusted wire data). */
function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : 0;
}

/** Coerce one wire sample (missing/garbled fields become zeros). */
function readSample(s: unknown): IoSample {
  const o = (s ?? {}) as Record<string, unknown>;
  return { tsMs: num(o["tsMs"]), diskBytes: num(o["diskBytes"]), netBytes: num(o["netBytes"]) };
}

export const useIostatStore = defineStore("iostat", () => {
  /** Width of one sample interval in ms (from the backend; 1000 in practice). */
  const bucketMs = ref<number>(1000);
  /** Trailing samples, oldest first, capped at [`IO_WINDOW_SAMPLES`]. */
  const samples = ref<IoSample[]>([]);

  function push(sample: IoSample): void {
    const next = [...samples.value, sample];
    samples.value = next.length > IO_WINDOW_SAMPLES ? next.slice(-IO_WINDOW_SAMPLES) : next;
  }

  /** Bytes-per-bucket series for the two sparklines, oldest first. */
  const diskSeries = computed<number[]>(() => samples.value.map((s) => s.diskBytes));
  const netSeries = computed<number[]>(() => samples.value.map((s) => s.netBytes));

  /** Headline bytes/sec over the last few samples, or null before any sample
   * has arrived (the tiles render their empty state). */
  function rateOf(series: number[]): number | null {
    if (series.length === 0 || bucketMs.value <= 0) return null;
    const window = series.slice(-IO_HEADLINE_SAMPLES);
    const bytes = window.reduce((a, b) => a + b, 0);
    return bytes / ((window.length * bucketMs.value) / 1000);
  }
  const diskRate = computed<number | null>(() => rateOf(diskSeries.value));
  const netRate = computed<number | null>(() => rateOf(netSeries.value));

  // --- lifecycle (the Activity view owns the registration) ------------------
  let unlisten: UnlistenFn | null = null;
  let started = false;

  /** Seed the ring + subscribe to live samples (idempotent). Best-effort: a
   * failed seed still leaves the live stream to fill the charts. */
  async function start(): Promise<void> {
    if (started) return;
    started = true;
    try {
      const dto = await ipc.ioThroughputSeries();
      if (typeof dto?.bucketMs === "number" && dto.bucketMs > 0) bucketMs.value = dto.bucketMs;
      const seeded = Array.isArray(dto?.samples) ? dto.samples.map(readSample) : [];
      samples.value = seeded.slice(-IO_WINDOW_SAMPLES);
    } catch (e) {
      console.error("io throughput seed failed", e);
    }
    try {
      const un = await onSyncIoThroughput((sample) => push(readSample(sample)));
      // stop() may have raced ahead while we awaited; honor it.
      if (!started) {
        un();
        return;
      }
      unlisten = un;
    } catch (e) {
      started = false;
      console.error("io throughput subscribe failed", e);
    }
  }

  /** Stop the live subscription (view unmount). */
  function stop(): void {
    started = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    bucketMs,
    samples,
    diskSeries,
    netSeries,
    diskRate,
    netRate,
    push,
    start,
    stop,
  };
});

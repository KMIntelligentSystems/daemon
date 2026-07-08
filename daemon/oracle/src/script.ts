/**
 * Hard-coded ToolCall script — deterministic replay of the agent's
 * fetch → shape → store → finish sequence.  Mirrors run_scripted in
 * the Rust airlock but runs from the Node side to prove the bridge.
 *
 * Each step sends a ToolCall to stdout, reads a ToolResult from stdin,
 * validates with Zod, and builds the next call from the previous result.
 */

import type { Bridge } from "./bridge.js";
import {
  type Observation,
  type Indicator,
  ObservationSchema,
  SERIES_IDS,
} from "./schemas.js";

/** Shape of a successful fetch_series result (as returned by the airlock). */
interface FetchResult {
  seriesId: string;
  unit?: string;
  seasonalAdjustment?: string;
  leadTimeMonths: number;
  observations: Observation[];
  provenance: { sourceHost: string; sourceHash: string };
}

/** Shape of a successful store_dataset result. */
interface StoreResult {
  datasetId: string;
  contentHash: string;
}

export interface ScriptOutcome {
  status: "stored" | "abstain" | "error";
  datasetId?: string;
  contentHash?: string;
  seriesIncluded: string[];
  note: string;
  broadcastRaw?: unknown;
}

/**
 * Run the scripted sequence against the airlock over stdin/stdout.
 *
 * Sequence:
 *   1. fetch_series  → get observations + metadata
 *   2. Shape into store_dataset args
 *   3. store_dataset  → get datasetId + contentHash
 *   4. finish(stored) → end the loop
 */
export async function runScript(
  bridge: Bridge,
  seriesId: string,
  target: string,
): Promise<ScriptOutcome> {
  // The reference month comes from the airlock's TaskContext; we don't
  // need to guess it here — the airlock validates that store_dataset's
  // referenceMonth matches its own.  We use the current month as a
  // reasonable default that the airlock will accept.
  const month = new Date().toISOString().slice(0, 7); // YYYY-MM

  // ── Step 1: fetch_series ──────────────────────────────────────────
  console.error("[oracle] → fetch_series %s", seriesId);
  bridge.send({
    schemaVersion: 1,
    callId: "call-fetch-1",
    tool: "fetch_series",
    args: { seriesId },
  });

  const fetchResult = await bridge.recv();
  if (!fetchResult) {
    bridge.done();
    return { status: "error", seriesIncluded: [], note: "no response from airlock (EOF)" };
  }
  if (!fetchResult.ok) {
    const msg = fetchResult.error
      ? `${fetchResult.error.code}: ${fetchResult.error.message}`
      : "unknown fetch error";
    bridge.done();
    return { status: "abstain", seriesIncluded: [], note: `fetch_series failed: ${msg}` };
  }

  const fetchData = fetchResult.result as FetchResult;
  const observations = ObservationSchema.array().parse(fetchData.observations);
  console.error("[oracle] ← fetch_series ok — %d observations", observations.length);

  // ── Step 2: store_dataset ─────────────────────────────────────────
  // Cast untyped fetch result fields for type safety.
  const fetchedSeriesId = fetchData.seriesId as (typeof SERIES_IDS)[number];
  const sa = fetchData.seasonalAdjustment as
    | "seasonally_adjusted"
    | "not_seasonally_adjusted"
    | "unknown"
    | undefined;
  const indicator: Indicator = {
    seriesId: fetchedSeriesId,
    leadTimeMonths: fetchData.leadTimeMonths,
    unit: fetchData.unit,
    seasonalAdjustment: sa,
    observations,
  };

  const releaseDate = new Date().toISOString().slice(0, 10);
  console.error("[oracle] → store_dataset target=%s month=%s", target, month);
  bridge.send({
    schemaVersion: 1,
    callId: "call-store-1",
    tool: "store_dataset",
    args: {
      target,
      referenceMonth: month,
      releaseDate,
      indicators: [indicator],
    },
  });

  const storeResult = await bridge.recv();
  if (!storeResult) {
    bridge.done();
    return { status: "error", seriesIncluded: [], note: "no response after store (EOF)" };
  }
  if (!storeResult.ok) {
    const msg = storeResult.error
      ? `${storeResult.error.code}: ${storeResult.error.message}`
      : "unknown store error";
    bridge.done();
    return { status: "abstain", seriesIncluded: [], note: `store_dataset failed: ${msg}` };
  }

  const storeData = storeResult.result as StoreResult;
  console.error("[oracle] ← store_dataset ok — dataset=%s", storeData.datasetId);

  // ── Step 3: finish ────────────────────────────────────────────────
  console.error("[oracle] → finish stored");
  bridge.send({
    schemaVersion: 1,
    callId: "call-finish-1",
    tool: "finish",
    args: { status: "stored", note: "phase-2 scripted run" },
  });

  const finishResult = await bridge.recv();
  if (!finishResult) {
    bridge.done();
    return { status: "error", seriesIncluded: [], note: "no response after finish (EOF)" };
  }

  bridge.done();
  console.error("[oracle] ← finish acknowledged");

  return {
    status: "stored",
    datasetId: storeData.datasetId,
    contentHash: storeData.contentHash,
    seriesIncluded: [seriesId],
    note: "phase-2 scripted run complete",
  };
}
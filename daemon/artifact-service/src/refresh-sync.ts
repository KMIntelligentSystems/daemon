/**
 * refresh-sync.ts — the azure-side bridge artifact-service → refresh-daemon.
 *
 * Port of http_proxy/src/refresh-history-bridge.ts, adapted to this service's
 * flat `artifact` table (id / tags / url — no replaces_id chain, content lives
 * in files/ rather than a content column). The pipeline is isomorphic:
 *
 *   artifact rows tagged with a seriesId (mime_type = 'text/csv')
 *     → strict re-validation against the baked-in series-map allowlist
 *     → dates normalized to YYYY-MM
 *     → HMAC-signed POST (X-Daemon-Sig) to POST $REFRESH_DAEMON_URL/refresh/bootstrap
 *     → refresh-daemon upserts into indicator_history keyed on (series_id, date)
 *
 * The LLM never sees the payload — a thin broker tool
 * (azure-foundry: sync_indicator_history) just fires POST /refresh-sync here
 * with the user's X-User-Id / X-User-Role headers. Same trust shape as the
 * http_proxy orchestrator tool, whose execute() just calls syncIndicatorHistory().
 *
 * Idempotent by construction: the daemon's INSERT OR REPLACE converges no
 * matter how often the same content is pushed.
 */
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

const DEFAULT_SERIES_MAP_PATH = path.join(process.cwd(), "data", "series-map.json");
const SERIES_MAP_PATH = process.env["SERIES_MAP_PATH"] ?? DEFAULT_SERIES_MAP_PATH;
export const REFRESH_DAEMON_URL = process.env["REFRESH_DAEMON_URL"] ?? "http://127.0.0.1:8792";
const HMAC_KEY = process.env["DAEMON_HMAC_KEY"] ?? "dev-insecure-hmac-key-change-me";

export function hmacSign(payload: string): string {
  return crypto.createHmac("sha256", HMAC_KEY).update(payload).digest("hex");
}

export interface SeriesSyncEntry {
  seriesId: string;
  status: "sent" | "would-send" | "missing" | "invalid";
  artifactId?: string;
  observations?: number;
  range?: [string, string];
  warning?: string;
  error?: string;
}

export interface SyncReport {
  ok: boolean;
  dryRun: boolean;
  reason: string;
  daemonUrl: string;
  series: SeriesSyncEntry[];
  daemon?: { httpStatus?: number; seeded?: number; error?: string };
}

interface SeriesSpec {
  header: string;
  valueRange: [number, number];
  minObs: number;
}
interface SeriesMap {
  series: Record<string, SeriesSpec>;
}

export function loadSeriesMap(): SeriesMap {
  return JSON.parse(fs.readFileSync(SERIES_MAP_PATH, "utf-8")) as SeriesMap;
}

/** Strict re-validation (defense in depth — the artifact row's file is the
 *  last gate before the refresh target). Normalizes dates to YYYY-MM so a
 *  CSV-sourced month and a broadcast-sourced month collide on the same
 *  (series_id, date) upsert key. Returns null + errors when invalid. */
function parseBackboneCsv(
  seriesId: string,
  spec: SeriesSpec,
  content: string,
): { observations: { date: string; value: number }[]; errors: string[] } {
  const errors: string[] = [];
  const text = content.charCodeAt(0) === 0xfeff ? content.slice(1) : content;
  const lines = text.replace(/\r\n/g, "\n").split("\n").filter((l) => l.trim() !== "");
  if (lines.length < 2) errors.push(`${seriesId}: <2 lines`);
  else if (lines[0].trim() !== spec.header) errors.push(`${seriesId}: header "${lines[0]}" != "${spec.header}"`);
  const seen = new Set<string>();
  const observations: { date: string; value: number }[] = [];
  for (let i = 1; i < lines.length; i++) {
    const cols = lines[i].split(",");
    if (cols.length !== 2) { errors.push(`${seriesId}:${i + 1}: ${cols.length} columns`); continue; }
    const ds = cols[0].trim();
    if (!/^\d{4}-(0[1-9]|1[0-2])(-\d{2})?$/.test(ds)) { errors.push(`${seriesId}:${i + 1}: bad date "${ds}"`); continue; }
    const month = ds.slice(0, 7); // ← the YYYY-MM normalization
    if (seen.has(month)) { errors.push(`${seriesId}:${i + 1}: duplicate month ${month}`); continue; }
    seen.add(month);
    const v = Number(cols[1].trim());
    if (!Number.isFinite(v)) { errors.push(`${seriesId}:${i + 1}: non-numeric "${cols[1]}"`); continue; }
    if (v < spec.valueRange[0] || v > spec.valueRange[1]) { errors.push(`${seriesId}:${i + 1}: ${v} outside [${spec.valueRange}]`); continue; }
    observations.push({ date: month, value: v });
  }
  if (observations.length < spec.minObs) errors.push(`${seriesId}: ${observations.length} obs < minObs ${spec.minObs}`);
  observations.sort((a, b) => a.date.localeCompare(b.date));
  return { observations, errors };
}

interface ArtifactCsvRow {
  id: string;
  url: string;
  created_at: string;
}

/** Pull the newest text/csv artifact row carrying the seriesId tag, then read
 *  its file content from FILES_DIR. The url column is "/files/<storedName>"
 *  (uploaded by POST /artifacts/upload). basename() strips any escape. */
function readBackboneRow(
  db: DatabaseSync,
  filesDir: string,
  seriesId: string,
): { id: string; content: string; warning?: string } | null {
  const rows = db.prepare(
    `SELECT id, url, created_at FROM artifact
     WHERE mime_type = 'text/csv' AND tags LIKE ?
     ORDER BY created_at DESC`,
  ).all(`%${seriesId}%`) as unknown as ArtifactCsvRow[];
  if (rows.length === 0) return null;
  if (!rows[0].url.startsWith("/files/")) return null; // url doesn't match the upload convention
  const storedName = path.basename(rows[0].url);
  const file = path.join(filesDir, storedName);
  if (!fs.existsSync(file)) return null;
  const warning = rows.length > 1
    ? `${rows.length} text/csv heads carry series tag "${seriesId}" — using the newest (${rows[0].id}); curate the others`
    : undefined;
  return { id: rows[0].id, content: fs.readFileSync(file, "utf8"), warning };
}

/** Build the per-series report; POST unless dryRun. */
export async function syncIndicatorHistory(
  db: DatabaseSync,
  filesDir: string,
  opts: { dryRun?: boolean; reason?: string } = {},
): Promise<SyncReport> {
  const report: SyncReport = {
    ok: true,
    dryRun: opts.dryRun ?? false,
    reason: opts.reason ?? "api",
    daemonUrl: REFRESH_DAEMON_URL,
    series: [],
  };

  let map: SeriesMap;
  try {
    map = loadSeriesMap();
  } catch (err) {
    report.ok = false;
    report.daemon = { error: `series-map unreadable: ${err instanceof Error ? err.message : String(err)}` };
    return report;
  }

  const payloadSeries: { seriesId: string; observations: { date: string; value: number }[] }[] = [];
  for (const [seriesId, spec] of Object.entries(map.series)) {
    const row = readBackboneRow(db, filesDir, seriesId);
    if (!row) {
      report.series.push({ seriesId, status: "missing", error: "no tagged text/csv artifact in the catalog — upload via /artifacts/upload + save" });
      continue;
    }
    const parsed = parseBackboneCsv(seriesId, spec, row.content);
    const base: Omit<SeriesSyncEntry, "status"> = {
      seriesId,
      artifactId: row.id,
      observations: parsed.observations.length,
      ...(parsed.observations.length
        ? { range: [parsed.observations[0].date, parsed.observations[parsed.observations.length - 1].date] as [string, string] }
        : {}),
      ...(row.warning ? { warning: row.warning } : {}),
    };
    if (parsed.errors.length) {
      report.ok = false;
      report.series.push({ ...base, status: "invalid", error: parsed.errors.slice(0, 5).join("; ") });
      continue;
    }
    report.series.push({ ...base, status: report.dryRun ? "would-send" : "sent" });
    payloadSeries.push({ seriesId, observations: parsed.observations });
  }

  // A series that fails validation is NOT posted (closed surface); the rest go.
  if (report.dryRun) return report;

  const body = JSON.stringify({ series: payloadSeries });
  try {
    const resp = await fetch(`${REFRESH_DAEMON_URL}/refresh/bootstrap`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Daemon-Sig": hmacSign(body) },
      body,
    });
    const json = (await resp.json().catch(() => ({}))) as { seeded?: number; error?: string };
    report.daemon = { httpStatus: resp.status, seeded: json.seeded, ...(json.error ? { error: json.error } : {}) };
    if (!resp.ok) report.ok = false;
  } catch (err) {
    report.ok = false;
    report.daemon = { error: `unreachable: ${err instanceof Error ? err.message : String(err)}` };
  }
  return report;
}

export interface RefreshSyncOpts {
  dryRun?: boolean;
}

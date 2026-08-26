/**
 * refresh-sync endpoint test — stub refresh-daemon + temp artifact catalog.
 *
 * Proves:
 *   1. POST /refresh-sync (admin) → /refresh/bootstrap received a correctly
 *      HMAC-signed payload containing the test series' normalized YYYY-MM obs.
 *   2. dryRun:true does not post to the daemon.
 *   3. Non-admin caller → 403.
 *   4. A series-map entry with no tagged csv artifact is reported 'missing';
 *      the catalog never posts whatever it doesn't validate (closed allowlist).
 */
import http from "node:http";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SERVICE_DIR = path.resolve(__dirname, "..");
const HMAC_KEY = "dev-insecure-hmac-key-change-me";

let passed = 0, failed = 0;
const check = (name, cond, detail = "") =>
  cond ? (passed++, console.log(`${name}: PASS`)) : (failed++, console.log(`${name}: FAIL ${detail}`));

function getPort() {
  return new Promise((resolve) => {
    const s = http.createServer();
    s.listen(0, () => { const p = s.address().port; s.close(() => resolve(p)); });
  });
}

// ── Stub refresh-daemon ────────────────────────────────────────────────────
const posted = [];
const exportCalls = [];
const daemonPort = await getPort();
const daemon = http.createServer((req, res) => {
  if (req.method === "POST" && req.url === "/refresh/bootstrap") {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const expected = crypto.createHmac("sha256", HMAC_KEY).update(body).digest("hex");
      if (req.headers["x-daemon-sig"] !== expected) {
        res.writeHead(401, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "bad sig" }));
        return;
      }
      const json = JSON.parse(body);
      posted.push(json);
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ seeded: json.series.reduce((n, s) => n + s.observations.length, 0) }));
    });
    return;
  }
  if (req.method === "POST" && req.url === "/refresh/export-panel") {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const expected = crypto.createHmac("sha256", HMAC_KEY).update(body).digest("hex");
      if (req.headers["x-daemon-sig"] !== expected) {
        res.writeHead(401, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "bad export sig" }));
        return;
      }
      exportCalls.push(JSON.parse(body));
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({
        subjectId: "stub",
        series: ["test_series"],
        rows: [{ seriesId: "test_series", observations: [{ date: "2024-01", value: 10, is_preliminary: 0 }] }],
        panelHash: "abc123stub",
      }));
    });
    return;
  }
  res.writeHead(404).end();
});
await new Promise((r) => daemon.listen(daemonPort, r));

// ── Temp catalog: one tagged csv series, one series-map entry missing ─────
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "artifact-svc-test-"));
const filesDir = path.join(tmp, "files");
fs.mkdirSync(filesDir, { recursive: true });
const dbPath = path.join(tmp, "artifacts.db");
const seriesMapPath = path.join(tmp, "series-map.json");
const refreshDbPath = path.join(tmp, "refresh.db");
const csv = "date,value\n2024-01,10\n2024-02,11\n2024-03-15,12\n";
fs.writeFileSync(
  seriesMapPath,
  JSON.stringify({
    series: {
      test_series: { header: "date,value", valueRange: [0, 1000], minObs: 2 },
      absent_series: { header: "date,value", valueRange: [0, 1000], minObs: 1 },
    },
  }),
);

// ── Seed refresh.db with one test_series (the "refresh-daemon" in the test) ─
{
  const { DatabaseSync } = await import("node:sqlite");
  const rdb = new DatabaseSync(refreshDbPath);
  rdb.exec("CREATE TABLE IF NOT EXISTS indicator_history (series_id TEXT NOT NULL, date TEXT NOT NULL, value REAL, is_preliminary INTEGER, observed_at TEXT, PRIMARY KEY (series_id, date))");
  rdb.prepare("INSERT INTO indicator_history (series_id, date, value, is_preliminary, observed_at) VALUES (?,?,?,?,?)").run("test_series", "2024-01", 10, 0, new Date().toISOString());
  rdb.prepare("INSERT INTO indicator_history (series_id, date, value, is_preliminary, observed_at) VALUES (?,?,?,?,?)").run("test_series", "2024-02", 11, 0, new Date().toISOString());
  rdb.close();
}

const svcPort = await getPort();
const svc = spawn("node", ["dist/server.js"], {
  cwd: SERVICE_DIR,
  env: {
    ...process.env,
    PORT: String(svcPort),
    ARTIFACT_DB_PATH: dbPath,
    ARTIFACT_FILES_DIR: filesDir,
    SERIES_MAP_PATH: seriesMapPath,
    REFRESH_DAEMON_URL: `http://127.0.0.1:${daemonPort}`,
    REFRESH_DB_PATH: refreshDbPath,
    DAEMON_HMAC_KEY: HMAC_KEY,
  },
  stdio: "pipe",
});
svc.on("exit", (code) => { if (code && code !== 0) console.error(`[svc] exited ${code}`); });

// Wait for dbReady
for (let i = 0; i < 50; i++) {
  try {
    const h = await fetch(`http://127.0.0.1:${svcPort}/health`).then((r) => r.json());
    if (h.dbReady) break;
  } catch { /* retry */ }
  if (i === 49) { console.error("service never became ready"); process.exit(1); }
  await new Promise((r) => setTimeout(r, 200));
}

const headers = (extra = {}) => ({
  "X-User-Id": "test",
  "X-User-Role": "admin",
  ...extra,
});

// Upload the csv content → /artifacts/upload, then catalog it with the series tag
const upload = await fetch(`http://127.0.0.1:${svcPort}/artifacts/upload`, {
  method: "POST",
  headers: headers({ "X-File-Name": "test_series.csv", "X-Mime-Type": "text/csv" }),
  body: csv,
}).then((r) => r.json());
check("upload", upload.url?.startsWith("/files/"), JSON.stringify(upload));

const saved = await fetch(`http://127.0.0.1:${svcPort}/artifacts`, {
  method: "POST",
  headers: headers({ "Content-Type": "application/json" }),
  body: JSON.stringify({
    category: "Economics",
    subject: "Test Backbone",
    title: "Test Series CSV",
    mimeType: "text/csv",
    url: upload.url,
    tags: '["test_series","nsa"]',
  }),
}).then((r) => r.json());
check("catalog", Boolean(saved.id), JSON.stringify(saved));

// 1. admin sync
const rep = await fetch(`http://127.0.0.1:${svcPort}/refresh-sync`, {
  method: "POST",
  headers: headers({ "Content-Type": "application/json" }),
  body: JSON.stringify({}),
}).then((r) => r.json());
check("admin sync ok", rep.ok === true, JSON.stringify(rep));
const testSeries = rep.series.find((s) => s.seriesId === "test_series");
const absentSeries = rep.series.find((s) => s.seriesId === "absent_series");
check("sent status", testSeries?.status === "sent", JSON.stringify(rep.series));
check("missing status", absentSeries?.status === "missing", JSON.stringify(rep.series));
check("obs count", testSeries?.observations === 3, JSON.stringify(testSeries));
check("posted to daemon", posted.length === 1, JSON.stringify(posted));
const payload = posted[0];
check("payload has test_series only",
  payload?.series.length === 1 && payload.series[0].seriesId === "test_series",
  JSON.stringify(payload));
const obs = payload?.series[0]?.observations;
check("YYYY-MM normalization (2024-03-15 → 2024-03)",
  obs?.some((o) => o.date === "2024-03" && o.value === 12),
  JSON.stringify(obs));

// 2. dryRun does not post
const dry = await fetch(`http://127.0.0.1:${svcPort}/refresh-sync`, {
  method: "POST",
  headers: headers({ "Content-Type": "application/json" }),
  body: JSON.stringify({ dryRun: true }),
}).then((r) => r.json());
check("dryRun would-send", dry.series.find((s) => s.seriesId === "test_series")?.status === "would-send");
check("dryRun skipped daemon", posted.length === 1);

// 3. non-admin rejected
const denied = await fetch(`http://127.0.0.1:${svcPort}/refresh-sync`, {
  method: "POST",
  headers: { "X-User-Id": "test", "Content-Type": "application/json" },
  body: JSON.stringify({}),
});
check("non-admin → 403", denied.status === 403);

// 4. refresh-panel: admin → 200 with row order + hash; non-admin → 403
const panel = await fetch(`http://127.0.0.1:${svcPort}/refresh-panel`, {
  method: "POST",
  headers: headers({ "Content-Type": "application/json" }),
  body: JSON.stringify({ subject: "Test Backbone", series: ["test_series"] }),
});
check("refresh-panel → 200", panel.status === 200);
const panelJson = await panel.json();
check("two observations", panelJson.rows?.[0]?.observations?.length === 2, JSON.stringify(panelJson));
check("observations ordered by date ASC",
  panelJson.rows?.[0]?.observations?.[0]?.date === "2024-01" && panelJson.rows?.[0]?.observations?.[1]?.date === "2024-02",
  JSON.stringify(panelJson.rows?.[0]?.observations));
check("panelHash present", typeof panelJson.panelHash === "string" && panelJson.panelHash.length === 64, JSON.stringify(panelJson.panelHash?.length));
const panelDenied = await fetch(`http://127.0.0.1:${svcPort}/refresh-panel`, {
  method: "POST",
  headers: { "X-User-Id": "test", "Content-Type": "application/json" },
  body: JSON.stringify({ series: ["test_series"] }),
});
check("refresh-panel non-admin → 403", panelDenied.status === 403);

await new Promise((resolve) => {
  svc.on("exit", resolve);
  svc.kill();
});
daemon.close();
try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* Windows lock race — tolerate */ }
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);

/**
 * Artifact Service — manages saved artifact metadata (catalog tree).
 *
 * Separate from refresh-daemon (which holds flow-2 refresh.db). This service
 * owns the user-scoped artifact catalog: save, list, categorize, retrieve.
 * The React app's left sidebar (Documents) calls this; the orchestrator's
 * artifacts get saved here from workspace to catalog.
 *
 * Storage: SQLite on Azure Files (/data/artifacts.db — NOT the http_proxy
 * artifacts.db; this is a fresh service-specific DB).
 */
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { syncIndicatorHistory, hmacSign, REFRESH_DAEMON_URL } from "./refresh-sync.js";

const PORT = Number(process.env["PORT"] ?? 8793);
const DB_PATH = process.env["ARTIFACT_DB_PATH"] ?? path.join(process.cwd(), "data", "artifacts.db");
const FILES_DIR = process.env["ARTIFACT_FILES_DIR"] ?? path.join(process.cwd(), "data", "files");
fs.mkdirSync(FILES_DIR, { recursive: true });

// ── CORS ─────────────────────────────────────────────────────────────────
// The React app (SWA + Vite dev server) calls this cross-origin with custom
// X-User-Id / X-User-Role headers, which forces a preflight OPTIONS. Bare
// node:http has no CORS handling, so we add it here. Configure the allow-list
// with ALLOWED_ORIGINS (comma-separated); "*" allows any origin.
const ALLOWED_ORIGINS = (process.env["ALLOWED_ORIGINS"] ?? "*")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
const ALLOW_ANY = ALLOWED_ORIGINS.includes("*");

function applyCors(req: http.IncomingMessage, res: http.ServerResponse): void {
  const origin = req.headers["origin"] as string | undefined;
  if (!origin) return; // non-browser / same-origin client — no CORS needed
  const allowed = ALLOW_ANY || ALLOWED_ORIGINS.includes(origin);
  res.setHeader("Access-Control-Allow-Origin", allowed ? (ALLOW_ANY ? "*" : origin) : "null");
  res.setHeader("Vary", "Origin");
  res.setHeader("Access-Control-Allow-Methods", "GET,POST,DELETE,OPTIONS");
  res.setHeader("Access-Control-Allow-Headers", "Content-Type,X-User-Id,X-User-Role,X-File-Name,X-Mime-Type");
  res.setHeader("Access-Control-Max-Age", "86400");
}

interface ArtifactRow {
  id: string;
  user_id: string;
  category: string;
  subject: string;
  title: string;
  mime_type: string;
  url: string;
  created_at: string;
  tags: string | null;
}

const CREATE_SQL = `
    CREATE TABLE IF NOT EXISTS artifact (
      id         TEXT PRIMARY KEY,
      user_id    TEXT NOT NULL,
      category   TEXT NOT NULL,
      subject    TEXT NOT NULL,
      title      TEXT NOT NULL,
      mime_type  TEXT NOT NULL,
      url        TEXT NOT NULL,
      created_at TEXT NOT NULL,
      tags       TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_artifact_user ON artifact(user_id, category, subject);
  `;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// Azure Files (SMB/cifs) mounts in ACA do not support POSIX byte-range
// locking, which SQLite's default "unix" VFS requires — every open fails
// with "database is locked" (fcntl → ENOLCK → SQLITE_BUSY), even on a
// brand-new file with zero SMB handles. The "unix-dotfile" VFS uses a lock
// FILE (<db>.lock) instead, which works over SMB and still gives real
// cross-process mutual exclusion (e.g. old/new replica overlap during a
// revision swap). WAL stays off: its shared-memory index needs mmap, which
// is unreliable on network filesystems — the default rollback journal works.
const DB_URI =
  process.platform === "win32" ? DB_PATH : `file:${DB_PATH}?vfs=unix-dotfile`;

// Attempt one DB open+init. Returns the db or throws. Caller retries.
function tryOpenDb(): DatabaseSync {
  const d = new DatabaseSync(DB_URI);
  try {
    d.exec("PRAGMA busy_timeout=5000;");
    d.exec(CREATE_SQL);
    return d;
  } catch (e) {
    try { d.close(); } catch { /* ignore */ }
    throw e;
  }
}

// DB is opened in the background so the HTTP server (and its /health probe)
// starts immediately even while Azure Files holds a stale SMB lease on
// artifacts.db after a revision swap. DB routes return 503 until ready.
let _db: DatabaseSync | null = null;
let _dbError: string | null = null;

async function initDb(): Promise<void> {
  fs.mkdirSync(path.dirname(DB_PATH), { recursive: true });
  for (let attempt = 1; ; attempt++) {
    try {
      _db = tryOpenDb();
      _dbError = null;
      console.log(`[artifact-service] artifacts.db ready (attempt ${attempt})`);
      return;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      _dbError = msg;
      if (!/locked|busy/i.test(msg)) {
        console.error(`[artifact-service] openDb fatal (non-lock) error: ${msg}`);
      }
      console.warn(`[artifact-service] openDb attempt ${attempt} failed: ${msg}; retrying in 3s…`);
      // A crashed predecessor can leave a stale dot-file lock (<db>.lock)
      // behind. Clear it periodically so a single crash can't wedge the
      // service forever.
      if (attempt % 10 === 0) {
        try {
          fs.rmSync(DB_PATH + ".lock", { recursive: true, force: true });
          console.warn(`[artifact-service] cleared stale lock file (attempt ${attempt})`);
        } catch { /* none present or already gone */ }
      }
      await sleep(3000);
    }
  }
}

// Throws until the background init has connected. Route handlers catch this
// and translate it to 503 so the service never crashes on a locked DB.
function db(): DatabaseSync {
  if (!_db) throw new Error(`DB not ready${_dbError ? ` (last error: ${_dbError})` : ""}`);
  return _db;
}

function json(res: http.ServerResponse, code: number, body: unknown) {
  const b = JSON.stringify(body);
  res.writeHead(code, { "Content-Type": "application/json" });
  res.end(b);
}

function readBody(req: http.IncomingMessage): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString("utf8")));
      } catch (e) {
        reject(e);
      }
    });
  });
}

// ── Handlers ──────────────────────────────────────────────────────────────

function listArtifacts(userId: string, isAdmin: boolean): ArtifactRow[] {
  if (isAdmin) {
    return db().prepare("SELECT * FROM artifact ORDER BY category, subject, created_at DESC").all() as unknown as ArtifactRow[];
  }
  return db().prepare("SELECT * FROM artifact WHERE user_id = ? ORDER BY category, subject, created_at DESC").all(userId) as unknown as ArtifactRow[];
}

function saveArtifact(body: { userId: string; category: string; subject: string; title: string; mimeType: string; url: string; tags?: string | string[] }): ArtifactRow {
  const id = `art-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  const now = new Date().toISOString();
  // tags arrives as a JSON string or (from older clients) a string[] — normalize
  // to a single JSON string so node:sqlite can bind it.
  const tags = Array.isArray(body.tags) ? JSON.stringify(body.tags) : (body.tags ?? null);
  db().prepare(
    "INSERT INTO artifact (id, user_id, category, subject, title, mime_type, url, created_at, tags) VALUES (?,?,?,?,?,?,?,?,?)"
  ).run(id, body.userId, body.category, body.subject, body.title, body.mimeType, body.url, now, tags);
  return db().prepare("SELECT * FROM artifact WHERE id = ?").get(id) as unknown as ArtifactRow;
}

function deleteArtifact(id: string, userId: string, isAdmin: boolean): boolean {
  if (isAdmin) {
    const r = db().prepare("DELETE FROM artifact WHERE id = ?").run(id);
    return r.changes > 0;
  }
  const r = db().prepare("DELETE FROM artifact WHERE id = ? AND user_id = ?").run(id, userId);
  return r.changes > 0;
}

// ── Server ────────────────────────────────────────────────────────────────

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://127.0.0.1:${PORT}`);
  applyCors(req, res);

  // Preflight — answer before any route logic so the browser passes the check.
  if (req.method === "OPTIONS") {
    res.writeHead(204).end();
    return;
  }

  // Health
  if (req.method === "GET" && url.pathname === "/health") {
    return json(res, 200, { ok: true, dbReady: _db !== null });
  }

  // Header: X-User-Id (required for all artifact ops), X-User-Role (admin|user)
  const userId = req.headers["x-user-id"] as string | undefined;
  const isAdmin = (req.headers["x-user-role"] as string | undefined) === "admin";

  if (!userId && url.pathname !== "/health") {
    return json(res, 401, { error: "X-User-Id header required" });
  }

  // DB-backed routes return 503 until the background init has connected, so a
  // transient Azure Files lock after a revision swap never crashes the replica.
  const needsDb =
    url.pathname === "/artifacts" || url.pathname.startsWith("/artifacts/");
  if (needsDb && !_db) {
    return json(res, 503, { error: "artifact store not ready", detail: _dbError });
  }

  // GET /artifacts — list (admin sees all, user sees own)
  if (req.method === "GET" && url.pathname === "/artifacts") {
    return json(res, 200, { artifacts: listArtifacts(userId!, isAdmin) });
  }

  // POST /artifacts — save
  if (req.method === "POST" && url.pathname === "/artifacts") {
    const body = (await readBody(req)) as { userId?: string; category?: string; subject?: string; title?: string; mimeType?: string; url?: string; tags?: string };
    if (!body.category || !body.subject || !body.title || !body.mimeType || !body.url) {
      return json(res, 400, { error: "category, subject, title, mimeType, url required" });
    }
    const saved = saveArtifact({
      userId: userId!,
      category: body.category,
      subject: body.subject,
      title: body.title,
      mimeType: body.mimeType,
      url: body.url,
      tags: body.tags,
    });
    return json(res, 201, saved);
  }

  // DELETE /artifacts/:id — discard (admin can delete any; user only own)
  if (req.method === "DELETE" && url.pathname.startsWith("/artifacts/")) {
    const id = url.pathname.slice("/artifacts/".length);
    const deleted = deleteArtifact(id, userId!, isAdmin);
    if (!deleted) return json(res, 404, { error: "not found or no permission" });
    return json(res, 200, { ok: true });
  }

  // POST /artifacts/upload — upload file content, store in files/, return URL
  if (req.method === "POST" && url.pathname === "/artifacts/upload") {
    const fileName = req.headers["x-file-name"] as string | undefined;
    const mimeType = req.headers["x-mime-type"] as string | undefined;
    if (!fileName || !mimeType) {
      return json(res, 400, { error: "X-File-Name and X-Mime-Type headers required" });
    }
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const content = Buffer.concat(chunks);
    const id = `file-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    const ext = path.extname(fileName);
    const storedName = `${id}${ext}`;
    const filePath = path.join(FILES_DIR, storedName);
    fs.writeFileSync(filePath, content);
    const url = `/files/${storedName}`;
    return json(res, 201, { url, bytes: content.length });
  }

  // GET /files/:name — serve uploaded file
  if (req.method === "GET" && url.pathname.startsWith("/files/")) {
    const name = url.pathname.slice("/files/".length);
    const filePath = path.join(FILES_DIR, name);
    if (!fs.existsSync(filePath)) return json(res, 404, { error: "not found" });
    const ext = path.extname(name);
    const mimeType = ext === ".html" ? "text/html" : ext === ".json" ? "application/json" : ext === ".md" ? "text/markdown" : "application/octet-stream";
    res.writeHead(200, { "Content-Type": mimeType });
    fs.createReadStream(filePath).pipe(res);
    return;
  }

  // GET /artifacts/:id — get one (for preview)
  if (req.method === "GET" && url.pathname.startsWith("/artifacts/")) {
    const id = url.pathname.slice("/artifacts/".length);
    const row = db().prepare("SELECT * FROM artifact WHERE id = ?").get(id) as unknown as ArtifactRow | undefined;
    if (!row) return json(res, 404, { error: "not found" });
    if (!isAdmin && row.user_id !== userId) return json(res, 403, { error: "no permission" });
    return json(res, 200, row);
  }

  // POST /refresh-sync — push tagged backbone CSVs to the refresh-daemon's
  // indicator_history (HMAC-authed /refresh/bootstrap). Admin-only: it's a
  // system verb, not a user op — the azure-foundry broker tool sends the
  // "admin" role header for exactly this one path. Body: { dryRun?: boolean }.
  if (req.method === "POST" && url.pathname === "/refresh-sync") {
    if (!isAdmin) return json(res, 403, { error: "refresh-sync requires admin role" });
    if (!_db) return json(res, 503, { error: "artifact store not ready", detail: _dbError });
    const body = (await readBody(req).catch(() => ({}))) as { dryRun?: boolean };
    const report = await syncIndicatorHistory(db(), FILES_DIR, { dryRun: body.dryRun === true, reason: "api" });
    return json(res, report.ok ? 200 : 502, report);
  }

  // POST /refresh-panel — deterministic export of indicator_history rows for
  // the azure orchestrator's read_indicator_panel tool. Admin-only (same gate
  // as /refresh-sync). Body: { subject?: string, series: string[] }. Proxies
  // the daemon's HMAC-signed export; never materializes SQL itself.
  if (req.method === "POST" && url.pathname === "/refresh-panel") {
    if (!isAdmin) return json(res, 403, { error: "refresh-panel requires admin role" });
    const body = (await readBody(req).catch(() => ({}))) as { subject?: string; series?: string[] };
    if (!Array.isArray(body.series) || body.series.length === 0) {
      return json(res, 400, { error: "series[] required" });
    }
    const payload = JSON.stringify({ subject: body.subject ?? null, series: body.series });
    const daemonRes = await fetch(`${REFRESH_DAEMON_URL}/refresh/export-panel`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Daemon-Sig": hmacSign(payload) },
      body: payload,
    });
    const text = await daemonRes.text();
    res.writeHead(daemonRes.status, { "Content-Type": "application/json" });
    res.end(text);
    return;
  }

  res.writeHead(404).end();
});

server.listen(PORT, () => {
  console.log(`[artifact-service] listening on :${PORT} (artifacts.db: ${DB_PATH})`);
});

// Kick off DB init in the background; do not block listen().
void initDb();

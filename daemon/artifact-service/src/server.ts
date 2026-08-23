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

const PORT = Number(process.env["PORT"] ?? 8793);
const DB_PATH = process.env["ARTIFACT_DB_PATH"] ?? path.join(process.cwd(), "data", "artifacts.db");
const FILES_DIR = process.env["ARTIFACT_FILES_DIR"] ?? path.join(process.cwd(), "data", "files");
fs.mkdirSync(FILES_DIR, { recursive: true });

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

function openDb(): DatabaseSync {
  fs.mkdirSync(path.dirname(DB_PATH), { recursive: true });
  const db = new DatabaseSync(DB_PATH);
  db.exec(`
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
  `);
  return db;
}

const db = openDb();

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
    return db.prepare("SELECT * FROM artifact ORDER BY category, subject, created_at DESC").all() as unknown as ArtifactRow[];
  }
  return db.prepare("SELECT * FROM artifact WHERE user_id = ? ORDER BY category, subject, created_at DESC").all(userId) as unknown as ArtifactRow[];
}

function saveArtifact(body: { userId: string; category: string; subject: string; title: string; mimeType: string; url: string; tags?: string }): ArtifactRow {
  const id = `art-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  const now = new Date().toISOString();
  db.prepare(
    "INSERT INTO artifact (id, user_id, category, subject, title, mime_type, url, created_at, tags) VALUES (?,?,?,?,?,?,?,?,?)"
  ).run(id, body.userId, body.category, body.subject, body.title, body.mimeType, body.url, now, body.tags ?? null);
  return db.prepare("SELECT * FROM artifact WHERE id = ?").get(id) as unknown as ArtifactRow;
}

function deleteArtifact(id: string, userId: string, isAdmin: boolean): boolean {
  if (isAdmin) {
    const r = db.prepare("DELETE FROM artifact WHERE id = ?").run(id);
    return r.changes > 0;
  }
  const r = db.prepare("DELETE FROM artifact WHERE id = ? AND user_id = ?").run(id, userId);
  return r.changes > 0;
}

// ── Server ────────────────────────────────────────────────────────────────

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://127.0.0.1:${PORT}`);

  // Health
  if (req.method === "GET" && url.pathname === "/health") {
    return json(res, 200, { ok: true });
  }

  // Header: X-User-Id (required for all artifact ops), X-User-Role (admin|user)
  const userId = req.headers["x-user-id"] as string | undefined;
  const isAdmin = (req.headers["x-user-role"] as string | undefined) === "admin";

  if (!userId && url.pathname !== "/health") {
    return json(res, 401, { error: "X-User-Id header required" });
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
    const row = db.prepare("SELECT * FROM artifact WHERE id = ?").get(id) as unknown as ArtifactRow | undefined;
    if (!row) return json(res, 404, { error: "not found" });
    if (!isAdmin && row.user_id !== userId) return json(res, 403, { error: "no permission" });
    return json(res, 200, row);
  }

  res.writeHead(404).end();
});

server.listen(PORT, () => {
  console.log(`[artifact-service] listening on :${PORT} (artifacts.db: ${DB_PATH})`);
});

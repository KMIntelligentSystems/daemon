// List what's inside the deployed airlock's sandbox.db (datasets + outbox).
// Downloads the db from the Azure Files share to a temp file, reads it with
// Node's built-in SQLite. Usage: node list-datasets.mjs
import { execSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join } from "node:path";

const local = join(tmpdir(), "sandbox-ro.db");
execSync(
  `az storage file download --account-name daemonstore --share-name airlock-data --path sandbox.db --dest "${local}" --only-show-errors`,
  { stdio: "inherit", shell: "bash" }
);

const db = new DatabaseSync(local, { readOnly: true });
console.log("\n=== datasets ===");
for (const r of db.prepare(
  "SELECT dataset_id, reference_month, target, source, substr(content_hash,1,12) AS hash, created_at FROM datasets ORDER BY created_at DESC"
).all()) {
  console.log(`${r.created_at}  ${r.dataset_id}\n    month=${r.reference_month} target=${r.target} source=${r.source} hash=${r.hash}…`);
}
console.log("\n=== broadcast_outbox ===");
for (const r of db.prepare(
  "SELECT broadcast_id, dataset_id, state, attempts, next_attempt_at, last_error FROM broadcast_outbox ORDER BY created_at DESC"
).all()) {
  console.log(`${r.broadcast_id}  state=${r.state} attempts=${r.attempts} next=${r.next_attempt_at}${r.last_error ? `\n    last_error: ${r.last_error}` : ""}\n    dataset: ${r.dataset_id}`);
}
db.close();

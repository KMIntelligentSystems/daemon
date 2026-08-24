// Pull a stored dataset from the deployed airlock.
// Usage: node pull-dataset.mjs <datasetId> [--raw]
// Reads DAEMON_HMAC_KEY from ../daemon/airlock/.env
import { readFileSync } from "node:fs";
import crypto from "node:crypto";

const id = process.argv[2];
if (!id) { console.error("usage: node pull-dataset.mjs <datasetId> [--raw]"); process.exit(1); }

const env = readFileSync(new URL("../daemon/airlock/.env", import.meta.url), "utf8");
const key = env.match(/^DAEMON_HMAC_KEY=(.*)$/m)[1].trim().replace(/^"|"$/g, "");
const sig = crypto.createHmac("sha256", key).update(id).digest("hex");

const base = process.env.AIRLOCK_URL ?? "https://daemon-airlock.salmondune-2ffd394f.eastus.azurecontainerapps.io";
const res = await fetch(`${base}/datasets/${id}`, { headers: { "X-Daemon-Sig": sig } });
if (!res.ok) { console.error(`HTTP ${res.status}: ${await res.text()}`); process.exit(1); }
const d = await res.json();

if (process.argv.includes("--raw")) { console.log(JSON.stringify(d, null, 2)); process.exit(0); }

console.log(`dataset ${d.datasetId} | month ${d.referenceMonth} | target ${d.target} | source ${d.source}`);
for (const s of d.indicators ?? []) {
  console.log(`  ${s.seriesId} [${s.unit ?? "?"}] — ${(s.observations ?? []).length} obs; latest:`);
  for (const o of (s.observations ?? []).slice(-3)) console.log(`    ${o.date} = ${o.value}`);
}

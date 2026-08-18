#!/usr/bin/env bash
# ACA Job entrypoint: emit a signed RunRequest and POST it to the airlock app.
# Same image as the app; the cron job just runs a different command.
#
#   job-entry.sh --source census --month auto [--series a,b] [--target t]
#
# DAEMON_HMAC_KEY must be set (emit-run-request signs with it).
# AIRLOCK_URL defaults to the app's in-environment name (internal ingress).
set -euo pipefail

AIRLOCK_URL="${AIRLOCK_URL:-http://damoen-airlock}"
export AIRLOCK_URL

BODY=$(daemon-airlock --config /app/config.toml emit-run-request "$@")

# node:22-bookworm-slim has no curl; Node's built-in fetch does the POST.
node -e '
const url = process.env.AIRLOCK_URL;
const body = process.argv[1];
fetch(url + "/run", { method: "POST", headers: { "content-type": "application/json" }, body })
  .then(async (r) => { console.log("[job]", r.status, await r.text()); process.exit(r.ok ? 0 : 1); })
  .catch((e) => { console.error("[job] POST failed:", e.message); process.exit(1); });
' "$BODY"

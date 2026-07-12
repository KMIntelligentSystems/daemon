# daemon — Airlock-and-Oracle (leading-indicator nowcasting service)

A **separate Railway service** that supplies monthly leading-indicator datasets to the main server, which uses them to boost its M3 forecasts. See [`../daemon-plan.md`](../daemon-plan.md) for the original design; this README reflects the refined flow.

## What it does

The daemon is **agentic** — a tool-using LLM lives inside it. On a **per-source schedule** (each data source releases at a different point in the month), the daemon is woken to provide that source's data for the current reference month. The LLM **drives**; the Rust airlock is a thin **capability broker** that executes the tools the agent asks for.

```
Railway cron (one entry per source release window)
        │  wakes the daemon for source X, reference month M
        ▼
┌───────────────────────────────────────────────┐
│ DAEMON (Railway service)                       │
│                                                │
│  ORACLE (Node) — the agent. Only LLM_API_KEY.  │
│    runs the model loop; on each tool_use it    │
│    sends a ToolCall to the airlock, feeds the  │
│    ToolResult back, until it calls `finish`.   │
│    No fs / db / shell / data-source keys.      │
│         ▲  ToolCall / ToolResult (stdio)  │    │
│         └───────────────────────────────┐ ▼    │
│  AIRLOCK (Rust) — capability broker.           │
│    executes a CLOSED tool set:                 │
│      fetch_series · read_prior_vintage ·       │
│      store_dataset · finish                    │
│    holds API keys, sandbox DB, host allowlist, │
│    HMAC key; validates every call; then hashes │
│    + HMAC-signs the dataset and broadcasts it.  │
└───────────────┬────────────────────────────────┘
                │ AvailabilityBroadcast (HMAC-signed)
                ▼
          MAIN SERVER  ──►  accept │ reject(reason)   (BroadcastResponse)
                │
                ▼ on accept
   Orchestrating agent (separate LLM, main server)
   PULLS the stored IndicatorDataset by datasetId
   and does the forecasting.
```

The daemon provides **data only** (values + provenance). Forecasting is the main-server orchestrator's job. The **tool catalog is the capability boundary**: a prompt-injected agent can only invoke allowlisted tools with allowlisted args — there is no tool for arbitrary network, fs, or shell.

## Layout

```
daemon/
  grammar/     contract-first JSON Schemas (source of truth)
    # external (main-server contract) — Phase 0
    indicator-dataset.schema.json      dataset payload (via store_dataset) → orchestrator pull
    availability-broadcast.schema.json daemon → main server (announcement + HMAC envelope)
    broadcast-response.schema.json     main server → daemon (accept | reject)
    # internal (tool bus, airlock ↔ oracle) — Phase 1
    tool-catalog.schema.json           closed tool set + per-tool arg schemas
    tool-call.schema.json              oracle → airlock  { callId, tool, args }
    tool-result.schema.json            airlock → oracle  { callId, ok, result | error }
    task-context.schema.json           airlock → oracle at spawn
  airlock/     Rust crate — trusted capability broker. Executes tools; holds keys, DB, HMAC.
  oracle/      Node package — untrusted agent. Runs the LLM tool-loop; only LLM_API_KEY.
```

## Delivery contract (assumptions — confirm)

- **Push-notify then pull.** Daemon POSTs an `AvailabilityBroadcast` to the main server; on `accept` the orchestrator pulls the full `IndicatorDataset` from the daemon by `datasetId` (HMAC-authed endpoint).
- **Reject → retain + retry.** A rejected broadcast leaves the dataset in the sandbox; the daemon retries on its next scheduled wake.
- **Sources (v1):** free/keyed only — Census (M3 EITS, construction), FRED, BLS. Paywalled sentiment (ISM, S&P Global PMI) **excluded**; regional Fed surveys act as the PMI proxy.
- **Targets (v1):** `m3_new_orders`, `m3_unfilled_orders`.

## Running the airlock service (v6)

The airlock is a long-lived HTTP service. A **signed `RunRequest`** POSTed to `/run` wakes one job (source + reference month). This is the same on Windows (dev/local-run) and Railway (prod) — only the *scheduler* that sends the request differs.

```sh
cd airlock

# 1. Long-lived service (waits for external RunRequests):
cargo run -- serve-http --port 8791

# 2. Same, but also run the built-in scheduler loop (self-POSTs signed
#    RunRequests per config [schedule], fast poll for a dev proof):
cargo run -- serve-http --port 8791 --schedule --poll-secs 10

# 3. Emit a signed RunRequest (what a scheduler / the main server does),
#    then POST it to a running service:
cargo run -- emit-run-request --source fred --month 2026-07 > req.json
curl -X POST -H "Content-Type: application/json" --data @req.json http://127.0.0.1:8791/run
```

Endpoints: `POST /run` (signed RunRequest → job outcome), `GET /health`.

### Reference month & publication lag

Leading indicators publish on a lag: on a mid-July run, the newest capacity-
utilization month available is June (and the M3 full report for June only lands
in early August). So the scheduler must not ask for the current calendar month.

- `emit-run-request --month auto` derives the reference month from the resolved
  series' `reference_lag_months` (config), e.g. `current − 1` for the G.17 lag.
  Pass `--as-of YYYY-MM-DD` to compute against a fixed date (for testing).
- If a wake fires before the data is out, the job **abstains** (HTTP 200, status
  `abstain`, nothing stored/broadcast) and the next scheduled run retries. This is
  the abstain guard: the requested reference month must appear in the fetched
  observations or the job is a clean no-op — it never labels stale data as current.

The full release calendar (each source's rule, lag, and confirmed 2026 dates)
lives in [`data/lookups/leading_indicators.json`](data/lookups/leading_indicators.json);
`config.toml` carries the operational `reference_lag_months` per series.

### Dev scheduling on Windows (Task Scheduler)

Run the service **without** `--schedule`, and let Windows Task Scheduler be the
external scheduler (the same shape as Railway cron in prod — an HTTP POST):

```powershell
# service only (no built-in loop):
cargo run -- serve-http --port 8791

# register one task per source on its publication day (+1 buffer):
powershell -File scripts/register-tasks.ps1          # -WhatIf to preview, -Unregister to remove
```

Each task runs [`scripts/run-source.ps1`](scripts/run-source.ps1), which emits a
signed RunRequest (`--month auto`) and POSTs it to `/run`, logging the outcome to
`airlock/logs/scheduler.log`. Only series wired in `config.toml` are enabled
(currently `fred_mcumfn`); the rest print as ready-to-enable commands.

**Scheduling is pluggable and outside the trust boundary** — the airlock verifies the HMAC and validates every field against config before trusting anything:
- **Prod (Railway):** a cron entry per source POSTs a signed `RunRequest`.
- **Dev/local (Windows):** the `--schedule` loop, or a **Task Scheduler** task that runs `emit-run-request` and POSTs the result. Either way it's just an HTTP POST.

A `RunRequest` can *select within* the airlock's config (source/series/model/budget) but can never *expand* it: unknown source/series/model → HTTP 422; a bad HMAC → 401; requested budget is clamped down to the config ceiling.

Secrets: `FRED_API_KEY` (and `DAEMON_HMAC_KEY` for a real deployment) come from Railway env in prod, or a repo-root `.env` on a Windows dev box (loaded via `dotenvy`). With no `DAEMON_HMAC_KEY` set, an insecure dev key is used and logged as a warning.

## Build phases

Tracked in `daemon-plan.md` (being revised for this flow). Phase 0 (this scaffold + grammar) is complete; Phase 1+ require a Rust toolchain (`cargo`, `rustc`) — not yet installed on the dev box. Railway deploy config is a separate, still-open workstream.

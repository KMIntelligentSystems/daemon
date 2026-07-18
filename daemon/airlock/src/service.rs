//! v6 — the long-lived HTTP service and its scheduler.
//!
//! The airlock boots once and serves. A signed `RunRequest` POSTed to `/run` is
//! the wake mechanism (replacing cron): the handler verifies the HMAC, validates
//! every field against config (SELECT WITHIN capability, never expand), then runs
//! one job and returns a structured outcome. The `--schedule` interval loop is an
//! *in-process* stand-in for an external scheduler (Railway cron / Windows Task
//! Scheduler): it builds + signs RunRequests and POSTs them to our own `/run`,
//! exercising the identical path.
//!
//! Job execution in Phase 1.5 is still the deterministic scripted sequence
//! (fetch → store → finish → broadcast); the real LLM oracle arrives in Phase 3.

use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveDate, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

use crate::config::{Config, ScheduleEntry};
use crate::crypto::{hmac_sha256_hex, hmac_sha256_verify};
use crate::grammar::*;
use crate::tools::{Session, Tools};
use rusqlite::Connection;

const KNOWN_TARGETS: &[&str] = &["m3_new_orders", "m3_unfilled_orders", "m3_shipments", "mfg_capacity"];

/// The reference month that becomes newly available when a request's series
/// publish, computed from each series' configured `reference_lag_months`:
/// `as_of_month - max(lag)`. The max is conservative — if a request bundles
/// series with different lags, we pick the month for which *all* are expected
/// to be out. Series not found / with no lag default to 0 (published in-month).
///
/// This is the fix for the old `Utc::now()` behaviour: on a July trigger for a
/// lag-1 source we request June, not the not-yet-published July.
pub fn reference_month_for(cfg: &Config, series: &[String], as_of: NaiveDate) -> String {
    let lag = series
        .iter()
        .filter_map(|s| cfg.series_by_id(s))
        .map(|s| s.reference_lag_months)
        .max()
        .unwrap_or(0) as i32;
    // Absolute month index (0-based month), shift back by lag, reformat.
    let idx = as_of.year() * 12 + as_of.month0() as i32 - lag;
    let year = idx.div_euclid(12);
    let month = (idx.rem_euclid(12) + 1) as u32;
    format!("{year:04}-{month:02}")
}

/// A RunRequest that has passed HMAC verification and config gating. Every field
/// here is guaranteed allowlisted / clamped — the job runner trusts it.
#[derive(Debug, Clone)]
pub struct ValidatedJob {
    pub source: String,
    pub reference_month: String,
    pub target: String,
    pub series: Vec<String>,
    pub model: String,
    pub max_tool_calls: u32,
    pub wall_clock_secs: u64,
}

/// Structured result returned from `/run` (and logged by the scheduler).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOutcome {
    pub status: String, // "stored" | "abstain" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub series_included: Vec<String>,
    pub note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<EnvelopeV2>,
}

impl JobOutcome {
    fn abstain(note: impl Into<String>) -> Self {
        JobOutcome { status: "abstain".into(), dataset_id: None, content_hash: None, series_included: vec![], note: note.into(), broadcast: None }
    }
    fn error(note: impl Into<String>) -> Self {
        JobOutcome { status: "error".into(), dataset_id: None, content_hash: None, series_included: vec![], note: note.into(), broadcast: None }
    }
}

/// A config-gating rejection: HTTP status + human reason.
struct Reject {
    code: u16,
    reason: String,
}
fn reject(code: u16, reason: impl Into<String>) -> Reject {
    Reject { code, reason: reason.into() }
}

// ---- config gating (the security heart of Phase 1.5) ---------------------

/// Validate a RunRequest body against config. Returns a ValidatedJob or a typed
/// rejection. A request may select within the airlock's capabilities; it can
/// never expand them: unknown source/series/target/model → reject; budget → clamp.
fn validate(cfg: &Config, req: &RunRequestBody) -> std::result::Result<ValidatedJob, Reject> {
    // source must be configured.
    let source_cfg = cfg
        .sources
        .get(&req.source)
        .ok_or_else(|| reject(422, format!("source not configured: {}", req.source)))?;

    // targets must be known contract values.
    for t in &req.targets {
        if !KNOWN_TARGETS.contains(&t.as_str()) {
            return Err(reject(400, format!("unknown target: {t}")));
        }
    }

    // resolve series: requested subset, else the source's configured default set.
    let series = match &req.series {
        Some(list) if !list.is_empty() => list.clone(),
        _ => source_cfg.default_series.clone(),
    };
    if series.is_empty() {
        return Err(reject(
            422,
            format!("no series to fetch for source {} (none requested, no default_series configured)", req.source),
        ));
    }
    // every series must be allowlisted AND belong to this source.
    for s in &series {
        match cfg.series_by_id(s) {
            None => return Err(reject(422, format!("series not allowlisted: {s}"))),
            Some(scfg) if scfg.source != req.source => {
                return Err(reject(422, format!("series {s} belongs to source {}, not {}", scfg.source, req.source)))
            }
            Some(_) => {}
        }
    }

    // model: gated against the allowlist; config default if absent.
    let model = match &req.model {
        Some(m) => {
            if !cfg.model_allowed(m) {
                return Err(reject(422, format!("model not allowlisted: {m}")));
            }
            m.clone()
        }
        None => cfg.models.default.clone(),
    };

    // budget: clamp each field down to the config ceiling.
    let mut max_tool_calls = cfg.budget.max_tool_calls;
    let mut wall_clock_secs = cfg.budget.wall_clock_secs;
    if let Some(b) = &req.budget {
        max_tool_calls = max_tool_calls.min(b.max_tool_calls);
        wall_clock_secs = wall_clock_secs.min(b.wall_clock_secs);
    }

    Ok(ValidatedJob {
        source: req.source.clone(),
        reference_month: req.reference_month.clone(),
        target: req.targets[0].clone(),
        series,
        model,
        max_tool_calls,
        wall_clock_secs,
    })
}

// ---- job runner (scripted; the real oracle lands in Phase 3) -------------

fn err_msg(res: &ToolResult) -> String {
    res.error
        .as_ref()
        .map(|e| format!("{}: {}", e.code, e.message))
        .unwrap_or_else(|| "unknown error".into())
}

/// Run one validated job end-to-end: fetch each series, store one dataset for
/// the primary target, finish, and build the signed broadcast. Deterministic —
/// no LLM yet. A fetch/store failure abstains (nothing broadcast), matching the
/// "failed/missed wakes store nothing" scheduling contract.
pub fn run_job(cfg: Config, db_path: &str, hmac_key: Vec<u8>, job: &ValidatedJob) -> JobOutcome {
    let mut tools = match Tools::open(cfg, db_path, hmac_key, job.source.clone(), job.reference_month.clone()) {
        Ok(t) => t,
        Err(e) => return JobOutcome::error(format!("open failed: {e}")),
    };
    // Apply the (clamped) per-job budget as the loop ceiling.
    tools.config.budget.max_tool_calls = job.max_tool_calls;
    tools.config.budget.wall_clock_secs = job.wall_clock_secs;
    let mut session = Session::default();

    eprintln!(
        "[job] source={} month={} target={} series={:?} model={} budget(maxCalls={}, wallSecs={})",
        job.source, job.reference_month, job.target, job.series, job.model, job.max_tool_calls, job.wall_clock_secs
    );

    let mut indicators: Vec<serde_json::Value> = Vec::new();
    for (i, series) in job.series.iter().enumerate() {
        let call = ToolCall {
            schema_version: SCHEMA_VERSION,
            call_id: format!("call-fetch-{i}"),
            tool: "fetch_series".into(),
            args: serde_json::json!({ "seriesId": series }),
        };
        let (res, _) = tools.dispatch(&call, &mut session);
        if !res.ok {
            return JobOutcome::abstain(format!("fetch {series} failed: {}", err_msg(&res)));
        }
        let r = res.result.unwrap_or_default();

        // Abstain guard: the requested reference month must actually be present
        // in the fetched observations. Without this, a wake that fires before the
        // data is published stores stale observations under a false month label
        // and broadcasts them. Instead we treat "not yet published" as a clean
        // no-op — nothing stored, retried on the next scheduled trigger.
        let has_ref_month = r
            .get("observations")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter().any(|o| {
                    o.get("date")
                        .and_then(|d| d.as_str())
                        .is_some_and(|d| d.starts_with(&job.reference_month))
                })
            })
            .unwrap_or(false);
        if !has_ref_month {
            return JobOutcome::abstain(format!(
                "no observation for {} in series {} - data not published yet; will retry on next trigger",
                job.reference_month, series
            ));
        }

        indicators.push(serde_json::json!({
            "seriesId": series,
            "leadTimeMonths": r.get("leadTimeMonths").and_then(|v| v.as_f64()).unwrap_or(0.0),
            "unit": r.get("unit"),
            "seasonalAdjustment": r.get("seasonalAdjustment"),
            "observations": r.get("observations").cloned().unwrap_or(serde_json::json!([])),
        }));
    }

    let release_date = Utc::now().format("%Y-%m-%d").to_string();
    let store_call = ToolCall {
        schema_version: SCHEMA_VERSION,
        call_id: "call-store".into(),
        tool: "store_dataset".into(),
        args: serde_json::json!({
            "target": job.target,
            "referenceMonth": job.reference_month,
            "releaseDate": release_date,
            "indicators": indicators,
        }),
    };
    let (store_res, _) = tools.dispatch(&store_call, &mut session);
    if !store_res.ok {
        return JobOutcome::abstain(format!("store failed: {}", err_msg(&store_res)));
    }

    let finish_call = ToolCall {
        schema_version: SCHEMA_VERSION,
        call_id: "call-finish".into(),
        tool: "finish".into(),
        args: serde_json::json!({ "status": "stored", "note": "phase-1.5 run_job" }),
    };
    let (_finish_res, status) = tools.dispatch(&finish_call, &mut session);
    let status = status.unwrap_or_else(|| "abstain".into());

        if status == "stored" {
            if let Some(stored) = session.stored.clone() {
                return match tools.build_broadcast(&stored) {
                    Ok(bc) => {
                        // Durably enqueue for the dispatcher; never POST inline
                        // (an HTTP 200 alone is not acceptance — the dispatcher
                        // parses the BroadcastResponse).
                        if let Ok(main_url) = std::env::var("DAEMON_MAIN_URL") {
                            let target_url = format!("{main_url}/ui/api/daemon/broadcast");
                            if let Err(e) = tools.enqueue_outbox(&bc, &target_url) {
                                eprintln!("[job] outbox enqueue failed: {e}");
                            } else {
                                eprintln!("[job] BROADCAST enqueued dataset={} hash={}", stored.dataset_id, stored.content_hash);
                            }
                        } else {
                            eprintln!("[job] BROADCAST built but DAEMON_MAIN_URL unset; dataset={} not enqueued", stored.dataset_id);
                        }
                        JobOutcome {
                            status: "stored".into(),
                            dataset_id: Some(stored.dataset_id.clone()),
                            content_hash: Some(stored.content_hash.clone()),
                            series_included: stored.series_included.clone(),
                            note: "stored and broadcast enqueued".into(),
                            broadcast: Some(bc),
                        }
                    }
                    Err(e) => JobOutcome::error(format!("broadcast build failed: {e}")),
                };
            }
        }
    JobOutcome::abstain(format!("finished with status '{status}', nothing stored"))
}

// ---- signing helpers (also what an external scheduler/main-server does) ---

/// Build an unsigned RunRequest body.
pub fn make_body(
    source: &str,
    month: &str,
    targets: Vec<String>,
    series: Option<Vec<String>>,
    model: Option<String>,
) -> RunRequestBody {
    RunRequestBody {
        schema_version: SCHEMA_VERSION,
        request_id: format!("rr-{}", uuid::Uuid::new_v4()),
        source: source.to_string(),
        reference_month: month.to_string(),
        targets,
        series,
        model,
        budget: None,
        issued_at: Utc::now().to_rfc3339(),
    }
}

/// Sign a RunRequest body: HMAC-SHA256 over the canonical body JSON (fields in
/// declaration order), matching the BroadcastBody convention.
pub fn sign_body(hmac_key: &[u8], body: &RunRequestBody) -> RunRequest {
    let signable = serde_json::to_string(body).expect("serialize RunRequestBody");
    let value = hmac_sha256_hex(hmac_key, signable.as_bytes());
    RunRequest {
        body: body.clone(),
        signature: Signature { alg: "HMAC-SHA256".into(), value },
    }
}

// ---- HTTP server + request handling --------------------------------------

fn reject_json(request_id: &str, code: u16, reason: impl Into<String>) -> (u16, String) {
    (
        code,
        serde_json::json!({ "ok": false, "requestId": request_id, "reason": reason.into() }).to_string(),
    )
}

/// Simple JSON error without a requestId.
fn json_err(code: u16, reason: impl Into<String>) -> (u16, String) {
    (code, serde_json::json!({ "ok": false, "reason": reason.into() }).to_string())
}

/// HMAC-authed pull: verify the X-Daemon-Sig header = HMAC-SHA256(datasetId).
fn check_pull_auth(hmac_key: &[u8], dataset_id: &str, request: &tiny_http::Request) -> bool {
    let provided = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("x-daemon-sig"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    hmac_sha256_verify(hmac_key, dataset_id.as_bytes(), provided)
}

/// Pull a stored dataset by id.  Returns the full IndicatorDataset JSON.
fn handle_pull(db_path: &str, hmac_key: &[u8], dataset_id: &str, request: &tiny_http::Request) -> (u16, String) {
    if !check_pull_auth(hmac_key, dataset_id, request) {
        return json_err(401, "invalid or missing X-Daemon-Sig header");
    }
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => return json_err(500, format!("db open: {e}")),
    };
    let body: std::result::Result<String, rusqlite::Error> = db.query_row(
        "SELECT body FROM datasets WHERE dataset_id = ?1",
        [dataset_id],
        |r| r.get(0),
    );
    match body {
        Ok(b) => (200, b),
        Err(rusqlite::Error::QueryReturnedNoRows) => json_err(404, "dataset not found"),
        Err(e) => json_err(500, format!("db read: {e}")),
    }
}

/// Verify → validate → run. Returns (http_status, json_body).
fn handle_run(config_path: &str, db_path: &str, hmac_key: &[u8], raw: &str) -> (u16, String) {
    let req: RunRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => return reject_json("rr-unknown", 400, format!("invalid RunRequest JSON: {e}")),
    };
    let rid = req.body.request_id.clone();

    // Verify HMAC over the body BEFORE trusting any field.
    let signable = match serde_json::to_string(&req.body) {
        Ok(s) => s,
        Err(e) => return reject_json(&rid, 500, format!("serialize body: {e}")),
    };
    if !hmac_sha256_verify(hmac_key, signable.as_bytes(), &req.signature.value) {
        return reject_json(&rid, 401, "HMAC verification failed");
    }

    let cfg = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => return reject_json(&rid, 500, format!("config load: {e}")),
    };
    let job = match validate(&cfg, &req.body) {
        Ok(j) => j,
        Err(rej) => return reject_json(&rid, rej.code, rej.reason),
    };

    let outcome = run_job(cfg, db_path, hmac_key.to_vec(), &job);

    // Delivery is handled by the outbox dispatcher thread (serve_http), which
    // parses the BroadcastResponse. No inline POST here — run_job already
    // enqueued the envelope durably.

    let code = if outcome.status == "error" { 500 } else { 200 };
    let body = serde_json::to_string(&serde_json::json!({ "ok": true, "requestId": rid, "outcome": outcome }))
        .unwrap_or_else(|_| "{}".into());
    (code, body)
}

/// Run the long-lived HTTP service. Blocks, serving requests sequentially.
pub fn serve_http(
    config_path: String,
    db_path: String,
    hmac_key: Vec<u8>,
    port: u16,
    schedule: bool,
    poll_secs_override: Option<u64>,
) -> Result<()> {
    // Bind all interfaces so the service is reachable cross-container on
    // Railway (the host expects 0.0.0.0:$PORT). Endpoints remain HMAC-authed,
    // so the wider bind does not widen trust. The scheduler's self-POST to
    // 127.0.0.1 still resolves to this listener.
    let server = tiny_http::Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow!("failed to bind 0.0.0.0:{port}: {e}"))?;
    eprintln!("[airlock] serving on http://0.0.0.0:{port}  (POST /run, GET /health, GET /datasets/:id)");

    if schedule {
        let cfg = Config::load(&config_path)?;
        let key = hmac_key.clone();
        std::thread::spawn(move || scheduler_loop(cfg, key, port, poll_secs_override));
    }

    // Outbox dispatcher: durable delivery of broadcasts to the target daemon.
    // Survives target outages via capped exponential backoff + jitter; parses
    // the BroadcastResponse (HTTP 200 alone is not acceptance).
    {
        let db_path_disp = db_path.clone();
        std::thread::spawn(move || outbox_dispatcher_loop(db_path_disp));
    }

    let json_header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("valid header");

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let (code, body) = match (method, url.as_str()) {
            (tiny_http::Method::Get, "/health") => (200u16, "{\"ok\":true}".to_string()),
            (tiny_http::Method::Post, "/run") => {
                let mut raw = String::new();
                if let Err(e) = request.as_reader().read_to_string(&mut raw) {
                    (400u16, format!("{{\"ok\":false,\"reason\":\"read body: {e}\"}}"))
                } else {
                    handle_run(&config_path, &db_path, &hmac_key, &raw)
                }
            }
            (tiny_http::Method::Get, url) if url.starts_with("/datasets/") => {
                let ds_id = url.strip_prefix("/datasets/").unwrap_or("");
                if ds_id.is_empty() {
                    json_err(400, "missing dataset id in path")
                } else {
                    handle_pull(&db_path, &hmac_key, ds_id, &request)
                }
            }
            _ => (404u16, "{\"ok\":false,\"reason\":\"not found\"}".to_string()),
        };
        let resp = tiny_http::Response::from_string(body)
            .with_status_code(code)
            .with_header(json_header.clone());
        if let Err(e) = request.respond(resp) {
            eprintln!("[airlock] failed to send response: {e}");
        }
    }
    Ok(())
}

// ---- broadcast outbox dispatcher ------------------------------------------

/// Capped exponential backoff with jitter (seconds) for retryable deliveries.
fn backoff_secs(attempts: u32) -> u64 {
    let base = 1u64 << attempts.min(6); // 1,2,4,8,16,32,64
    let jitter = (uuid::Uuid::new_v4().as_u128() as u64) % 4;
    (base + jitter).min(3600)
}

/// Classify a `BroadcastResponse` decision into a terminal outbox state.
/// `accept` and `reject: duplicate` are accepted (idempotent delivery).
fn classify_decision(decision: &str, reason: Option<&str>) -> &'static str {
    match decision {
        "accept" => "accepted",
        "reject" => match reason {
            Some("duplicate") => "accepted",
            // Schema/signature/content failures won't succeed on retry.
            Some("bad_signature") | Some("schema_mismatch") | Some("unknown_series")
            | Some("content_hash_mismatch") | Some("out_of_window") => "rejected_terminal",
            // Transient target-side failure; retry later.
            Some("storage_error") | _ => "retryable",
        },
        _ => "retryable",
    }
}

/// Long-lived loop that drains the outbox. Owns its own SQLite connection
/// (rusqlite Connection is not Sync) and HTTP agent. HTTP 200 is not
/// acceptance — the dispatcher requires a parsed `accept` decision.
fn outbox_dispatcher_loop(db_path: String) {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirects(0)
        .build();
    eprintln!("[outbox] dispatcher started");
    loop {
        drain_outbox_once(&db_path, &agent);
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn drain_outbox_once(db_path: &str, agent: &ureq::Agent) {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => { eprintln!("[outbox] db open: {e}"); return; }
    };
    let now = Utc::now().to_rfc3339();
    // Claim up to 16 rows whose next attempt is due.
    let rows: Vec<(String, String)> = match db.prepare(
        "SELECT broadcast_id, envelope_json FROM broadcast_outbox
         WHERE state IN ('pending','retryable') AND next_attempt_at <= ?1
         ORDER BY next_attempt_at ASC LIMIT 16",
    ) {
        Ok(mut s) => s.query_map([now.clone()], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))
            .ok().into_iter().flatten().filter_map(|x| x.ok()).collect(),
        Err(e) => { eprintln!("[outbox] query: {e}"); return; }
    };
    for (broadcast_id, envelope_json) in rows {
        // Mark delivering (best-effort; the row is single-owner by broadcast_id).
        let _ = db.execute(
            "UPDATE broadcast_outbox SET state='delivering', updated_at=?1 WHERE broadcast_id=?2 AND state IN ('pending','retryable')",
            rusqlite::params![now, broadcast_id],
        );
        let outcome = deliver_one(agent, &envelope_json);
        let (state, attempts_inc, err) = match outcome {
            DeliveryOutcome::Accepted => ("accepted", 0, None),
            DeliveryOutcome::RejectedTerminal(r) => ("rejected_terminal", 0, Some(r)),
            DeliveryOutcome::Retryable(r) => ("retryable", 1, Some(r)),
        };
        let attempts: i64 = db.query_row(
            "SELECT attempts FROM broadcast_outbox WHERE broadcast_id=?1", [&broadcast_id], |r| r.get(0),
        ).unwrap_or(0);
        let new_attempts = (attempts + attempts_inc) as u32;
        let next = Utc::now() + chrono::Duration::seconds(backoff_secs(new_attempts) as i64);
        let _ = db.execute(
            "UPDATE broadcast_outbox SET state=?1, attempts=?2, next_attempt_at=?3, last_error=?4, updated_at=?5 WHERE broadcast_id=?6",
            rusqlite::params![state, new_attempts, next.to_rfc3339(), err, Utc::now().to_rfc3339(), broadcast_id],
        );
        eprintln!("[outbox] {} -> {}", broadcast_id, state);
    }
}

enum DeliveryOutcome {
    Accepted,
    RejectedTerminal(String),
    Retryable(String),
}

fn deliver_one(agent: &ureq::Agent, envelope_json: &str) -> DeliveryOutcome {
    // The envelope carries its target_url? No — target_url is per-row. We POST
    // to DAEMON_MAIN_URL/ui/api/daemon/broadcast. (target_url column is kept
    // for future per-row routing but the dispatcher uses the env var for now.)
    let main_url = match std::env::var("DAEMON_MAIN_URL") {
        Ok(u) => u,
        Err(_) => return DeliveryOutcome::Retryable("DAEMON_MAIN_URL unset".into()),
    };
    let url = format!("{main_url}/ui/api/daemon/broadcast");
    let resp = agent.post(&url).set("Content-Type", "application/json").send_string(envelope_json);
    match resp {
        Ok(r) => {
            let body = r.into_string().unwrap_or_default();
            // Parse BroadcastResponse {schemaVersion, broadcastId, decision, reason}
            let v: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => return DeliveryOutcome::Retryable(format!("unparseable response: {e}")),
            };
            let decision = v.get("decision").and_then(|d| d.as_str()).unwrap_or("");
            let reason = v.get("reason").and_then(|r| r.as_str()).map(String::from);
            match classify_decision(decision, reason.as_deref()) {
                "accepted" => DeliveryOutcome::Accepted,
                "rejected_terminal" => DeliveryOutcome::RejectedTerminal(reason.unwrap_or_default()),
                _ => DeliveryOutcome::Retryable(reason.unwrap_or_else(|| format!("decision={decision}")).into()),
            }
        }
        Err(ureq::Error::Status(_code, _)) => DeliveryOutcome::Retryable(format!("HTTP non-2xx")),
        Err(e) => DeliveryOutcome::Retryable(format!("transport: {e}")),
    }
}

fn body_for_entry(entry: &ScheduleEntry, month: &str) -> RunRequestBody {
    make_body(&entry.source, month, entry.targets.clone(), entry.series.clone(), entry.model.clone())
}

/// The series an entry resolves to: its explicit list, else the source default.
fn entry_series(cfg: &Config, entry: &ScheduleEntry) -> Vec<String> {
    match &entry.series {
        Some(list) if !list.is_empty() => list.clone(),
        _ => cfg
            .sources
            .get(&entry.source)
            .map(|s| s.default_series.clone())
            .unwrap_or_default(),
    }
}

/// Fire each schedule entry once per (source, month) by POSTing a signed
/// RunRequest to our own /run. Retries a failed POST on the next poll.
fn scheduler_loop(cfg: Config, hmac_key: Vec<u8>, port: u16, poll_secs_override: Option<u64>) {
    let poll = poll_secs_override.unwrap_or(cfg.schedule.poll_secs).max(1);
    let url = format!("http://127.0.0.1:{port}/run");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build();
    let mut fired: HashSet<(String, String)> = HashSet::new();

    eprintln!("[scheduler] loop started: poll={poll}s, {} entr{}", cfg.schedule.entries.len(), if cfg.schedule.entries.len() == 1 { "y" } else { "ies" });

    loop {
        let as_of = Utc::now().date_naive();
        for entry in &cfg.schedule.entries {
            // Per-entry reference month: current month minus the source's
            // publication lag, so we request data that is actually published.
            let month = reference_month_for(&cfg, &entry_series(&cfg, entry), as_of);
            let key = (entry.source.clone(), month.clone());
            if fired.contains(&key) {
                continue;
            }
            let signed = sign_body(&hmac_key, &body_for_entry(entry, &month));
            let payload = match serde_json::to_value(&signed) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[scheduler] serialize failed: {e}");
                    continue;
                }
            };
            match agent.post(&url).send_json(payload) {
                Ok(resp) => {
                    eprintln!("[scheduler] fired source={} month={} -> HTTP {}", entry.source, month, resp.status());
                    fired.insert(key);
                }
                // ureq treats non-2xx as Err(Status). A 4xx is a bad request that
                // won't succeed on retry, so mark it fired; 5xx/transport → retry.
                Err(ureq::Error::Status(code, _)) => {
                    eprintln!("[scheduler] source={} month={} rejected -> HTTP {code}", entry.source, month);
                    if code < 500 {
                        fired.insert(key);
                    }
                }
                Err(e) => eprintln!("[scheduler] POST for source={} month={} failed (will retry): {e}", entry.source, month),
            }
        }
        std::thread::sleep(Duration::from_secs(poll));
    }
}

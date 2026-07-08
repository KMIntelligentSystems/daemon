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
use chrono::Utc;
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

use crate::config::{Config, ScheduleEntry};
use crate::crypto::{hmac_sha256_hex, hmac_sha256_verify};
use crate::grammar::*;
use crate::tools::{Session, Tools};
use rusqlite::Connection;

const KNOWN_TARGETS: &[&str] = &["m3_new_orders", "m3_unfilled_orders"];

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
    pub broadcast: Option<AvailabilityBroadcast>,
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
                    eprintln!("[job] BROADCAST (would POST to main server) dataset={} hash={}", stored.dataset_id, stored.content_hash);
                    JobOutcome {
                        status: "stored".into(),
                        dataset_id: Some(stored.dataset_id.clone()),
                        content_hash: Some(stored.content_hash.clone()),
                        series_included: stored.series_included.clone(),
                        note: "stored and broadcast built".into(),
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
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow!("failed to bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("[airlock] serving on http://127.0.0.1:{port}  (POST /run, GET /health)");

    if schedule {
        let cfg = Config::load(&config_path)?;
        let key = hmac_key.clone();
        std::thread::spawn(move || scheduler_loop(cfg, key, port, poll_secs_override));
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

// ---- built-in scheduler (in-process stand-in for cron / Task Scheduler) --

fn body_for_entry(entry: &ScheduleEntry, month: &str) -> RunRequestBody {
    make_body(&entry.source, month, entry.targets.clone(), entry.series.clone(), entry.model.clone())
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
        let month = Utc::now().format("%Y-%m").to_string();
        for entry in &cfg.schedule.entries {
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

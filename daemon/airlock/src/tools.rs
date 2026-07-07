//! The capability broker. Every method here holds a capability the oracle does
//! not have (network + keys, DB write, HMAC). `dispatch` is the single entry
//! point the tool loop calls; it validates and routes one ToolCall.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::Deserialize;

use crate::config::Config;
use crate::crypto::{hmac_sha256_hex, sha256_hex};
use crate::grammar::*;

pub struct Tools {
    pub config: Config,
    pub db: Connection,
    pub http: ureq::Agent,
    pub hmac_key: Vec<u8>,
    pub source: String,        // the source this wake handles (from CLI)
    pub reference_month: String,
}

/// What survives across tool calls within one session.
#[derive(Default)]
pub struct Session {
    pub tool_calls: u32,
    pub stored: Option<StoredDataset>,
}

#[derive(Clone)]
pub struct StoredDataset {
    pub dataset_id: String,
    pub content_hash: String,
    pub target: String,
    pub reference_month: String,
    pub source: String,
    pub release_date: String,
    pub series_included: Vec<String>,
}

// FRED observations response
#[derive(Deserialize)]
struct FredResp {
    observations: Vec<FredObs>,
}
#[derive(Deserialize)]
struct FredObs {
    date: String,
    value: String,
}

impl Tools {
    pub fn open(config: Config, db_path: &str, hmac_key: Vec<u8>, source: String, reference_month: String) -> Result<Self> {
        let db = Connection::open(db_path).with_context(|| format!("opening db {db_path}"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS datasets (
                dataset_id     TEXT PRIMARY KEY,
                reference_month TEXT NOT NULL,
                target         TEXT NOT NULL,
                source         TEXT NOT NULL,
                content_hash   TEXT NOT NULL,
                body           TEXT NOT NULL,
                created_at     TEXT NOT NULL
             );",
        )?;
        let http = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .build();
        Ok(Tools { config, db, http, hmac_key, source, reference_month })
    }

    /// Single routing point for the tool loop. Returns a ToolResult always
    /// (errors become ok:false results, not process failures) plus an optional
    /// `finish` status so the caller can end the loop.
    pub fn dispatch(&mut self, call: &ToolCall, session: &mut Session) -> (ToolResult, Option<String>) {
        session.tool_calls += 1;
        if session.tool_calls > self.config.budget.max_tool_calls {
            return (
                ToolResult::err(&call.call_id, "budget_exceeded", "max_tool_calls reached"),
                Some("abstain".to_string()),
            );
        }
        match call.tool.as_str() {
            "fetch_series" => (self.tool_fetch_series(call), None),
            "read_prior_vintage" => (self.tool_read_prior_vintage(call), None),
            "store_dataset" => (self.tool_store_dataset(call, session), None),
            "finish" => {
                let status = call
                    .args
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("abstain")
                    .to_string();
                (
                    ToolResult::ok(&call.call_id, serde_json::json!({ "acknowledged": true })),
                    Some(status),
                )
            }
            other => (
                ToolResult::err(&call.call_id, "unknown_tool", format!("no such tool: {other}")),
                None,
            ),
        }
    }

    fn tool_fetch_series(&self, call: &ToolCall) -> ToolResult {
        let args: FetchSeriesArgs = match serde_json::from_value(call.args.clone()) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(&call.call_id, "invalid_args", e.to_string()),
        };

        // Capability gate 1: series must be configured (closed allowlist).
        let series = match self.config.series_by_id(&args.series_id) {
            Some(s) => s.clone(),
            None => {
                return ToolResult::err(
                    &call.call_id,
                    "series_not_allowed",
                    format!("series not in config: {}", args.series_id),
                )
            }
        };
        let source = match self.config.sources.get(&series.source) {
            Some(s) => s.clone(),
            None => {
                return ToolResult::err(
                    &call.call_id,
                    "series_not_allowed",
                    format!("unknown source: {}", series.source),
                )
            }
        };
        // Capability gate 2: host must be on the allowlist.
        if !self.config.host_allowed(&source.host) {
            return ToolResult::err(
                &call.call_id,
                "host_not_allowed",
                format!("host not allowed: {}", source.host),
            );
        }
        let api_key = match std::env::var(&source.api_key_env) {
            Ok(k) if !k.is_empty() => k,
            _ => {
                return ToolResult::err(
                    &call.call_id,
                    "fetch_failed",
                    format!("missing api key env {}", source.api_key_env),
                )
            }
        };

        // Only FRED wired in Phase 1.
        if series.source != "fred" {
            return ToolResult::err(&call.call_id, "fetch_failed", "only fred wired in Phase 1");
        }

        let mut req = self
            .http
            .get(&source.base_url)
            .query("series_id", &series.provider_series_id)
            .query("api_key", &api_key)
            .query("file_type", "json");
        if let Some(start) = &args.start {
            req = req.query("observation_start", start);
        }
        if let Some(end) = &args.end {
            req = req.query("observation_end", end);
        }

        let raw = match req.call() {
            Ok(resp) => match resp.into_string() {
                Ok(s) => s,
                Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
            },
            Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
        };
        let source_hash = sha256_hex(raw.as_bytes());

        let parsed: FredResp = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", format!("parse: {e}")),
        };

        let mut observations = Vec::new();
        for o in parsed.observations {
            if o.value == "." {
                continue; // FRED missing marker
            }
            let v: f64 = match o.value.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Range check against config bounds.
            if v < series.min_value || v > series.max_value {
                return ToolResult::err(
                    &call.call_id,
                    "range_check_failed",
                    format!("{} value {} out of [{}, {}]", args.series_id, v, series.min_value, series.max_value),
                );
            }
            observations.push(Observation { date: o.date, value: v, is_preliminary: None });
        }

        // Keep the most recent 6 for a compact dataset.
        let n = observations.len();
        if n > 6 {
            observations = observations.split_off(n - 6);
        }

        ToolResult::ok(
            &call.call_id,
            serde_json::json!({
                "seriesId": args.series_id,
                "unit": series.unit,
                "seasonalAdjustment": series.seasonal_adjustment,
                "leadTimeMonths": series.lead_time_months,
                "observations": observations,
                "provenance": { "sourceHost": source.host, "sourceHash": source_hash }
            }),
        )
    }

    fn tool_read_prior_vintage(&self, call: &ToolCall) -> ToolResult {
        let series_id = call.args.get("seriesId").and_then(|v| v.as_str()).unwrap_or("");
        let reference_month = call.args.get("referenceMonth").and_then(|v| v.as_str()).unwrap_or("");
        if self.config.series_by_id(series_id).is_none() {
            return ToolResult::err(&call.call_id, "series_not_allowed", format!("series not in config: {series_id}"));
        }
        // Return the most recent stored dataset body for this month, if any.
        let row: rusqlite::Result<String> = self.db.query_row(
            "SELECT body FROM datasets WHERE reference_month = ?1 ORDER BY created_at DESC LIMIT 1",
            [reference_month],
            |r| r.get(0),
        );
        match row {
            Ok(body) => {
                let val: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                ToolResult::ok(&call.call_id, serde_json::json!({ "found": true, "priorDataset": val }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                ToolResult::ok(&call.call_id, serde_json::json!({ "found": false }))
            }
            Err(e) => ToolResult::err(&call.call_id, "storage_error", e.to_string()),
        }
    }

    fn tool_store_dataset(&self, call: &ToolCall, session: &mut Session) -> ToolResult {
        let args: StoreDatasetArgs = match serde_json::from_value(call.args.clone()) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(&call.call_id, "invalid_args", e.to_string()),
        };

        // Every indicator series must be configured, and every value in range.
        for ind in &args.indicators {
            let scfg = match self.config.series_by_id(&ind.series_id) {
                Some(s) => s,
                None => {
                    return ToolResult::err(
                        &call.call_id,
                        "validation_failed",
                        format!("series not in config: {}", ind.series_id),
                    )
                }
            };
            for o in &ind.observations {
                if o.value < scfg.min_value || o.value > scfg.max_value {
                    return ToolResult::err(
                        &call.call_id,
                        "range_check_failed",
                        format!("{} value {} out of range", ind.series_id, o.value),
                    );
                }
            }
        }

        // The airlock fills the fields the agent must not forge.
        let now = Utc::now();
        let dataset_id = format!("ds-{}", uuid::Uuid::new_v4());
        let series_included: Vec<String> = args.indicators.iter().map(|i| i.series_id.clone()).collect();
        let dataset = IndicatorDataset {
            schema_version: SCHEMA_VERSION,
            dataset_id: dataset_id.clone(),
            reference_month: args.reference_month.clone(),
            target: args.target.clone(),
            source: self.source.clone(),
            release_date: args.release_date.clone(),
            as_of: now.to_rfc3339(),
            indicators: args.indicators.clone(),
            provenance: Provenance {
                fetched_at: now.to_rfc3339(),
                source_host: self
                    .config
                    .sources
                    .get(&self.source)
                    .map(|s| s.host.clone())
                    .unwrap_or_default(),
                source_hash: "0".repeat(64), // per-series hashes recorded at fetch; aggregate placeholder in Phase 1
            },
        };

        // Hash exactly the bytes we store, so the main server's post-pull
        // re-check matches regardless of any canonicalization scheme.
        let body = match serde_json::to_string(&dataset) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(&call.call_id, "storage_error", e.to_string()),
        };
        let content_hash = sha256_hex(body.as_bytes());

        if let Err(e) = self.db.execute(
            "INSERT INTO datasets (dataset_id, reference_month, target, source, content_hash, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                dataset_id,
                args.reference_month,
                args.target,
                self.source,
                content_hash,
                body,
                now.to_rfc3339()
            ],
        ) {
            return ToolResult::err(&call.call_id, "storage_error", e.to_string());
        }

        session.stored = Some(StoredDataset {
            dataset_id: dataset_id.clone(),
            content_hash: content_hash.clone(),
            target: args.target,
            reference_month: args.reference_month,
            source: self.source.clone(),
            release_date: args.release_date,
            series_included,
        });

        ToolResult::ok(
            &call.call_id,
            serde_json::json!({ "datasetId": dataset_id, "contentHash": content_hash }),
        )
    }

    /// Build + HMAC-sign the availability broadcast for a stored dataset.
    pub fn build_broadcast(&self, stored: &StoredDataset) -> Result<AvailabilityBroadcast> {
        let body = BroadcastBody {
            schema_version: SCHEMA_VERSION,
            broadcast_id: format!("bc-{}", uuid::Uuid::new_v4()),
            dataset_id: stored.dataset_id.clone(),
            reference_month: stored.reference_month.clone(),
            target: stored.target.clone(),
            source: stored.source.clone(),
            series_included: stored.series_included.clone(),
            release_date: stored.release_date.clone(),
            content_hash: stored.content_hash.clone(),
            emitted_at: Utc::now().to_rfc3339(),
        };
        let signable = serde_json::to_string(&body)?;
        let sig = hmac_sha256_hex(&self.hmac_key, signable.as_bytes());
        Ok(AvailabilityBroadcast {
            body,
            signature: Signature { alg: "HMAC-SHA256".to_string(), value: sig },
        })
    }
}

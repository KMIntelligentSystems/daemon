//! The capability broker. Every method here holds a capability the oracle does
//! not have (network + keys, DB write, HMAC). `dispatch` is the single entry
//! point the tool loop calls; it validates and routes one ToolCall.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::Deserialize;

use crate::config::{Config, SeriesCfg, Source};
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
             );
             CREATE TABLE IF NOT EXISTS broadcast_outbox (
                broadcast_id    TEXT PRIMARY KEY,
                dataset_id      TEXT NOT NULL,
                target_url      TEXT NOT NULL,
                envelope_json   TEXT NOT NULL,
                state           TEXT NOT NULL,
                attempts        INTEGER NOT NULL DEFAULT 0,
                next_attempt_at TEXT NOT NULL,
                last_error      TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
             );",
        )?;
        let http = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
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

    // -- fetch_series (gates + dispatch) -------------------------------------

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

        // Dispatch on source -- each has its own API shape.
        match series.source.as_str() {
            "fred" => self.fetch_fred(&series, &source, &api_key, &args, call),
            "census" => self.fetch_census(&series, &source, &api_key, &args, call),
            "bls" => self.fetch_bls(&series, &source, &api_key, &args, call),
            other => ToolResult::err(
                &call.call_id,
                "source_not_wired",
                format!("source '{}' not yet wired (available: fred, census, bls)", other),
            ),
        }
    }

    // -- FRED fetch (GET with series_id, file_type=json) ---------------------

    fn fetch_fred(
        &self,
        series: &SeriesCfg,
        source: &Source,
        api_key: &str,
        args: &FetchSeriesArgs,
        call: &ToolCall,
    ) -> ToolResult {
        let mut req = self
            .http
            .get(&source.base_url)
            .query("series_id", &series.provider_series_id)
            .query("api_key", api_key)
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
                continue;
            }
            let v: f64 = match o.value.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v < series.min_value || v > series.max_value {
                return ToolResult::err(
                    &call.call_id,
                    "range_check_failed",
                    format!("{} value {} out of [{}, {}]", args.series_id, v, series.min_value, series.max_value),
                );
            }
            observations.push(Observation { date: o.date, value: v, is_preliminary: None });
        }

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
                "provenance": { "sourceHost": source.host, "sourceHash": source_hash },
            }),
        )
    }

    // -- Census M3 EITS fetch (GET with category_code, data_type_code, seasonal_adj)

    fn fetch_census(
        &self,
        series: &SeriesCfg,
        source: &Source,
        api_key: &str,
        args: &FetchSeriesArgs,
        call: &ToolCall,
    ) -> ToolResult {
        // provider_series_id format: "category_code:data_type_code:seasonal_adj"
        // e.g. "MTM:NO:NSA" = Total Manufacturing, New Orders, Not Seasonally Adjusted
        let parts: Vec<&str> = series.provider_series_id.split(':').collect();
        if parts.len() != 3 {
            return ToolResult::err(
                &call.call_id,
                "fetch_failed",
                format!(
                    "invalid provider_series_id for census: {} (expected CATEGORY:DATA_TYPE:ADJ)",
                    series.provider_series_id
                ),
            );
        }
        let category_code = parts[0];
        let data_type_code = parts[1];
        let seasonal_adj = parts[2];

        // Derive years to request: from (reference_month year - 5) through reference_month year.
        let ref_year: i32 = self.reference_month[..4].parse().unwrap_or(2026);
        let years: Vec<String> = ((ref_year - 5)..=ref_year).map(|y| y.to_string()).collect();

        let mut req = self
            .http
            .get(&source.base_url)
            .query("get", "cell_value,time_slot_id")
            .query("category_code", category_code)
            .query("data_type_code", data_type_code)
            .query("seasonal_adj", seasonal_adj)
            .query("key", api_key);
        for y in &years {
            req = req.query("YEAR", y.as_str());
        }

        let raw = match req.call() {
            Ok(resp) => match resp.into_string() {
                Ok(s) => s,
                Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
            },
            Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
        };
        let source_hash = sha256_hex(raw.as_bytes());

        // Census returns a 2D JSON array: [[headers], [row0], [row1], ...]
        let rows: Vec<Vec<serde_json::Value>> = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::err(&call.call_id, "fetch_failed", format!("parse: {e}"));
            }
        };
        if rows.is_empty() {
            return ToolResult::err(&call.call_id, "fetch_failed", "empty census response");
        }
        let headers = &rows[0];
        let col_cell_value = headers.iter().position(|h| h.as_str() == Some("cell_value"));
        let col_time = headers.iter().position(|h| h.as_str() == Some("time_slot_id"));
        let (Some(ci), Some(ti)) = (col_cell_value, col_time) else {
            return ToolResult::err(
                &call.call_id,
                "fetch_failed",
                "missing expected columns (cell_value, time_slot_id) in census response",
            );
        };

        let mut observations = Vec::new();
        for row in rows.iter().skip(1) {
            let date = match row.get(ti).and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let val: f64 = match row.get(ci).and_then(|v| v.as_f64()) {
                Some(v) => v,
                None => match row.get(ci).and_then(|v| v.as_str()) {
                    Some(s) => match s.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    },
                    None => continue,
                },
            };
            if val < series.min_value || val > series.max_value {
                return ToolResult::err(
                    &call.call_id,
                    "range_check_failed",
                    format!("{} value {} out of [{}, {}]", args.series_id, val, series.min_value, series.max_value),
                );
            }
            observations.push(Observation { date, value: val, is_preliminary: None });
        }

        // Sort by date ascending (Census may not guarantee order).
        observations.sort_by(|a, b| a.date.cmp(&b.date));

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
                "provenance": { "sourceHost": source.host, "sourceHash": source_hash },
            }),
        )
    }

    // -- BLS fetch (POST with JSON body) -----------------------------------

    fn fetch_bls(
        &self,
        series: &SeriesCfg,
        source: &Source,
        api_key: &str,
        args: &FetchSeriesArgs,
        call: &ToolCall,
    ) -> ToolResult {
        // Derive year range from reference month.
        let ref_year: i32 = self.reference_month[..4].parse().unwrap_or(2026);
        let start_year = (ref_year - 5).to_string();
        let end_year = ref_year.to_string();

        let body = serde_json::json!({
            "seriesid": [&series.provider_series_id],
            "startyear": &start_year,
            "endyear": &end_year,
            "registrationkey": api_key,
        });

        let raw = match self.http.post(&source.base_url).send_json(body) {
            Ok(resp) => match resp.into_string() {
                Ok(s) => s,
                Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
            },
            Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", e.to_string()),
        };
        let source_hash = sha256_hex(raw.as_bytes());

        // Parse BLS v2 response
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(&call.call_id, "fetch_failed", format!("parse: {e}")),
        };

        // Check BLS status field
        let status = parsed.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status != "REQUEST_SUCCEEDED" {
            let msg = parsed
                .get("message")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("unknown BLS error");
            return ToolResult::err(&call.call_id, "fetch_failed", format!("BLS API: {msg}"));
        }

        // Navigate: Results.series[0].data[]
        let data = parsed
            .get("Results")
            .and_then(|r| r.get("series"))
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.get("data"))
            .and_then(|d| d.as_array());

        let data = match data {
            Some(d) => d,
            None => return ToolResult::err(&call.call_id, "fetch_failed", "no data array in BLS response"),
        };

        let mut observations = Vec::new();
        for entry in data {
            let year = entry.get("year").and_then(|v| v.as_str()).unwrap_or("");
            let period = entry.get("period").and_then(|v| v.as_str()).unwrap_or("");
            let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");

            // Convert period M01..M12 into YYYY-MM
            if !period.starts_with('M') || period.len() != 3 {
                continue;
            }
            let date = format!("{}-{}", year, &period[1..]);

            let val: f64 = match value.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };

            if val < series.min_value || val > series.max_value {
                return ToolResult::err(
                    &call.call_id,
                    "range_check_failed",
                    format!("{} value {} out of [{}, {}]", args.series_id, val, series.min_value, series.max_value),
                );
            }
            observations.push(Observation { date, value: val, is_preliminary: None });
        }

        // Sort by date ascending (BLS returns newest first by default).
        observations.sort_by(|a, b| a.date.cmp(&b.date));

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
                "provenance": { "sourceHost": source.host, "sourceHash": source_hash },
            }),
        )
    }

    // -- read_prior_vintage --------------------------------------------------

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

    // -- store_dataset -------------------------------------------------------

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
                source_hash: "0".repeat(64), // per-series hashes recorded at fetch; aggregate placeholder
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

    /// Build the envelope-v2 broadcast for a stored dataset (delegates to the
    /// free `build_envelope` so the fixture path can sign without a Tools).
    pub fn build_broadcast(&self, stored: &StoredDataset) -> Result<EnvelopeV2> {
        build_envelope(&self.hmac_key, stored)
    }

    /// Enqueue a built envelope into the durable broadcast outbox. The
    /// dispatcher thread (service.rs) later POSTs it to the target daemon and
    /// parses the BroadcastResponse. Storing the envelope durably means a
    /// target outage is retried after recovery without refetching.
    pub fn enqueue_outbox(&self, bc: &EnvelopeV2, target_url: &str) -> Result<()> {
        let body_bytes = crate::crypto::base64_decode(&bc.body_b64)
            .context("decoding envelope body for outbox")?;
        let body: BroadcastBody = serde_json::from_slice(&body_bytes)
            .context("parsing envelope body for outbox")?;
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT INTO broadcast_outbox
               (broadcast_id, dataset_id, target_url, envelope_json, state,
                attempts, next_attempt_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6, ?6)
             ON CONFLICT(broadcast_id) DO NOTHING",
            rusqlite::params![
                body.broadcast_id,
                body.dataset_id,
                target_url,
                serde_json::to_string(bc)?,
                now,
                now,
            ],
        ).context("insert outbox row")?;
        Ok(())
    }
}

/// Free-function envelope builder: serializes BroadcastBody to canonical JSON
/// (serde declaration order), base64-encodes those exact bytes, and HMACs the
/// raw bytes. Used by `build_broadcast` (via Tools) and by the fixture path
/// (no Tools instance needed).
pub fn build_envelope(hmac_key: &[u8], stored: &StoredDataset) -> Result<EnvelopeV2> {
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
    let body_bytes = serde_json::to_vec(&body)?; // canonical, compact, declaration order
    let sig = hmac_sha256_hex(hmac_key, &body_bytes);
    let key_id = std::env::var("DAEMON_KEY_ID").unwrap_or_else(|_| "daemon-dev-1".to_string());
    Ok(EnvelopeV2 {
        schema_version: 2,
        body_b64: crate::crypto::base64_encode(&body_bytes),
        signature: SignatureV2 {
            alg: "HMAC-SHA256".to_string(),
            key_id,
            value: sig,
        },
    })
}
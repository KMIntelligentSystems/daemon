//! serde structs mirroring the JSON Schemas in daemon/grammar/.
//! All emit camelCase JSON to match the contracts.

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

// ---- IndicatorDataset (external contract) --------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub date: String,
    pub value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_preliminary: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Indicator {
    pub series_id: String,
    pub lead_time_months: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seasonal_adjustment: Option<String>,
    pub observations: Vec<Observation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub fetched_at: String,
    pub source_host: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorDataset {
    pub schema_version: u32,
    pub dataset_id: String,
    pub reference_month: String,
    pub target: String,
    pub source: String,
    pub release_date: String,
    pub as_of: String,
    pub indicators: Vec<Indicator>,
    pub provenance: Provenance,
}

// ---- store_dataset tool args (agent-supplied draft; no provenance) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreDatasetArgs {
    pub target: String,
    pub reference_month: String,
    pub release_date: String,
    pub indicators: Vec<Indicator>,
}

// ---- fetch_series tool args ----------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchSeriesArgs {
    pub series_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vintage: Option<String>,
}

// ---- AvailabilityBroadcast (external contract) ---------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    pub alg: String,
    pub value: String,
}

/// The signable body: every broadcast field except the signature itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadcastBody {
    pub schema_version: u32,
    pub broadcast_id: String,
    pub dataset_id: String,
    pub reference_month: String,
    pub target: String,
    pub source: String,
    pub series_included: Vec<String>,
    pub release_date: String,
    pub content_hash: String,
    pub emitted_at: String,
}

// ---- Envelope v2 (sign-what-you-send) -----------------------------------
// Replaces the flattened v1 AvailabilityBroadcast. The signed bytes are
// transported verbatim as bodyB64 (base64 of the canonical BroadcastBody
// JSON the daemon serialized). The verifier base64-decodes, HMACs those
// exact bytes, and compares in constant time. No cross-language JSON field
// ordering assumptions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeV2 {
    pub schema_version: u32, // 2
    pub body_b64: String,    // base64(utf8(serde_json::to_string(&BroadcastBody)))
    pub signature: SignatureV2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureV2 {
    pub alg: String,   // "HMAC-SHA256" (Ed25519 in a later hardening phase)
    pub key_id: String, // rotation support; resolves to a key in the verifier's trust set
    pub value: String,  // hex HMAC-SHA256 over the DECODED body_b64 bytes
}

// ---- Legacy v1 (superseded by EnvelopeV2; `Signature` is still used by RunRequest) ----

// ---- RunRequest (inbound control contract, v6) ---------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetReq {
    pub max_tool_calls: u32,
    pub wall_clock_secs: u64,
}

/// The signable body: every RunRequest field except the signature itself.
/// HMAC is computed over `serde_json::to_string(&RunRequestBody)` — the fields
/// in declaration order — matching the BroadcastBody convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequestBody {
    pub schema_version: u32,
    pub request_id: String,
    pub source: String,
    pub reference_month: String,
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetReq>,
    pub issued_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    #[serde(flatten)]
    pub body: RunRequestBody,
    pub signature: Signature,
}

// ---- TaskContext (internal contract, airlock → oracle at spawn) ------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetCtx {
    pub max_tool_calls: u32,
    pub wall_clock_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContext {
    pub schema_version: u32,
    pub session_id: String,
    pub source: String,
    pub reference_month: String,
    pub goal: String,
    pub model: String,
    pub series: Vec<String>,
    pub budget: BudgetCtx,
}

// ---- Tool bus (internal contract) ----------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub schema_version: u32,
    pub call_id: String,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub schema_version: u32,
    pub call_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
}

impl ToolResult {
    pub fn ok(call_id: &str, result: serde_json::Value) -> Self {
        ToolResult {
            schema_version: SCHEMA_VERSION,
            call_id: call_id.to_string(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(call_id: &str, code: &str, message: impl Into<String>) -> Self {
        ToolResult {
            schema_version: SCHEMA_VERSION,
            call_id: call_id.to_string(),
            ok: false,
            result: None,
            error: Some(ToolError {
                code: code.to_string(),
                message: message.into(),
            }),
        }
    }
}

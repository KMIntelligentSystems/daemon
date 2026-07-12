//! daemon-airlock — capability broker for the agentic nowcasting daemon (Phase 2).
//!
//! Subcommands:
//!   serve       run the stdio tool loop (oracle spawned externally — Phase 2 testing)
//!   run-oracle  spawn the Node oracle and run the tool loop (production flow)
//!   scripted    drive the loop with a canned agent sequence (Phase 1 proof, no oracle)
//!   serve-http  long-lived HTTP service (POST /run wakes a job)
//!   emit-run-request  build + HMAC-sign a RunRequest

mod config;
mod crypto;
mod grammar;
mod lockdown;
mod service;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use config::Config;
use grammar::*;
use tools::{Session, Tools};

#[derive(Parser)]
#[command(name = "daemon-airlock", version, about = "Airlock capability broker")]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: String,
    #[arg(long, default_value = "sandbox.db")]
    db: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read ToolCall lines from stdin, write ToolResult lines to stdout.
    /// Optionally reads a TaskContext as the first line (handshake).
    Serve {
        #[arg(long)]
        source: String,
        #[arg(long)]
        month: String,
    },
    /// Spawn the Node oracle and run the tool loop over child stdin/stdout.
    /// This is the production flow: airlock owns the oracle lifecycle.
    RunOracle {
        #[arg(long)]
        source: String,
        #[arg(long)]
        month: String,
        #[arg(long, default_value = "node")]
        node_bin: String,
        #[arg(long, default_value = "../oracle/dist/main.js")]
        oracle_script: String,
    },
    /// Simulate the agent: fetch one series, store it, broadcast. Real fetch.
    Scripted {
        #[arg(long, default_value = "fred")]
        source: String,
        #[arg(long)]
        month: String,
        #[arg(long, default_value = "fred_mcumfn")]
        series: String,
        #[arg(long, default_value = "m3_new_orders")]
        target: String,
    },
    /// Long-lived HTTP service. POST a signed RunRequest to /run to wake a job.
    ServeHttp {
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Also run the built-in scheduler loop (self-POSTs signed RunRequests).
        #[arg(long)]
        schedule: bool,
        /// Override [schedule].poll_secs (handy for a fast dev proof).
        #[arg(long)]
        poll_secs: Option<u64>,
    },
    /// Build + HMAC-sign a RunRequest and print it (what a scheduler/main-server does).
    EmitRunRequest {
        #[arg(long, default_value = "fred")]
        source: String,
        /// Reference month "YYYY-MM", or "auto" to derive it from the resolved
        /// series' publication lag (config reference_lag_months) at --as-of.
        #[arg(long)]
        month: String,
        /// Override "today" for `--month auto` (YYYY-MM-DD). Defaults to now.
        #[arg(long)]
        as_of: Option<String>,
        /// Comma-separated seriesIds; omit to let the airlock use the source default.
        #[arg(long)]
        series: Option<String>,
        #[arg(long, default_value = "m3_new_orders")]
        target: String,
        #[arg(long)]
        model: Option<String>,
        /// Corrupt the signature (to prove /run rejects a bad HMAC with 401).
        #[arg(long)]
        tamper: bool,
    },
}

fn load_hmac_key() -> Vec<u8> {
    match std::env::var("DAEMON_HMAC_KEY") {
        Ok(k) if !k.is_empty() => k.into_bytes(),
        _ => {
            eprintln!("[airlock] WARN: DAEMON_HMAC_KEY not set — using an insecure dev key");
            b"dev-insecure-hmac-key-change-me".to_vec()
        }
    }
}

fn main() -> Result<()> {
    // Load .env (searches cwd and parent dirs) so FRED_API_KEY is available.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    let hmac_key = load_hmac_key();

    match cli.cmd {
        Cmd::Serve { source, month } => run_serve(cfg, &cli.db, hmac_key, source, month),
        Cmd::RunOracle { source, month, node_bin, oracle_script } => {
            run_oracle(cfg, &cli.db, hmac_key, source, month, node_bin, oracle_script)
        }
        Cmd::Scripted { source, month, series, target } => {
            run_scripted(cfg, &cli.db, hmac_key, source, month, series, target)
        }
        Cmd::ServeHttp { port, schedule, poll_secs } => {
            // cfg was loaded above to validate config at boot; the service reloads
            // per request (picks up edits, avoids sharing a DB handle across calls).
            drop(cfg);
            service::serve_http(cli.config, cli.db, hmac_key, port, schedule, poll_secs)
        }
        Cmd::EmitRunRequest { source, month, as_of, series, target, model, tamper } => {
            run_emit(cfg, hmac_key, source, month, as_of, series, target, model, tamper)
        }
    }
}

/// Build + sign a RunRequest and print it to stdout.
fn run_emit(
    cfg: Config,
    hmac_key: Vec<u8>,
    source: String,
    month: String,
    as_of: Option<String>,
    series: Option<String>,
    target: String,
    model: Option<String>,
    tamper: bool,
) -> Result<()> {
    let series_list = series.map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>());

    // "auto" → derive the reference month from the resolved series' publication
    // lag, so a scheduler/Task Scheduler task never has to hardcode the month.
    let month = if month == "auto" {
        let resolved = match &series_list {
            Some(l) if !l.is_empty() => l.clone(),
            _ => cfg
                .sources
                .get(&source)
                .map(|s| s.default_series.clone())
                .ok_or_else(|| anyhow::anyhow!("unknown source '{source}' — cannot resolve --month auto"))?,
        };
        let as_of_date = match as_of {
            Some(s) => chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .with_context(|| format!("parsing --as-of '{s}' (want YYYY-MM-DD)"))?,
            None => chrono::Utc::now().date_naive(),
        };
        service::reference_month_for(&cfg, &resolved, as_of_date)
    } else {
        month
    };

    let body = service::make_body(&source, &month, vec![target], series_list, model);
    let mut signed = service::sign_body(&hmac_key, &body);
    if tamper {
        signed.signature.value = "0".repeat(64);
    }
    println!("{}", serde_json::to_string(&signed)?);
    Ok(())
}

/// stdio tool loop.  Optionally reads a TaskContext as the first input line
/// (handshake); if the first line is not valid TaskContext JSON it is treated
/// as the first ToolCall (backward-compatible with direct invocation).
fn run_serve(cfg: Config, db: &str, hmac_key: Vec<u8>, source: String, month: String) -> Result<()> {
    let mut tools = Tools::open(cfg, db, hmac_key, source, month)?;
    let mut session = Session::default();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut lines = stdin.lock().lines();

    // Optional TaskContext handshake — first non-empty line may be TaskContext.
    let mut first_call: Option<ToolCall> = None;
    for line in &mut lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Try TaskContext first; if it parses we've done the handshake and continue.
        if let Ok(ctx) = serde_json::from_str::<TaskContext>(&line) {
            eprintln!(
                "[airlock] serve handshake: session={} source={} month={} model={}",
                ctx.session_id, ctx.source, ctx.reference_month, ctx.model
            );
            // If source or month disagree with CLI args, the caller misconfigured.
            if ctx.source != tools.source {
                anyhow::bail!(
                    "TaskContext source '{}' != CLI source '{}'",
                    ctx.source, tools.source
                );
            }
            if ctx.reference_month != tools.reference_month {
                anyhow::bail!(
                    "TaskContext referenceMonth '{}' != CLI month '{}'",
                    ctx.reference_month, tools.reference_month
                );
            }
            // Apply budget from context (already clamped by the caller).
            tools.config.budget.max_tool_calls = ctx.budget.max_tool_calls;
            tools.config.budget.wall_clock_secs = ctx.budget.wall_clock_secs;
            continue; // next line should be a ToolCall
        }
        // Not TaskContext — treat as the first ToolCall.
        match serde_json::from_str::<ToolCall>(&line) {
            Ok(c) => { first_call = Some(c); break; }
            Err(e) => {
                let res = ToolResult::err("call-unknown", "invalid_args", format!("bad ToolCall: {e}"));
                writeln!(out, "{}", serde_json::to_string(&res)?)?;
                out.flush()?;
                continue;
            }
        }
    }

    // Process the deferred first call, then the rest of the lines.
    let all_calls = first_call.into_iter().chain(lines.filter_map(|l| {
        let l = l.ok()?;
        if l.trim().is_empty() { None } else { serde_json::from_str(&l).ok() }
    }));

    for call in all_calls {
        let (result, finish) = tools.dispatch(&call, &mut session);
        writeln!(out, "{}", serde_json::to_string(&result)?)?;
        out.flush()?;
        if let Some(status) = finish {
            emit_broadcast_if_stored(&tools, &session, status, &mut out)?;
            break;
        }
    }
    Ok(())
}

/// Spawn the Node oracle, send TaskContext, then run the tool loop over
/// child stdin/stdout.  The airlock owns the oracle's lifecycle — this is
/// the production flow.
fn run_oracle(
    cfg: Config,
    db: &str,
    hmac_key: Vec<u8>,
    source: String,
    month: String,
    node_bin: String,
    oracle_script: String,
) -> Result<()> {
    let mut tools = Tools::open(cfg, db, hmac_key, source.clone(), month.clone())?;
    let mut session = Session::default();

    // Build TaskContext — the handshake payload the oracle receives on argv.
    // Include the allowed series for this source so the LLM knows what to fetch.
    let allowed_series: Vec<String> = tools
        .config
        .series
        .iter()
        .filter(|s| s.source == source)
        .map(|s| s.id.clone())
        .collect();
    let ctx = TaskContext {
        schema_version: SCHEMA_VERSION,
        session_id: format!("sess-{}", uuid::Uuid::new_v4()),
        source: source.clone(),
        reference_month: month.clone(),
        goal: format!("Assemble {source} leading indicators for {month}"),
        model: tools.config.models.default.clone(),
        series: allowed_series.clone(),
        budget: BudgetCtx {
            max_tool_calls: tools.config.budget.max_tool_calls,
            wall_clock_secs: tools.config.budget.wall_clock_secs,
        },
    };
    let ctx_json = serde_json::to_string(&ctx)?;

    eprintln!(
        "[airlock] spawning oracle: {} {} '<ctx>'",
        node_bin, oracle_script
    );
    eprintln!(
        "[airlock] session={} source={} month={} model={}",
        ctx.session_id, ctx.source, ctx.reference_month, ctx.model
    );

    let mut child_cmd = Command::new(&node_bin);
    child_cmd
        .arg(&oracle_script)
        .arg(&ctx_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    // Phase 4: strip env, set ulimits, chroot before spawn.
    lockdown::harden_child(&mut child_cmd);

    let mut child = child_cmd
        .spawn()
        .with_context(|| format!("spawning {} {}", node_bin, oracle_script))?;

    let child_stdout = child.stdout.take().context("child stdout")?;
    let child_stdin = child.stdin.take().context("child stdin")?;
    let reader = std::io::BufReader::new(child_stdout);
    let mut writer = child_stdin;

    // Tool loop: read ToolCall lines from oracle stdout, write ToolResult
    // lines to oracle stdin — identical protocol to run_serve but over child pipes.
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let call: ToolCall = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                let res = ToolResult::err("call-unknown", "invalid_args", format!("bad ToolCall: {e}"));
                writeln!(writer, "{}", serde_json::to_string(&res)?)?;
                writer.flush()?;
                continue;
            }
        };
        eprintln!("  → ToolCall  {}", serde_json::to_string(&call)?);
        let (result, finish) = tools.dispatch(&call, &mut session);
        eprintln!("  ← ToolResult {}", serde_json::to_string(&result)?);
        writeln!(writer, "{}", serde_json::to_string(&result)?)?;
        writer.flush()?;
        if let Some(status) = finish {
            // Drop writer so the child sees EOF on stdin, then emit broadcast.
            drop(writer);
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            emit_broadcast_if_stored(&tools, &session, status, &mut out)?;
            break;
        }
    }

    let status = child.wait().context("waiting for oracle")?;
    eprintln!("[airlock] oracle exited with {status}");
    Ok(())
}

/// Phase-1 proof: play the agent deterministically against the real broker.
fn run_scripted(
    cfg: Config,
    db: &str,
    hmac_key: Vec<u8>,
    source: String,
    month: String,
    series: String,
    target: String,
) -> Result<()> {
    let mut tools = Tools::open(cfg, db, hmac_key, source, month.clone())?;
    let mut session = Session::default();

    eprintln!("[airlock] scripted run: source={} month={} series={}", tools.source, month, series);

    // Step 1: agent fetches the series.
    let fetch_call = ToolCall {
        schema_version: SCHEMA_VERSION,
        call_id: "call-fetch-1".to_string(),
        tool: "fetch_series".to_string(),
        args: serde_json::json!({ "seriesId": series }),
    };
    print_call(&fetch_call)?;
    let (fetch_res, _) = tools.dispatch(&fetch_call, &mut session);
    print_result(&fetch_res)?;
    let fetch_result = fetch_res
        .result
        .clone()
        .context("fetch_series returned no result")?;
    if !fetch_res.ok {
        anyhow::bail!("fetch failed: {:?}", fetch_res.error);
    }

    // Step 2: agent shapes the fetched observations into a store_dataset draft.
    let observations = fetch_result.get("observations").cloned().unwrap_or(serde_json::json!([]));
    let unit = fetch_result.get("unit").and_then(|v| v.as_str()).map(String::from);
    let sa = fetch_result.get("seasonalAdjustment").and_then(|v| v.as_str()).map(String::from);
    let lead = fetch_result.get("leadTimeMonths").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let indicator = serde_json::json!({
        "seriesId": series,
        "leadTimeMonths": lead,
        "unit": unit,
        "seasonalAdjustment": sa,
        "observations": observations,
    });
    let release_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let store_call = ToolCall {
        schema_version: SCHEMA_VERSION,
        call_id: "call-store-1".to_string(),
        tool: "store_dataset".to_string(),
        args: serde_json::json!({
            "target": target,
            "referenceMonth": month,
            "releaseDate": release_date,
            "indicators": [indicator],
        }),
    };
    print_call(&store_call)?;
    let (store_res, _) = tools.dispatch(&store_call, &mut session);
    print_result(&store_res)?;
    if !store_res.ok {
        anyhow::bail!("store failed: {:?}", store_res.error);
    }

    // Step 3: agent finishes → airlock broadcasts.
    let finish_call = ToolCall {
        schema_version: SCHEMA_VERSION,
        call_id: "call-finish-1".to_string(),
        tool: "finish".to_string(),
        args: serde_json::json!({ "status": "stored", "note": "phase-1 scripted run" }),
    };
    print_call(&finish_call)?;
    let (finish_res, status) = tools.dispatch(&finish_call, &mut session);
    print_result(&finish_res)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    emit_broadcast_if_stored(&tools, &session, status.unwrap_or_default(), &mut out)?;
    Ok(())
}

fn emit_broadcast_if_stored(
    tools: &Tools,
    session: &Session,
    status: String,
    out: &mut impl Write,
) -> Result<()> {
    if status == "stored" {
        if let Some(stored) = &session.stored {
            let bc = tools.build_broadcast(stored)?;
            eprintln!("[airlock] BROADCAST (would POST to main server):");
            writeln!(out, "{}", serde_json::to_string_pretty(&bc)?)?;
            out.flush()?;
        } else {
            eprintln!("[airlock] finish=stored but nothing was stored this session");
        }
    } else {
        eprintln!("[airlock] finish={status} — no broadcast");
    }
    Ok(())
}

fn print_call(call: &ToolCall) -> Result<()> {
    eprintln!("  → ToolCall  {}", serde_json::to_string(call)?);
    Ok(())
}
fn print_result(res: &ToolResult) -> Result<()> {
    eprintln!("  ← ToolResult {}", serde_json::to_string(res)?);
    Ok(())
}

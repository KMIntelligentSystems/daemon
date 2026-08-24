/**
 * daemon-oracle — untrusted agent (Phase 3: real LLM over OpenRouter).
 *
 * Spawned by the airlock:
 *   node dist/main.js '<task-context-json>'
 *
 * TaskContext is the single positional argument.  The oracle calls
 * OpenRouter with the tool catalog, drives the tool-calling loop
 * over stdin/stdout, and exits after finish or budget exhaustion.
 *
 * Environment:
 *   OPENROUTER_API_KEY — required, loaded from process.env
 */

import { createStdioBridge } from "./bridge.js";
import { runLLMLoop } from "./llm.js";
import { TaskContextSchema } from "./schemas.js";

async function main(): Promise<void> {
  // TaskContext is the first positional argument (argv[2]).
  const raw = process.argv[2];
  if (!raw) {
    console.error("[oracle] FATAL: expected TaskContext JSON as first argument");
    process.exit(2);
  }

  let ctx;
  try {
    ctx = TaskContextSchema.parse(JSON.parse(raw));
  } catch (err) {
    console.error("[oracle] FATAL: invalid TaskContext:", err instanceof Error ? err.message : err);
    process.exit(2);
  }

  const apiKey = process.env["OPENROUTER_API_KEY"];
  if (!apiKey) {
    console.error("[oracle] FATAL: OPENROUTER_API_KEY not set");
    process.exit(2);
  }

  console.error(
    "[oracle] session=%s source=%s month=%s model=%s budget(calls=%d, secs=%d)",
    ctx.sessionId, ctx.source, ctx.referenceMonth, ctx.model,
    ctx.budget.maxToolCalls, ctx.budget.wallClockSecs,
  );

  const bridge = createStdioBridge();
  const outcome = await runLLMLoop(bridge, ctx, apiKey);

  console.error(
    "[oracle] done: status=%s reason=%s iters=%d tokens(in=%d out=%d) cost=$%.5f",
    outcome.status, outcome.reason, outcome.iterations,
    outcome.inputTokens, outcome.outputTokens, outcome.costDollars,
  );

  process.exit(outcome.status === "error" ? 1 : 0);
}

main().catch((err) => {
  console.error("[oracle] fatal:", err);
  process.exit(2);
});
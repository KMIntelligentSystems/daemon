/**
 * daemon-oracle — untrusted agent (Phase 3: real LLM via Foundry Responses API).
 *
 * Spawned by the airlock:
 *   node dist/main.js '<task-context-json>'
 *
 * TaskContext is the single positional argument.  The oracle calls the
 * Foundry Responses API with the tool catalog, drives the tool-calling loop
 * over stdin/stdout, and exits after finish or budget exhaustion.
 *
 * Environment (injected by the airlock's lockdown; nothing else survives):
 *   FOUNDRY_ACCESS_TOKEN      — short-lived Entra bearer, minted by the airlock
 *   AZURE_AI_PROJECT_ENDPOINT — Foundry project endpoint
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

  const token = process.env["FOUNDRY_ACCESS_TOKEN"];
  const endpoint = process.env["AZURE_AI_PROJECT_ENDPOINT"];
  if (!token || !endpoint) {
    console.error("[oracle] FATAL: FOUNDRY_ACCESS_TOKEN / AZURE_AI_PROJECT_ENDPOINT not set");
    process.exit(2);
  }

  console.error(
    "[oracle] session=%s source=%s month=%s model=%s budget(calls=%d, secs=%d)",
    ctx.sessionId, ctx.source, ctx.referenceMonth, ctx.model,
    ctx.budget.maxToolCalls, ctx.budget.wallClockSecs,
  );

  const bridge = createStdioBridge();
  const outcome = await runLLMLoop(bridge, ctx, token, endpoint);

  console.error(
    `[oracle] done: status=${outcome.status} reason=${outcome.reason} iters=${outcome.iterations} ` +
    `tokens(in=${outcome.inputTokens} out=${outcome.outputTokens}) cost=$${outcome.costDollars.toFixed(5)}`,
  );

  process.exit(outcome.status === "error" ? 1 : 0);
}

main().catch((err) => {
  console.error("[oracle] fatal:", err);
  process.exit(2);
});
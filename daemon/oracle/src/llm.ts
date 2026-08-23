/**
 * Foundry Responses API client — sends the tool catalog to the model and
 * drives the tool-calling loop until `finish` or budget exhaustion.
 *
 * Dependencies: native `fetch` only.  No vendor SDK.
 *
 * Wire mapping from the old chat/completions shape (design §3.1):
 *   messages[]             -> input[] (model output items echoed back verbatim)
 *   system message         -> `instructions` request param
 *   choice.tool_calls[]    -> output[] items {type:"function_call", call_id, name, arguments}
 *   {role:"tool", ...}     -> {type:"function_call_output", call_id, output}
 *   usage.prompt_tokens    -> usage.input_tokens  (completion_tokens -> output_tokens)
 *
 * Auth: the airlock hands the oracle a short-lived Entra bearer as
 * FOUNDRY_ACCESS_TOKEN (+ AZURE_AI_PROJECT_ENDPOINT).  The oracle cannot
 * mint tokens; the credential dies with the job (design §3.2).
 */

import type { Bridge } from "./bridge.js";
import {
  type ResponsesFunctionCall,
  type ResponsesTool,
  type TaskContext,
  type ToolCall,
  SYSTEM_PROMPT,
  TOKEN_CEILING,
  COST_PER_1K_INPUT,
  COST_PER_1K_OUTPUT,
  COST_CEILING_DOLLARS,
  buildToolCatalog,
} from "./schemas.js";

// ─── Budget tracker ────────────────────────────────────────────────────

interface Budget {
  maxToolCalls: number;
  wallClockDeadline: number;
  iterations: number;
  cumulativeInputTokens: number;
  cumulativeOutputTokens: number;
}

function makeBudget(ctx: TaskContext): Budget {
  return {
    maxToolCalls: ctx.budget.maxToolCalls,
    wallClockDeadline: Date.now() + ctx.budget.wallClockSecs * 1000,
    iterations: 0,
    cumulativeInputTokens: 0,
    cumulativeOutputTokens: 0,
  };
}

function projectedCost(b: Budget): number {
  return (
    b.cumulativeInputTokens * (COST_PER_1K_INPUT / 1000) +
    b.cumulativeOutputTokens * (COST_PER_1K_OUTPUT / 1000)
  );
}

function budgetExceeded(b: Budget): string | null {
  if (b.iterations >= b.maxToolCalls) {
    return `max tool calls (${b.maxToolCalls}) reached`;
  }
  if (Date.now() >= b.wallClockDeadline) {
    return "wall-clock budget exhausted";
  }
  const tokens = b.cumulativeInputTokens + b.cumulativeOutputTokens;
  if (tokens >= TOKEN_CEILING) {
    return `token ceiling (${TOKEN_CEILING}) reached (${tokens} used)`;
  }
  if (projectedCost(b) >= COST_CEILING_DOLLARS) {
    return `cost ceiling ($${COST_CEILING_DOLLARS.toFixed(2)}) reached ($${projectedCost(b).toFixed(4)} used)`;
  }
  return null;
}

// ─── Foundry Responses API call ─────────────────────────────────────────

interface ResponsesResult {
  output: unknown[];
  usage: { input_tokens: number; output_tokens: number };
}

async function callResponses(
  token: string,
  endpoint: string,
  model: string,
  input: unknown[],
  tools: ResponsesTool[],
): Promise<ResponsesResult> {
  const body = JSON.stringify({
    model,
    instructions: SYSTEM_PROMPT, // was messages[0] {role:"system"}
    input,
    tools,
    tool_choice: "auto",
    temperature: 0, // deterministic for data-fetching
    // Must fit a full store_dataset payload (indicators x observations inline
    // in the arguments JSON); 800 truncated it mid-string and JSON.parse fell
    // back to {} — the airlock then (correctly) rejected missing args.
    max_output_tokens: 8192,
    store: false, // no server-side state: each call is a pure function of input[]
  });

  const res = await fetch(`${endpoint}/openai/v1/responses`, {
    method: "POST",
    headers: {
      "Authorization": `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body,
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "(no body)");
    throw new Error(`Foundry Responses HTTP ${res.status}: ${text.slice(0, 400)}`);
  }

  const data = await res.json();
  return {
    output: data.output ?? [],
    usage: {
      input_tokens: data.usage?.input_tokens ?? 0,
      output_tokens: data.usage?.output_tokens ?? 0,
    },
  };
}

// ─── First user message ─────────────────────────────────────────────────

function userText(ctx: TaskContext): string {
  return `Task: ${ctx.goal}\nSource: ${ctx.source}\nReference month: ${ctx.referenceMonth}\n\nFetch the required data, store it, then call finish.`;
}

// ─── Tool call conversion ──────────────────────────────────────────────

function toToolCall(raw: ResponsesFunctionCall, idx: number): ToolCall {
  let args: unknown;
  try {
    args = JSON.parse(raw.arguments);
  } catch {
    args = {};
  }
  return {
    schemaVersion: 1,
    // NB: we generate our own callId — the airlock's grammar requires the
    // /^call-[A-Za-z0-9_-]{1,64}$/ shape; the model's call_id is only used
    // for the function_call_output echo, not sent to the airlock.
    callId: `call-llm-${idx}`,
    tool: raw.name as ToolCall["tool"],
    args: args as Record<string, unknown>,
  };
}

// ─── The main loop ─────────────────────────────────────────────────────

export interface LLMOutcome {
  status: "stored" | "abstain" | "error";
  reason: string;
  iterations: number;
  inputTokens: number;
  outputTokens: number;
  costDollars: number;
}

function outcome(
  status: LLMOutcome["status"],
  reason: string,
  budget: Budget,
): LLMOutcome {
  return {
    status,
    reason,
    iterations: budget.iterations,
    inputTokens: budget.cumulativeInputTokens,
    outputTokens: budget.cumulativeOutputTokens,
    costDollars: projectedCost(budget),
  };
}

/**
 * Run the LLM tool-calling loop.
 *
 * 1. Send instructions + user message + tool catalog to the Responses API.
 * 2. On function_call items: convert to ToolCall, send over bridge, feed the
 *    ToolResult back as a function_call_output item (echoing the model's
 *    output items into input first — that is the Responses idiom).
 * 3. Repeat until the model calls finish, or the budget is exhausted.
 */
export async function runLLMLoop(
  bridge: Bridge,
  ctx: TaskContext,
  token: string,
  endpoint: string,
): Promise<LLMOutcome> {
  const tools = buildToolCatalog(ctx.series);
  const budget = makeBudget(ctx);
  const input: unknown[] = [{ role: "user", content: userText(ctx) }];

  console.error("[oracle] model=%s budget(calls=%d, secs=%d)", ctx.model, budget.maxToolCalls, (budget.wallClockDeadline - Date.now()) / 1000 | 0);

  for (let i = 0; i < budget.maxToolCalls; i++) {
    budget.iterations = i + 1;

    // Check budget before each model call.
    const exceeded = budgetExceeded(budget);
    if (exceeded) {
      console.error("[oracle] budget exceeded: %s", exceeded);
      bridge.send({
        schemaVersion: 1,
        callId: "call-finish-budget",
        tool: "finish",
        args: { status: "abstain", note: exceeded.slice(0, 400) },
      });
      await bridge.recv(); // read the finish ack so the airlock can proceed
      bridge.done();
      return outcome("abstain", exceeded, budget);
    }

    // Call the model.
    console.error(`[oracle] -> LLM call ${i + 1} (tokens in=${budget.cumulativeInputTokens} out=${budget.cumulativeOutputTokens} cost=$${projectedCost(budget).toFixed(4)})`);
    const resp = await callResponses(token, endpoint, ctx.model, input, tools);
    budget.cumulativeInputTokens += resp.usage.input_tokens;
    budget.cumulativeOutputTokens += resp.usage.output_tokens;

    // Responses idiom: echo the model's output items back into input before
    // appending our function_call_output items.
    input.push(...resp.output);

    const calls = resp.output.filter(
      (o): o is ResponsesFunctionCall =>
        typeof o === "object" && o !== null && (o as { type?: string }).type === "function_call",
    );

    if (calls.length === 0) {
      // Model returned text without tool calls — nudge it back to the task.
      const msg = resp.output.find(
        (o): o is { type: "message"; content?: { text?: string }[] } =>
          typeof o === "object" && o !== null && (o as { type?: string }).type === "message",
      );
      const text = msg?.content?.map((c) => c?.text ?? "").join("") ?? "";
      if (text) console.error("[oracle] <- LLM text: %s", text.slice(0, 120));
      if (resp.output.length === 0) {
        bridge.done();
        return outcome("error", "LLM returned empty response (no output items)", budget);
      }
      input.push({ role: "user", content: "Continue the task: fetch the required data, store it with store_dataset, then call finish." });
      continue;
    }

    for (let j = 0; j < calls.length; j++) {
      const raw = calls[j];
      const call = toToolCall(raw, i * 10 + j);

      console.error("[oracle] -> tool %s (callId=%s)", call.tool, call.callId);

      // Special: finish is the terminal tool — send it and return.
      if (call.tool === "finish") {
        const status = (call.args as Record<string, unknown>)?.status === "stored" ? "stored" : "abstain";
        bridge.send(call);
        await bridge.recv(); // ack
        bridge.done();
        return outcome(
          status,
          ((call.args as Record<string, unknown>)?.note as string) ?? "LLM called finish",
          budget,
        );
      }

      // Send tool call to airlock, receive result.
      bridge.send(call);
      const tr = await bridge.recv();

      if (!tr) {
        bridge.done();
        return outcome("error", "airlock closed connection (EOF)", budget);
      }

      console.error("[oracle] <- tool %s ok=%s", call.tool, tr.ok);

      // Feed the tool result back to the model (Responses function_call_output).
      input.push({
        type: "function_call_output",
        call_id: raw.call_id,
        output: JSON.stringify(tr),
      });
    }
  }

  // Exhausted iterations without calling finish.
  bridge.send({
    schemaVersion: 1,
    callId: "call-finish-exhausted",
    tool: "finish",
    args: { status: "abstain", note: "exhausted max tool calls without explicit finish" },
  });
  await bridge.recv();
  bridge.done();
  return outcome("abstain", "exhausted iterations", budget);
}

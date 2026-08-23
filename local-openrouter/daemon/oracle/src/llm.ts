/**
 * OpenRouter LLM client — sends the tool catalog to the model and drives
 * the tool-calling loop until `finish` or budget exhaustion.
 *
 * Dependencies: native `fetch` only.  No vendor SDK.
 */

import type { Bridge } from "./bridge.js";
import {
  type OpenRouterMessage,
  type OpenRouterToolCall,
  type OpenRouterTool,
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

// ─── OpenRouter API call ───────────────────────────────────────────────

const OR_BASE = "https://openrouter.ai/api/v1/chat/completions";

async function chatCompletion(
  apiKey: string,
  model: string,
  messages: OpenRouterMessage[],
  tools: OpenRouterTool[],
): Promise<{ message: OpenRouterMessage; usage: { prompt_tokens: number; completion_tokens: number } }> {
  const body = JSON.stringify({
    model,
    messages,
    tools,
    tool_choice: "auto",
    temperature: 0, // deterministic for data-fetching
  });

  const res = await fetch(OR_BASE, {
    method: "POST",
    headers: {
      "Authorization": `Bearer ${apiKey}`,
      "Content-Type": "application/json",
      "HTTP-Referer": "https://daemon.local",
      "X-Title": "daemon-oracle",
    },
    body,
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "(no body)");
    throw new Error(`OpenRouter HTTP ${res.status}: ${text.slice(0, 400)}`);
  }

  const data = await res.json();
  const choice = data?.choices?.[0];
  if (!choice) throw new Error("OpenRouter returned no choices");

  return {
    message: {
      role: "assistant",
      content: choice.message?.content ?? null,
      tool_calls: choice.message?.tool_calls,
    },
    usage: {
      prompt_tokens: data.usage?.prompt_tokens ?? 0,
      completion_tokens: data.usage?.completion_tokens ?? 0,
    },
  };
}

// ─── Message builder ────────────────────────────────────────────────────

function userMessage(ctx: TaskContext): OpenRouterMessage {
  return {
    role: "user",
    content: `Task: ${ctx.goal}\nSource: ${ctx.source}\nReference month: ${ctx.referenceMonth}\n\nFetch the required data, store it, then call finish.`,
  };
}

function systemMessage(): OpenRouterMessage {
  return { role: "system", content: SYSTEM_PROMPT };
}

// ─── Tool call conversion ──────────────────────────────────────────────

function toToolCall(raw: OpenRouterToolCall, idx: number): ToolCall {
  let args: unknown;
  try {
    args = JSON.parse(raw.function.arguments);
  } catch {
    args = {};
  }
  return {
    schemaVersion: 1,
    callId: `call-llm-${idx}`,
    tool: raw.function.name as ToolCall["tool"],
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

/**
 * Run the LLM tool-calling loop.
 *
 * 1. Send system prompt + user message + tool catalog to OpenRouter.
 * 2. On tool_calls: convert to ToolCall, send over bridge, feed ToolResult
 *    back as a tool-role message.
 * 3. Repeat until model calls finish, or budget is exhausted.
 */
export async function runLLMLoop(
  bridge: Bridge,
  ctx: TaskContext,
  apiKey: string,
): Promise<LLMOutcome> {
  const tools = buildToolCatalog(ctx.series);
  const budget = makeBudget(ctx);
  const messages: OpenRouterMessage[] = [
    systemMessage(),
    userMessage(ctx),
  ];

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
      // Read the finish ack so the airlock can proceed.
      await bridge.recv();
      bridge.done();
      return {
        status: "abstain",
        reason: exceeded,
        iterations: budget.iterations,
        inputTokens: budget.cumulativeInputTokens,
        outputTokens: budget.cumulativeOutputTokens,
        costDollars: projectedCost(budget),
      };
    }

    // Call the model.
    console.error("[oracle] → LLM call %d (tokens in=%d out=%d cost=$%.4f)", i + 1, budget.cumulativeInputTokens, budget.cumulativeOutputTokens, projectedCost(budget));
    const resp = await chatCompletion(apiKey, ctx.model, messages, tools);
    budget.cumulativeInputTokens += resp.usage.prompt_tokens;
    budget.cumulativeOutputTokens += resp.usage.completion_tokens;

    const choice = resp.message;
    messages.push(choice);

    // Model wants to call tools.
    if (choice.tool_calls && choice.tool_calls.length > 0) {
      for (let j = 0; j < choice.tool_calls.length; j++) {
        const raw = choice.tool_calls[j];
        const call = toToolCall(raw, i * 10 + j);

        console.error("[oracle] → tool %s (callId=%s)", call.tool, call.callId);

        // Special: finish is the terminal tool — send it and return.
        if (call.tool === "finish") {
          const status = (call.args as Record<string, unknown>)?.status === "stored" ? "stored" : "abstain";
          bridge.send(call);
          await bridge.recv(); // ack
          bridge.done();
          return {
            status,
            reason: (call.args as Record<string, unknown>)?.note as string ?? "LLM called finish",
            iterations: budget.iterations,
            inputTokens: budget.cumulativeInputTokens,
            outputTokens: budget.cumulativeOutputTokens,
            costDollars: projectedCost(budget),
          };
        }

        // Send tool call to airlock, receive result.
        bridge.send(call);
        const tr = await bridge.recv();

        if (!tr) {
          bridge.done();
          return {
            status: "error",
            reason: "airlock closed connection (EOF)",
            iterations: budget.iterations,
            inputTokens: budget.cumulativeInputTokens,
            outputTokens: budget.cumulativeOutputTokens,
            costDollars: projectedCost(budget),
          };
        }

        console.error("[oracle] ← tool %s ok=%s", call.tool, tr.ok);

        // Feed the tool result back to the model.
        messages.push({
          role: "tool",
          tool_call_id: raw.id,
          content: JSON.stringify(tr),
        });
      }
      continue; // next model call
    }

    // Model returned text without tool calls — feed back and loop.
    if (choice.content) {
      console.error("[oracle] ← LLM text: %s", choice.content.slice(0, 120));
    }

    // If no tool calls and no content, something is wrong.
    if (!choice.tool_calls && !choice.content) {
      bridge.done();
      return {
        status: "error",
        reason: "LLM returned empty response (no content, no tool calls)",
        iterations: budget.iterations,
        inputTokens: budget.cumulativeInputTokens,
        outputTokens: budget.cumulativeOutputTokens,
        costDollars: projectedCost(budget),
      };
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
  return {
    status: "abstain",
    reason: "exhausted iterations",
    iterations: budget.iterations,
    inputTokens: budget.cumulativeInputTokens,
    outputTokens: budget.cumulativeOutputTokens,
    costDollars: projectedCost(budget),
  };
}
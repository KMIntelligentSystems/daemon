/**
 * Stdio bridge — reads ToolResults from process.stdin, writes ToolCalls
 * to process.stdout.  The airlock spawns the oracle and owns the pipes;
 * the oracle never spawns anything.
 */

import { createInterface } from "node:readline";
import type { ToolCall, ToolResult } from "./schemas.js";
import { ToolResultSchema } from "./schemas.js";

export interface Bridge {
  /** Send one ToolCall to the airlock (→ stdout). */
  send(call: ToolCall): void;
  /** Wait for the next ToolResult from the airlock (← stdin). Returns null on EOF. */
  recv(): Promise<ToolResult | null>;
  /** Close stdin so the airlock knows we're done. */
  done(): void;
}

export function createStdioBridge(): Bridge {
  const rl = createInterface({ input: process.stdin });
  let resolver: ((r: ToolResult | null) => void) | null = null;
  let queue: (ToolResult | null)[] = [];

  rl.on("line", (line: string) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch {
      // Non-JSON line (e.g. broadcast envelope outside the tool loop) —
      // surface it raw so the caller can still inspect it.
      parsed = { _raw: trimmed };
    }
    const result = ToolResultSchema.safeParse(parsed);
    const tr: ToolResult | null = result.success
      ? result.data
      : (parsed as ToolResult | null);

    if (resolver) {
      resolver(tr);
      resolver = null;
    } else {
      queue.push(tr);
    }
  });

  rl.on("close", () => {
    if (resolver) {
      resolver(null);
      resolver = null;
    }
    queue.push(null);
  });

  return {
    send(call: ToolCall) {
      process.stdout.write(JSON.stringify(call) + "\n");
    },
    async recv() {
      if (queue.length > 0) return queue.shift()!;
      return new Promise<ToolResult | null>((resolve) => {
        resolver = resolve;
      });
    },
    done() {
      rl.close();
    },
  };
}
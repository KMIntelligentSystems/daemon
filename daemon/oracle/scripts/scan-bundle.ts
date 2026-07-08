/**
 * Bundle integrity scan — run as part of `npm run build` or CI.
 *
 * Scans the compiled dist/ for Node.js APIs the oracle is forbidden
 * to use.  Any hit is a build failure.
 */

import { readFileSync, readdirSync } from "node:fs";
import { join, extname, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DIST_DIR = join(__dirname, "..", "dist");

// Patterns that indicate forbidden capability use.
const FORBIDDEN: [RegExp, string][] = [
  [/require\s*\(\s*['"](?:node:)?fs['"]\s*\)/, "filesystem access (require fs)"],
  [/require\s*\(\s*['"](?:node:)?child_process['"]\s*\)/, "subprocess spawn (require child_process)"],
  [/require\s*\(\s*['"](?:node:)?net['"]\s*\)/, "raw network (require net)"],
  [/require\s*\(\s*['"](?:node:)?dns['"]\s*\)/, "DNS resolution (require dns)"],
  [/require\s*\(\s*['"](?:node:)?vm['"]\s*\)/, "code execution (require vm)"],
  [/require\s*\(\s*['"](?:node:)?worker_threads['"]\s*\)/, "worker threads"],
  [/import\s*\(/, "dynamic import()"],
];

function scanFile(filePath: string): string[] {
  const content = readFileSync(filePath, "utf-8");
  const violations: string[] = [];
  for (const [pattern, desc] of FORBIDDEN) {
    if (pattern.test(content)) {
      violations.push(`  ${filePath}: ${desc}`);
    }
  }
  return violations;
}

function main(): void {
  const allViolations: string[] = [];

  for (const entry of readdirSync(DIST_DIR, { withFileTypes: true })) {
    if (!entry.isFile()) continue;
    if (!entry.name.endsWith(".js")) continue;
    if (entry.name.endsWith(".map")) continue;

    allViolations.push(...scanFile(join(DIST_DIR, entry.name)));
  }

  if (allViolations.length > 0) {
    console.error("━━━ BUNDLE SCAN FAILED ━━━");
    console.error("Forbidden capabilities detected in dist/:");
    for (const v of allViolations) console.error(v);
    console.error("");
    console.error("The oracle must not access the filesystem, spawn subprocesses,");
    console.error("or open arbitrary network connections.  Remove the offending");
    console.error("imports before deploying.");
    console.error("━━━━━━━━━━━━━━━━━━━━━━━━━━");
    process.exit(1);
  }

  console.error("[scan] dist/ bundle clean — no forbidden capabilities detected.");
}

main();
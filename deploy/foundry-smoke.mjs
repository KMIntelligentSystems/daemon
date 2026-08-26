// Prove the Daemon SP can call the Foundry Responses API end-to-end:
// client-credentials token -> POST {endpoint}/openai/v1/responses.
// Reads AZURE_* from ../daemon/airlock/.env (dotenvy-compatible parsing).
import { readFileSync } from "node:fs";

const env = Object.fromEntries(
  readFileSync(new URL("../daemon/airlock/.env", import.meta.url), "utf8")
    .split(/\r?\n/)
    .filter((l) => /^[A-Z_]+=/.test(l))
    .map((l) => {
      const i = l.indexOf("=");
      return [l.slice(0, i), l.slice(i + 1).replace(/^"|"$/g, "")];
    })
);

// 1. Acquire token (client credentials)
const tokenRes = await fetch(
  `https://login.microsoftonline.com/${env.AZURE_TENANT_ID}/oauth2/v2.0/token`,
  {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      client_id: env.AZURE_CLIENT_ID,
      client_secret: env.AZURE_CLIENT_SECRET,
      scope: "https://ai.azure.com/.default",
      grant_type: "client_credentials",
    }),
  }
);
if (!tokenRes.ok) {
  console.error("token failed:", tokenRes.status, await tokenRes.text());
  process.exit(1);
}
const { access_token, expires_in } = await tokenRes.json();
console.log(`token acquired (expires_in ${expires_in}s)`);

// 2. Responses API call
const url = `${env.AZURE_AI_PROJECT_ENDPOINT}/openai/v1/responses`;
const res = await fetch(url, {
  method: "POST",
  headers: { Authorization: `Bearer ${access_token}`, "Content-Type": "application/json" },
  body: JSON.stringify({
    model: "gpt-4.1-mini",
    input: "Reply with exactly: foundry responses api ok",
    max_output_tokens: 50,
    store: false,
  }),
});
const text = await res.text();
if (!res.ok) {
  console.error("responses failed:", res.status, text.slice(0, 500));
  process.exit(1);
}
const data = JSON.parse(text);
console.log("status:", res.status, "| model:", data.model);
console.log("output_text:", JSON.stringify(data.output_text));
console.log("usage:", JSON.stringify(data.usage));

# local-openrouter — pre-Azure (OpenRouter) file set

Pristine copies of the files changed by the **§3 Foundry repoint** (2026-08-18),
extracted from git `HEAD` (commit aa48a14 "Added cron"). Keep these to run the
daemon locally against **OpenRouter** instead of Azure AI Foundry.

## The two variants

| File | OpenRouter (this dir, = git HEAD) | Foundry (current working tree, uncommitted) |
|---|---|---|
| `daemon/airlock/config.toml` | OpenRouter model ids | Foundry deployment names |
| `daemon/airlock/src/lockdown.rs` | injects `OPENROUTER_API_KEY` | mints Entra token, injects `FOUNDRY_ACCESS_TOKEN` |
| `daemon/oracle/src/llm.ts` | chat/completions loop | Responses API loop |
| `daemon/oracle/src/schemas.ts` | nested tool catalog, OR prices | flat strict catalog, Azure prices |
| `daemon/oracle/src/main.ts` | reads `OPENROUTER_API_KEY` | reads `FOUNDRY_ACCESS_TOKEN` + endpoint |

## ⚠️ Before you swap

The Foundry variant is **uncommitted working-tree state** — copying this
OpenRouter set over it DESTROYS it. Preserve it first (one time):

```powershell
cd c:/repos/daemon
New-Item -Force -ItemType Directory local-foundry\daemon\airlock\src, local-foundry\daemon\oracle\src | Out-Null
Copy-Item daemon\airlock\config.toml        local-foundry\daemon\airlock\
Copy-Item daemon\airlock\src\lockdown.rs    local-foundry\daemon\airlock\src\
Copy-Item daemon\oracle\src\llm.ts          local-foundry\daemon\oracle\src\
Copy-Item daemon\oracle\src\main.ts         local-foundry\daemon\oracle\src\
Copy-Item daemon\oracle\src\schemas.ts      local-foundry\daemon\oracle\src\
```

(Or simply `git add -A && git commit -m "Foundry repoint (§3)"` — after which
swapping is just `git checkout <commit>`, and these directories become
unnecessary. Recommended, but your call.)

## To run the OpenRouter variant

```powershell
cd c:/repos/daemon
Copy-Item -Recurse -Force local-openrouter\daemon\* daemon\
cd daemon\oracle; npm run build; cd ..\airlock; cargo build
```

You need `OPENROUTER_API_KEY` set (env or `daemon/airlock/.env`) — this
variant's lockdown injects it into the oracle child. The `AZURE_*` entries in
`.env` are ignored by this variant and can stay.

## What does NOT need swapping

`daemon/airlock/src/tools.rs` and `daemon/airlock/src/service.rs` were also
changed for Azure (nolock/DB_LOCK for SQLite on Azure Files) but are
**committed** (in aa48a14) and behave identically locally — leave them as-is
in both variants.

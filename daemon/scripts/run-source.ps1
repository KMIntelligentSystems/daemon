<#
.SYNOPSIS
  One scheduled "wake" for the daemon: build a signed RunRequest for a source
  and POST it to the running airlock. This is the external scheduler (Windows
  Task Scheduler) standing in for Railway cron - the airlock re-verifies the
  HMAC and re-validates every field, so this script is outside the trust boundary.

.DESCRIPTION
  The reference month is NOT hardcoded here. `emit-run-request --month auto`
  derives it from the resolved series' publication lag (config reference_lag_months),
  so a July run of a lag-1 source requests June. If the data for that month is not
  published yet, the airlock returns an "abstain" outcome (HTTP 200) and stores
  nothing - the next scheduled run retries.

.EXAMPLE
  .\run-source.ps1 -Source fred
  .\run-source.ps1 -Source fred -Series fred_mcumfn -Target m3_new_orders
#>
param(
  [Parameter(Mandatory = $true)][string]$Source,
  [string]$Series,                       # optional; omitted => source default_series
  [string]$Target = "m3_new_orders",
  [int]$Port = 8791,
  [string]$AirlockDir = "C:\repos\daemon\daemon\airlock",
  [string]$Exe = "target\debug\daemon-airlock.exe"
)

$ErrorActionPreference = "Stop"
# Run from the airlock dir so config.toml and .env (FRED_API_KEY, DAEMON_HMAC_KEY)
# resolve exactly as they do for the service - the HMAC key MUST match.
Set-Location $AirlockDir

$logDir = Join-Path $AirlockDir "logs"
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir | Out-Null }
$log = Join-Path $logDir "scheduler.log"
$stamp = (Get-Date).ToString("s")

# 1. Build + sign a RunRequest with an auto-derived reference month.
$emitArgs = @("emit-run-request", "--source", $Source, "--month", "auto", "--target", $Target)
if ($Series) { $emitArgs += @("--series", $Series) }
$req = & $Exe @emitArgs
if ($LASTEXITCODE -ne 0) {
  "$stamp ERROR emit-run-request failed for source=$Source (exit $LASTEXITCODE)" | Add-Content $log
  exit 1
}

# 2. POST it to the running airlock. abstain/stored come back as HTTP 200;
#    config rejects (bad source/series/model) as 4xx, which Invoke-RestMethod throws on.
try {
  $resp = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/run" -ContentType "application/json" -Body $req
  $line = "$stamp source=$Source status=$($resp.outcome.status) note=""$($resp.outcome.note)"""
  if ($resp.outcome.datasetId) { $line += " dataset=$($resp.outcome.datasetId)" }
  $line | Add-Content $log
  Write-Output $line
} catch {
  "$stamp ERROR POST /run failed for source=$Source : $($_.Exception.Message)" | Add-Content $log
  exit 1
}

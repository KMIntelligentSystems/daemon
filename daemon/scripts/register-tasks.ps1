<#
.SYNOPSIS
  Register Windows Task Scheduler tasks that wake the daemon per source, on each
  source's publication day (+1 day buffer). Each task runs run-source.ps1, which
  emits a signed RunRequest (reference month auto-derived) and POSTs it to the
  running airlock. This is the dev/local stand-in for Railway cron.

.DESCRIPTION
  Trigger days come from data/lookups/leading_indicators.json (the confirmed 2026
  release calendar). We fire one day AFTER the nominal release; if a release slips
  (e.g. a BLS holiday shift) the airlock simply abstains and the next month retries.

  Currently wired (Phase B): all 9 FRED leading-indicator series + 2 Census M3 targets.
  BLS tasks remain disabled until that source is wired.

  Run from an ordinary (non-elevated) PowerShell - per-user tasks don't need admin.
  Prereq: the airlock is running as `serve-http --port 8791` WITHOUT --schedule.

.EXAMPLE
  .\register-tasks.ps1                 # create enabled tasks
  .\register-tasks.ps1 -WhatIf         # show what would be created
  .\register-tasks.ps1 -Unregister     # remove all Daemon\ tasks
#>
param(
  [int]$Port = 8791,
  [string]$RunScript = "C:\repos\daemon\daemon\scripts\run-source.ps1",
  [switch]$Unregister,
  [switch]$WhatIf
)

$ErrorActionPreference = "Stop"

# name | source | series (blank => source default) | target | schtasks schedule args | enabled(=wired)
$tasks = @(
  # --- FRED — Phase 2 wired (all enabled) ------------------------------------
  @{ Name = "fred-mcumfn";      Source = "fred"; Series = "fred_mcumfn";  Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","18"); Enabled = $true  },  # G.17 ~17th, lag1
  @{ Name = "fred-tcu";         Source = "fred"; Series = "fred_tcu";     Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","18"); Enabled = $true  },  # G.17 ~17th, lag1
  @{ Name = "fred-ipman";       Source = "fred"; Series = "fred_ipman";   Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","18"); Enabled = $true  },  # G.17 ~17th, lag1
  @{ Name = "fred-cfnai";       Source = "fred"; Series = "fred_cfnai";   Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","27"); Enabled = $true  },  # CFNAI ~21st-26th, lag1
  @{ Name = "fred-empire";      Source = "fred"; Series = "fred_empire_state_mfg"; Target = "m3_new_orders"; Sched = @("/SC","MONTHLY","/D","16"); Enabled = $true  },  # NY ~15th, lag0
  @{ Name = "fred-philly";      Source = "fred"; Series = "fred_philly_fed_mfg";   Target = "m3_new_orders"; Sched = @("/SC","MONTHLY","/MO","THIRD","/D","THU"); Enabled = $true  },  # Philly 3rd Thu, lag0
  @{ Name = "fred-richmond";    Source = "fred"; Series = "fred_richmond_fed_mfg"; Target = "m3_new_orders"; Sched = @("/SC","MONTHLY","/MO","FOURTH","/D","TUE"); Enabled = $true  },  # Richmond 4th Tue, lag0
  @{ Name = "fred-dallas";      Source = "fred"; Series = "fred_dallas_fed_mfg";   Target = "m3_new_orders"; Sched = @("/SC","MONTHLY","/MO","LAST","/D","MON"); Enabled = $true  },  # Dallas last Mon, lag0
  @{ Name = "fred-kansas-city"; Source = "fred"; Series = "fred_kansas_city_fed_mfg"; Target = "m3_new_orders"; Sched = @("/SC","MONTHLY","/MO","LAST","/D","THU"); Enabled = $true  },  # KC last Thu, lag0

  # --- Census M3 — Phase B wired (both enabled) ------------------------------
  @{ Name = "census-m3-full";   Source = "census"; Series = "m3_new_orders";      Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","5");  Enabled = $true  },  # M3 full ~2nd-4th of M+2, lag2
  @{ Name = "census-m3-adv";    Source = "census"; Series = "m3_new_orders";      Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","28"); Enabled = $true  },  # M3 advance ~25th-27th, lag1
  @{ Name = "census-m3-uo";     Source = "census"; Series = "m3_unfilled_orders"; Target = "m3_unfilled_orders"; Sched = @("/SC","MONTHLY","/D","5");  Enabled = $true  },  # Unfilled orders, lag2

  # --- BLS — Phase C wired (both enabled) -----------------------------------
  @{ Name = "bls-ppi";          Source = "bls";  Series = "bls_ppi_mfg";       Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/D","16"); Enabled = $true  },  # PPI ~2nd full week, lag1
  @{ Name = "bls-ces-hours";    Source = "bls";  Series = "bls_ces_mfg_hours"; Target = "m3_new_orders";      Sched = @("/SC","MONTHLY","/MO","SECOND","/D","FRI"); Enabled = $true  }   # Employment Situation 1st Fri M+1 (buffer to 2nd Fri), lag1
)

if ($Unregister) {
  foreach ($t in $tasks) {
    & schtasks.exe /Delete /TN "Daemon\$($t.Name)" /F 2>$null
  }
  Write-Output "Removed Daemon\ tasks (any that existed)."
  return
}

foreach ($t in $tasks) {
  $seriesArg = if ($t.Series) { " -Series $($t.Series)" } else { "" }
  $tr = "powershell -NoProfile -ExecutionPolicy Bypass -File `"$RunScript`" -Source $($t.Source)$seriesArg -Target $($t.Target) -Port $Port"
  $createArgs = @("/Create","/TN","Daemon\$($t.Name)","/TR",$tr) + $t.Sched + @("/ST","09:00","/F")

  if (-not $t.Enabled) {
    Write-Output "SKIP (series not wired yet) - enable later with:"
    Write-Output "  schtasks $($createArgs -join ' ')"
    continue
  }
  if ($WhatIf) {
    Write-Output "WHATIF: schtasks $($createArgs -join ' ')"
    continue
  }
  Write-Output "Creating Daemon\$($t.Name) ..."
  & schtasks.exe @createArgs
}
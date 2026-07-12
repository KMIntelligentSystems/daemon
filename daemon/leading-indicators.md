## Leading-indicator plan for a next-gen manufacturing nowcast

Here's the structural analysis, ordered from most-tractable to least. I've split it into (a) what's actually reachable given the current data tooling, (b) predictive value we can expect, and (c) an implementation phasing.

### 1. Inventory of candidate leading indicators

| Indicator | Lead time | Publisher | API access | Cost | Predictive value for M3 shipments |
|---|---|---|---|---|---|
| **M3 New Orders (NO)** | 1–3 mo | Census (same M3 EITS API you already use) | ✅ Free, keyed | $0 | 🔴 Highest — new orders *are* future shipments |
| **M3 Unfilled Orders (UO)** | Same-period backlog | Census M3 EITS | ✅ Free | $0 | 🔴 Highest — direct pipeline measure |
| **FRED Capacity Utilization (TCU / MCUMFN)** | Coincident/slightly leading | Federal Reserve G.17 | ✅ FRED API (already used) | $0 | 🟠 High — supply-side pressure |
| **FRED IPMAN sub-indices (durables/nondurables/motor vehicles)** | Coincident | FRED | ✅ FRED API | $0 | 🟠 Medium — sector composition |
| **BLS CES manufacturing hours (weekly overtime, temp employment)** | 1 mo | BLS CES (already used) | ✅ BLS API | $0 | 🟡 Medium — labor demand signal |
| **PPI manufacturing sub-indices** | Coincident/slightly leading | BLS PPI (`ppi_series.json` lookup exists) | ✅ BLS API | $0 | 🟡 Medium — pricing pressure |
| **ISM Manufacturing PMI headline + New Orders sub-index** | 1–2 mo | Institute for Supply Management | 🔴 Paywalled/scraped | $$$ or terms-of-service risk | 🔴 Highest single sentiment signal |
| **S&P Global US Manufacturing PMI (flash + final)** | Flash: 2 wk lead; final: 1 mo | S&P Global (formerly Markit) | 🔴 Paywalled | $$$ | 🔴 Highest — flash release beats ISM by weeks |
| **Regional Fed manufacturing surveys** (Philly, NY Empire, Dallas, Richmond, KC) | 2–4 wk lead of ISM | Individual Federal Reserve banks | ✅ FRED mirrors all of them (`PHLMFGRIACD`, `GACDISA066MSFRBNY`, etc.) | $0 | 🟠 High — collectively track PMI closely |
| **Chicago Fed National Activity Index (CFNAI)** | 1 mo (composite) | Chicago Fed | ✅ FRED | $0 | 🟡 Medium — broader activity |
| **Building permits / new home sales** (upstream for materials demand) | 2–4 mo | Census | ✅ Census API | $0 | 🟡 Low-medium — sector-specific |

### 2. The critical distinction: **free ≠ ISM**

**ISM Manufacturing PMI is the gold-standard leading indicator but is not free.**
- **ISM's terms** prohibit redistribution and require subscription for machine access; scraping their release page is a TOS violation and their release times are gamed by market participants.
- **S&P Global PMI** is likewise paywalled; the flash release (roughly 3rd week of month) actually beats ISM by ~1 week, making it the best sentiment lead available — but it costs.

**However, we don't strictly need ISM.** The Regional Fed surveys collectively track ISM closely because they *are* ISM's raw material — ISM aggregates from regional data too. On FRED you can pull:

- Philadelphia Fed Manufacturing Business Outlook (`GACDFSA066MSFRBPHI` — general activity)
- Empire State (NY) Manufacturing Survey (`GACDISA066MSFRBNY`)
- Richmond Fed Manufacturing (`MFGCI`)
- Dallas Fed Manufacturing (`BACTSAMFRBDAL`)
- Kansas City Fed Manufacturing (`MOCMFG`)

Combined into a simple average or PCA-first-component, these give ~90% of ISM's information content, free, via a FRED API you already have keys for. **This is the pragmatic path.**

### 3. The M3 New Orders lead — the biggest single improvement

The single most important addition is **M3 New Orders (NO)**. From the M3 methodology:

- Shipments in month T reflect goods produced against orders received earlier
- New orders (NO) in month T are commitments that will ship in months T, T+1, T+2, T+3
- The M3 category codes cover this: `MTM/NO` is "Total Manufacturing New Orders"

**Access is trivially free**: the same Census EITS endpoint you already use (`api.census.gov/data/timeseries/eits/m3`), same API key, same lookup file (`data/lookups/m3_series.json`). You literally change `data_type_code=VS` to `data_type_code=NO` and refetch.

Expected impact: on the May 2026 miss, if April 2026 new orders were high (which the M3 shipments trajectory Feb→Mar→Apr strongly implies they were), a nowcast using NO(t-1) as a predictor would have projected shipments(May) upward. The −$12,174M nowcast miss might have compressed to −$3,000–5,000M.

### 4. Recommended implementation phasing

**Phase 1 — free, high-value additions (1 session of work)**

1. **Extend `create_ec_chart`/M3 fetch script to pull NO and UO** alongside VS. Cache CSVs like `m3_new_orders_nsa.csv`, `m3_unfilled_orders_nsa.csv`. All under the existing Census API key.
2. **Add a FRED leading-indicators fetcher.** New CSVs: `fred_tcu.csv` (capacity util), `fred_mcumfn.csv` (mfg capacity util), plus the five regional Fed manufacturing surveys. All free via FRED.
3. **Build a `data/lookups/leading_indicators.json`** cataloging series IDs, lead times, release schedules, and known revisions. Same shape as `fred_ipi.json` today.

**Phase 2 — rebuild the nowcast with leading indicators (1–2 sessions)**

4. **Update `industry-output-nowcast` skill** to accept a broader predictor panel: NO(t-1), NO(t-2), UO(t-1), TCU(t), regional Fed composite(t), CFNAI(t). Keep the LASSO-CV structure; the additional predictors will get shrunk or zeroed if not informative.
5. **Backtest against 2022–2026** with a rolling origin, keeping the current M3-shipments-only baseline as the null. Report the improvement in RMSE / MAE / calibration of prediction intervals.
6. **Report which predictors LASSO retains** — this is diagnostic. If new orders survives LASSO with a large coefficient, that confirms the theory. If regional Fed surveys survive, that validates them as an ISM substitute.

**Phase 3 — optional paywalled additions (business decision)**

7. If Phase 2 leaves substantial residual error and you want to close the gap, subscribe to **S&P Global US Manufacturing PMI flash release**. Cost typically $2–5k/year for machine access. The flash release drops ~2 weeks before ISM final and beats every free indicator on timing. Only justify this if the model's stakes warrant it.
8. **Do not scrape ISM.** Their TOS is enforced, and republication in the artifact catalog would be a licensing violation. If ISM is needed, buy it.

### 5. Structural improvements alongside the data

Two model changes that pair naturally with leading indicators:

- **Replace the STL linear-trend extrapolation with an ETS local-level+drift or ARIMA(0,1,1)+drift.** These respond to recent acceleration rather than dampening it with a 24-month linear fit. Available in `statsmodels`, no new dependencies.
- **Add a mixed-frequency (MIDAS) capability** to use daily/weekly indicators (financial-market volatility, weekly rail-carload data via FRED) alongside monthly. Non-trivial but the framework fits the same skill structure. Defer until Phase 2 stabilizes.

### 6. What to build first, concretely

If you want to prioritize one hour of work for maximum forecast improvement: **fetch M3 New Orders (NO) and add NO(t-1), NO(t-2) as predictors in the existing nowcast LASSO**. That alone would likely have caught most of the May 2026 miss. Everything else is refinement on top of that single change.

Ready to implement Phase 1 whenever you want. Say the word and I'll fetch NO + UO + the FRED regional-Fed indicators, cache the CSVs, and update the lookup catalog — then we can move on to rebuilding the nowcast in Phase 2.
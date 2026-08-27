# Entry and Predictive Model

## 1. Current-vs-target distinction

The baseline “model” is primarily:
- hand-weighted signal scoring;
- fixed thresholds;
- regime heuristics;
- manually constructed wallet alpha score;
- separate DexScreener momentum score.

These are useful baselines. They are not yet a validated predictive model.

## 2. Candidate decision hierarchy

Canonical order:

```text
Basic validity
→ Data readiness
→ Fatal risk veto
→ Survival risk
→ Opportunity / expected return
→ Execution feasibility
→ Portfolio risk
→ Enter / Observe / Reject
```

The order matters. A high opportunity score may not override fatal risk or unexecutable impact.

## 3. Fatal risk

Fatal rules should cover only conditions where policy says the system must not enter, for example:
- unsafe authority/privilege conditions;
- known malicious creator/entity with sufficient evidence;
- immediate creator/cluster dump;
- severe liquidity/exit infeasibility;
- honeypot/impossible sell evidence;
- already-collapsed market;
- critical data unavailable when policy requires it.

Fatal context may contain `Known`, `Unknown`, and `Unavailable`. It must not turn unknown into false.

## 4. Survival model

Target examples:
- `P(severe_drawdown_before_H)`;
- `P(exit_failure_or_liquidity_collapse_before_H)`;
- `P(creator_or_cluster_dump_before_H)`.

This can begin as rules + calibrated baseline, then transition to a statistical model once replay data exists.

## 5. Opportunity model

Prefer horizon/barrier outcomes over “will graduate”.

Example labels:
- `P(+10% net before -8% net within 30s)`;
- `P(+20% net before -10% net within 60s)`;
- expected net executable return at 15/30/60/120 seconds.

Barrier percentages are examples, not canonical constants. Select them using observed outcome distributions and economic costs.

## 6. Recommended first model family

For structured tabular features with missingness and nonlinear interactions:
- CatBoost;
- LightGBM;
- XGBoost.

Start with interpretable boosted trees before neural architectures.

Requirements:
- chronological train/validation/test;
- walk-forward evaluation;
- probability calibration;
- feature leakage checks;
- stability by market regime;
- comparison with current heuristic baseline.

## 7. Feature families

### Launch/protocol
- quote asset/regime;
- Pump instruction/version regime;
- Mayhem status;
- initial creator buy;
- initial real/effective reserves;
- metadata/social features as weak/contextual features only.

### Order flow
- net quote flow;
- buy-volume share;
- volume velocity/acceleration;
- distinct buyer velocity;
- median and tail buy size;
- sell-wave magnitude;
- recovery after sell wave;
- new-vs-repeat buyer share.

### Entity quality
- creator history;
- buyer wallet history with sample uncertainty;
- funding graph;
- cluster concentration;
- fresh-wallet concentration;
- same-slot coordination.

### Supply/holders
- creator holdings;
- top holder/cluster concentration;
- top-N concentration;
- concentration change;
- holder growth;
- Gini/entropy if correctly defined.

### Price/liquidity
- reserve velocity;
- price returns;
- peak drawdown;
- volatility;
- estimated exit impact for our intended size;
- remaining bonding-curve state / graduation proximity where applicable.

### Execution
- current priority estimate;
- quote age;
- expected impact;
- route latency history;
- recent fill rate;
- recent realized slippage.

## 8. Wallet alpha

The baseline wallet profiler should not directly control sizing until rebuilt with:
- quantity-aware inventory;
- partial buys/sells;
- realized and unrealized P&L;
- protocol/network/route costs;
- actual launch timing;
- recency weighting;
- minimum samples;
- shrinkage toward population mean;
- uncertainty intervals.

A wallet with 4/5 wins is not automatically more trustworthy than one with 650/1000 wins.

## 9. Mayhem regime

Pump Mayhem behavior can produce synthetic/randomized trades. Therefore:
- identify it explicitly where possible;
- exclude it from ordinary training initially or create a separate regime;
- do not treat its raw trade count/volume as ordinary organic demand.

## 10. Position size

Position size is downstream of edge and uncertainty.

Inputs:
- predicted net edge;
- calibrated risk;
- prediction uncertainty;
- current portfolio exposure;
- remaining loss budget;
- executable exit capacity;
- execution quality.

The sizer must return `0` when remaining portfolio capacity is below minimum tradable size. A final minimum clamp may not re-inflate a size above remaining capacity.

## 11. Explainability

Every entry decision must save:
- model/rule versions;
- fatal gate results;
- top risk drivers;
- top opportunity drivers;
- probability/expected return;
- execution budget;
- final reason.

This is for debugging and model validation, not marketing.

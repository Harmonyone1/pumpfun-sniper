# Added Module Provenance and Runtime-Status Review

Baseline head reviewed: `2158c64426de0f0c532221c578348c1dad1e075d`.

This document exists because a large portion of the current repository was added after the initial commit. “File exists” and “commit message says wired” do **not** mean the subsystem is currently populated with valid live data.

## 1. Repository expansion since the initial commit

Comparing initial commit `9199c3b5f6cad565c8861488260037fd77240b21` to the current baseline shows 23 commits of expansion.

Major added families include:
- DexScreener hot-token scanner;
- adaptive filter + scoring + cache + enrichment;
- Helius integration;
- holder watcher;
- kill-switch and bundled-wallet detection;
- early momentum signals;
- smart-money profiling and clustering;
- full `src/strategy/` package;
- multi-wallet trading support.

These additions must be evaluated by **runtime wiring and input truth**, not by module sophistication.

## 2. Strategy package provenance

The broad strategy system was introduced primarily in commit:
`41dd14cd4ba41b14069ac29ad76f9b8194067323`
(“Implement confidence regime model with readiness gates”).

It added:
- `src/strategy/arbitrator.rs`
- `src/strategy/chain_health.rs`
- `src/strategy/creator_privileges.rs`
- `src/strategy/delta_tracker.rs`
- `src/strategy/engine.rs`
- `src/strategy/execution_feedback.rs`
- `src/strategy/exit_manager.rs`
- `src/strategy/fatal_risk.rs`
- `src/strategy/liquidity.rs`
- `src/strategy/mod.rs`
- `src/strategy/portfolio_risk.rs`
- `src/strategy/price_action.rs`
- `src/strategy/randomization.rs`
- `src/strategy/regime.rs`
- `src/strategy/sizing.rs`
- `src/strategy/tactics/*`
- `src/strategy/types.rs`

### Runtime-status table

| Component | Exists | Constructed/used by StrategyEngine | Valid live inputs verified? | Canonical assessment |
|---|---:|---:|---:|---|
| DecisionArbitrator | yes | yes | dependent on upstream inputs | reusable concept |
| FatalRiskEngine | yes | yes | **no** for several critical fields | unsafe until real/unknown inputs |
| LiquidityAnalyzer | yes | yes | **no**; simplified SOL/virtual-reserve assumptions | replace/rebuild |
| PortfolioRiskGovernor | yes | yes | partial; live exit state divergence found | reconcile with PositionService |
| ChainHealth | yes | yes | **sampling not found wired**; tx feedback also largely unwired | currently weak/default state |
| ExecutionFeedback | yes | yes | live recording not found wired | dormant adaptation |
| RegimeClassifier | yes | yes | baseline CLI feeds synthetic order-flow/distribution/creator fields | output not trustworthy |
| PositionSizer | yes | yes | receives placeholder remaining capacity and derived confidence | unsafe for sizing truth |
| ExitManager | yes | yes | live path overlaps other exit systems and uses placeholder contexts | consolidate |
| Randomizer | yes | yes | **yes**; entry delay/size/skip can be active | remove from edge-critical decisions unless validated |
| DeltaTracker | yes | yes | update method exists; live market-event population not verified | mostly empty/default risk |
| PriceActionAnalyzer | yes | yes | update method exists; live population incomplete | mostly empty/default risk |
| CreatorPrivilegeChecker | yes | **not found integrated in engine/runtime search** | no | dormant despite safety importance |
| FrontRunDetector | yes | no runtime use found | no | experimental/dead |
| SniperPiggyback | yes | no runtime use found | no | experimental/dead |
| RugPredictor tactic | yes | no runtime use found | no | experimental/dead |

## 3. Randomization review

`RandomizationConfig::default()` enables randomization with:
- 50–200 ms entry delay;
- ±5% entry-size jitter;
- 2% random trade skip probability;
- exit-delay and exit-size jitter settings;
- strategy entropy/polling jitter features.

`StrategyEngine::evaluate_entry()` applies `jitter_entry()` to an approved entry. The CLI also calls `get_entry_delay()` before sending a buy.

Canonical conclusion:
- random delay can worsen quote staleness and adverse selection;
- random size changes complicate deterministic risk budgeting;
- random skip reduces measurable strategy consistency;
- no demonstrated adversarial benefit has been validated against the economic cost.

**Canonical policy:** disable/remove randomization from capital-critical entry/exit decisions until replay proves a positive net benefit. Execution timing may later use deliberately bounded micro-jitter only if it is part of a measured anti-adversarial design.

## 4. Liquidity analyzer review

`src/strategy/liquidity.rs` is not a canonical exit-feasibility implementation.

Baseline issues:
1. `analyze()` converts virtual SOL reserves using `1e9` and virtual token reserves using a hard-coded `1e6` token-decimal assumption.
2. It treats the virtual SOL reserve model as the basis for extractable exit capacity rather than constraining by real/effective quote reserves and current protocol semantics.
3. It is SOL-specific and not quote-mint-aware.
4. It omits current protocol/creator fees from the exit estimate.
5. `can_safely_exit()` uses a boolean expression where the slippage clause can return true even when `exit_feasible` is false:
   ```text
   exit_feasible && max_safe_exit >= position
   || calculated_slippage <= threshold
   ```
6. `analyze_simple()` accepts loose `f64` reserves whose upstream units may already be ambiguous.
7. Large-exit slippage uses interpolation/extrapolation heuristics instead of an exact current protocol quote.

Canonical conclusion:
**Do not use the current `LiquidityAnalysis.max_safe_exit_sol` as capital-sizing or fatal-risk truth.**
Rebuild exit feasibility from the current official Pump/PumpSwap quote math, real/effective quote reserves, actual quote asset, token decimals, fees, and the intended position size.

## 5. Chain-health review

`ChainHealth::sample()` can query:
- recent performance samples;
- recent prioritization fees.

However baseline runtime search did not find a scheduler invoking this sampler. `StrategyEngine` reads ChainHealth state, while transaction feedback calls that would populate failure rate were also not found wired into normal CLI execution.

Therefore chain state can remain dominated by defaults:
- ~400 ms slot time;
- 0 failure rate;
- low fallback priority fee.

Canonical conclusion:
a component that is not sampled must report `Unavailable/Uninitialized`, not `Normal`.

## 6. Creator-privilege review

`src/strategy/creator_privileges.rs` exists to detect mint/freeze/other privileges, but repository search found it only in its module/export, not integrated into `StrategyEngine` entry evaluation.

At the same time, `StrategyEngine::evaluate_entry()` populates fatal-risk authority fields with `false`.

This creates a particularly dangerous false-safety pattern:
```text
real checker exists but is not used
+
fatal context receives false
=
safety feature appears present while unable to veto
```

Canonical requirement:
the privilege checker/on-chain authority source must feed a tri-state risk input:
`Safe / Unsafe / Unavailable`.
`Unavailable` may never be converted to `false`.

## 7. Tactics review

The tactics package includes:
- accumulation/front-run pattern detector;
- profitable-sniper piggyback tracker;
- rug predictor.

Repository search found these classes only in their own modules and exports, not in the live CLI/engine path.

Canonical status:
**experimental / dormant**.

Do not tell future agents to “wire all tactics in”. Each tactic first requires:
- valid event inputs;
- replay dataset;
- leakage review;
- independent measurement;
- explicit interaction with fatal risk and opportunity model.

## 8. Smart-money additions

The smart-money MVP added:
- kill switch;
- bundled-wallet detection;
- wallet profiler;
- alpha score;
- clustering;
- smart-money signal provider.

Current canonical concerns remain:
- wallet profiler inventory/P&L logic is not quantity-aware enough for sizing;
- top-holder data is incomplete in parts of the live path;
- creator/sell-event monitoring depends on a valid authenticated trade stream;
- wallet count must be replaced by entity-aware demand where possible.

Smart money can be a feature family, not a bypass around candidate readiness.

## 9. Early-momentum additions

The early-momentum provider added:
- volume spike;
- accumulation pattern;
- first-trades quality;
- bonding-curve position;
- creator buyback.

The provider is registered, but baseline runtime review did not find its `record_trade()` fed by the live PumpPortal handler.

Canonical status:
**registered does not equal populated**.

Do not assign confidence to a provider whose internal window has not been populated from canonical events.

## 10. Multi-wallet additions

Multi-wallet support was added and then followed quickly by a severe verification/sell bug fix after many “abandoned” positions.

Canonical lesson:
wallet identity is not auxiliary metadata. It is part of execution and position identity and must flow through:
```text
wallet selection
→ execution plan
→ signing
→ balance reconciliation
→ position
→ exit signing
→ realized P&L
```

No default-wallet fallback is allowed when wallet identity is missing or malformed.

## 11. Module-review rule for future commits

When a new file is committed, the architecture/review layer must classify it before a coding agent is told to use it:

```text
A. canonical and live
B. canonical but not yet wired
C. experimental
D. legacy/deprecated
E. unsafe/blocked
```

The classification must be based on:
- who constructs it;
- who writes its inputs;
- who consumes its outputs;
- whether inputs are real or placeholders;
- whether it affects live money;
- whether tests exercise only isolated logic or actual integration.

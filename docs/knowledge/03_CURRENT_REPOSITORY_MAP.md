# Current Repository Map

Baseline: `2158c64426de0f0c532221c578348c1dad1e075d`.

This document describes the baseline repository, not the final canonical design.

## Root

| Path | Current role | Canonical status |
|---|---|---|
| `Cargo.toml` | Rust dependency manifest | Keep; executable should eventually track `Cargo.lock` |
| `config.toml` | Runtime configuration | **Security remediation required**; tracked baseline contains live-looking provider credentials |
| `.env.example` | Environment template | Keep, but ensure secrets are placeholders only |
| `.gitignore` | Ignore policy | Baseline ignores `Cargo.lock`; reconsider for reproducible executable builds |
| `credentials/` | Wallet/credential documentation and registry scaffolding | Treat as security-sensitive; tracked secret material is forbidden |

## CLI / runtime composition

### `src/main.rs`
Defines CLI:
- `start`
- `sell`
- `status`
- `config`
- `health`
- `wallet ...`
- `scan`
- `hot-scan`

Important baseline mismatch:
- `hot-scan` help claims “Survivor Mode validation”.
- CLI defaults for hot-scan are older than the newer `HotScanConfig` defaults in `src/dexscreener.rs`.

### `src/cli/commands.rs`
A very large runtime composition module. It currently owns too many responsibilities:
- startup wiring;
- PumpPortal handling;
- token filtering;
- strategy invocation;
- buy execution;
- buy verification;
- position construction;
- kill-switch processing;
- trade-event auto-entry;
- copy trading;
- live position monitoring;
- sell retries;
- scan/hot-scan commands;
- wallet commands.

Canonical direction: progressively extract orchestration into explicit services and make `commands.rs` a thin composition/CLI layer.

## Market stream: `src/stream/`

| File | Current role | Status |
|---|---|---|
| `pumpportal.rs` | PumpPortal WS events and dynamic subscriptions | **Must update auth, subscription and unit contracts** |
| `shredstream.rs` | Planned/partial Jito ShredStream path | **Deprecated direction**; Jito service sunset scheduled 2026-09-05 |
| `decoder.rs` | stream decoding support | Revalidate against current Pump instructions/IDL |
| `backpressure.rs` | event load/backpressure support | Potentially reusable |
| `mod.rs` | exports | Keep as composition boundary |

Critical baseline fact: `TradeEvent.sol_amount` is documented as already SOL.

## Pump protocol: `src/pump/`

| File | Current role | Status |
|---|---|---|
| `accounts.rs` | Handwritten BondingCurve/Global layouts and math | **Replace/update against official current IDL/client** |
| `instruction.rs` | Pump instruction construction | **Revalidate for V2 buy/sell and quote mints** |
| `mint.rs` | mint helpers | Revalidate Token-2022/current Pump support |
| `price.rs` | price helpers | Must use canonical typed units |
| `program.rs` | program constants/PDAs | Revalidate against official docs |
| `mod.rs` | exports | Keep |

Baseline `BondingCurve` ordering is inconsistent with current official Pump documentation and lacks newer fields/regimes.

## Filtering/intelligence: `src/filter/`

### Basic / safety-facing
- `token_filter.rs` — regex/basic thresholds.
- `kill_switch.rs` — deployer/top-holder sell decision logic.
- `holder_watcher.rs` — holder sell monitoring.
- `wallet_tracker.rs` — tracked-wallet utility.
- `bundled_detection.rs` — bundled/coordinated early-buy analysis.

### Adaptive system
- `adaptive/mod.rs` — adaptive filter coordinator.
- `adaptive/config.rs` — adaptive settings.
- `cache/mod.rs` — enrichment/filter cache.
- `enrichment.rs` — background enrichment.
- `helius.rs` — Helius integration.
- `scoring/mod.rs` — hand-weighted scoring and recommendations.
- `signals/mod.rs` — signal contracts/types.
- `signals/metadata.rs`
- `signals/wallet_behavior.rs`
- `signals/smart_money.rs`
- `signals/early_momentum.rs`

### Survivor/momentum
- `momentum.rs` — richer minimum-observation / trade / holder / second-wave validator.
- `momentum.rs.backup` — historical artifact; must not be treated as source of truth.

Baseline finding: `MomentumValidator` exists but is not wired into the live CLI path found during review.

### Smart money
- `smart_money/alpha_score.rs`
- `smart_money/clustering.rs`
- `smart_money/wallet_profiler.rs`

Baseline finding: wallet P&L matching is not quantity-aware enough to justify direct capital sizing.

## Strategy: `src/strategy/`

The strategy package presents a broad system, but multiple components are currently dormant or fed placeholder values.

### Safety
- `fatal_risk.rs`
- `liquidity.rs`
- `creator_privileges.rs`
- `portfolio_risk.rs`
- `arbitrator.rs`

### Intelligence
- `chain_health.rs`
- `delta_tracker.rs`
- `execution_feedback.rs`
- `price_action.rs`
- `regime.rs`

### Decisions
- `engine.rs`
- `sizing.rs`
- `exit_manager.rs`
- `randomization.rs`

### Tactics
- `tactics/frontrun.rs`
- `tactics/piggyback.rs`
- `tactics/rug_predict.rs`

Baseline runtime search found these tactic classes only in their own modules/exports, not in the live engine/CLI path. Treat them as experimental/dormant.

Additional strategy-package findings:
- `creator_privileges.rs` exists but was not found integrated into live entry evaluation.
- `randomization.rs` is enabled by default and can add entry delay, size jitter, and random skips; the live entry path uses it.
- `chain_health.rs` has an RPC sampler, but baseline runtime search did not find the sampler scheduled.
- `liquidity.rs` assumes SOL/6-decimal token semantics and virtual-reserve-based exit capacity; it is not canonical exit-feasibility truth.

Baseline findings:
- `StrategyEngine::evaluate_entry` receives several synthetic/minimal fields from `commands.rs`.
- fatal risk context contains placeholder safe values for several kill-switch inputs.
- `record_execution` / transaction-failure feedback are not wired into the live path found.
- strategy `evaluate_position` exit path is not the authoritative CLI live exit path found.
- strategy `record_exit()` was not found wired from the CLI live exit path during baseline review.

## Trading execution: `src/trading/`

| File | Current role | Status |
|---|---|---|
| `pumpportal_api.rs` | PumpPortal Lightning/Local build/send methods | Rework around canonical execution/reconciliation |
| `jito.rs` | Jito execution utilities | Reassess versus current Jito/Sender architecture |
| `transaction.rs` | transaction building | Candidate for canonical transaction boundary |
| `simulation.rs` | simulation support | Reuse where relevant |
| `tips.rs` | tip calculation | Rework as dynamic execution-cost input |
| `mod.rs` | exports | Keep |

Baseline hazard: unused/local retry code can escalate slippage dramatically. That is not canonical behavior.

## Positions: `src/position/`

| File | Current role | Status |
|---|---|---|
| `manager.rs` | open positions, P&L, persistence, daily stats | Keep concept; replace estimated truth with reconciled truth |
| `price_feed.rs` | bonding-curve poll + DexScreener fallback | Existing component is not wired as authoritative live feed in reviewed `start()` path |
| `auto_sell.rs` | another auto-sell implementation | Overlaps live monitor/strategy exit logic; consolidation required |
| `mod.rs` | exports | Keep |

## Wallets: `src/wallet/`

- `credentials.rs` — wallet registry/key material references.
- `manager.rs` — wallet orchestration.
- `multi_wallet.rs` — trading-wallet selection.
- `safety.rs` — deterministic limits.
- `extractor.rs` — profit extraction.
- `transfer.rs` — transfers.
- `advisor.rs` — proposal/advisor behavior.
- `types.rs` — wallet domain types.

Canonical requirement: the exact wallet used for a fill must be persisted on the execution and position and used for balance reconciliation and exit signing.

## DexScreener

`src/dexscreener.rs` implements scanning/hot-token heuristics. It is a separate discovery path from new-token PumpPortal ingestion and must not silently bypass canonical entry/risk requirements if it auto-buys.

# Known Defects and Migration Hazards

Baseline: `2158c64426de0f0c532221c578348c1dad1e075d`.  
These findings must be revalidated after the active implementation agent lands changes.

Severity:
- **P0:** can directly corrupt money, state, safety, or live decision truth.
- **P1:** materially weakens selection, sizing, or reliability.
- **P2:** maintainability/validation issue.

## P0

### DEF-P0-001 — Publicly tracked provider secrets
`config.toml` contains live-looking provider/API credentials in the public baseline.

Required outcome:
- rotate/revoke externally;
- remove from tracked config;
- load via environment/secret mechanism;
- purge history if security policy requires;
- never reproduce values in logs/docs/prompts.

### DEF-P0-002 — PumpPortal trade SOL double conversion
`TradeEvent.sol_amount` is explicitly “already in SOL”, but live trade handler divides by `1e9`.

Effects:
- corrupt trade logging;
- corrupt thresholds;
- corrupt kill-switch sell volume;
- corrupt copy-trade interpretation;
- suppress the `>=0.05 SOL` trade-trigger branch.

Safety coupling:
fixing this defect can activate the unsafe one-trade entry branch, so that branch must be disabled/rerouted in the same safe rollout.

### DEF-P0-003 — Contradictory `v_sol_in_bonding_curve` unit assumptions
New-token path treats the value as lamports in one place and later uses a `<1000` heuristic to guess SOL vs lamports.

Required outcome:
normalize provider event units exactly once.

### DEF-P0-004 — Stale/incorrect Pump BondingCurve layout
Baseline handwritten layout orders reserve fields differently from official Pump documentation and lacks newer creator/quote-mint-era structure.

Required outcome:
use current official IDL/client or a versioned, tested exact layout.

### DEF-P0-005 — Live strategy receives synthetic analysis inputs
The CLI creates:
- `organic_score` from position multiplier;
- wash score `0`;
- buy/sell ratio `1`;
- early sell pressure `0`;
- holder count `1`;
- top holder/top-10 `100%`;
- creator behavior as all-safe zeros/false;
- default price action;
- confidence = position multiplier.

Required outcome:
never call strategy evaluation until real required inputs are available; use explicit unavailable states.

### DEF-P0-006 — Fatal-risk context contains safe placeholders
Multiple veto conditions are structurally incapable of firing when their live input is populated with fake safe defaults.

### DEF-P0-007 — Live exit architecture is split
The repository contains:
- strategy `ExitManager`;
- `position/auto_sell.rs`;
- an inline monitor in `commands.rs`.

The reviewed runtime path uses overlapping/independent behavior rather than one authoritative exit engine.

### DEF-P0-008 — PriceFeed is not authoritative in reviewed live start path
A `PriceFeed` implementation exists, but the reviewed start path's inline monitor reads `position.current_price` and does not clearly wire this component as its updater.

### DEF-P0-009 — Local/Lightning sell routing conflict
Reviewed monitor logic can attempt Lightning sells before local fallback even when local mode is the relevant route; this is structurally incompatible with a local trader that lacks a Lightning key.

Revalidate against the recent multi-wallet fix because commit history claims part of this was corrected in another path.

### DEF-P0-010 — Sell retry exhaustion can erase risk truth
Reviewed inline monitor can close/remove a position with `0` proceeds after repeated sell failures.

Required outcome:
owned balance remains tracked as open/reconcile-required.

### DEF-P0-011 — Fixed buy-verification wait
A fixed two-second sleep and one balance query is not sufficient transaction reconciliation.

### DEF-P0-012 — Entry price / P&L unit inconsistency
Some positions use estimated prices from reserve/market-cap math while quantities may be raw token units. Actual fill-based cost basis is required.

### DEF-P0-013 — Liquidity/exit-feasibility model can overstate safety
`src/strategy/liquidity.rs` uses virtual SOL reserves and a hard-coded six-decimal token assumption, is not quote-mint-aware, omits current trading fees, and contains `can_safely_exit()` logic where a slippage clause can evaluate true even when `exit_feasible` is false.

Required outcome:
replace with current protocol-specific executable exit quote math constrained by real/effective quote reserves, actual quote asset, token decimals, fees, and intended position size.

## P1

### DEF-P1-001 — Momentum/Survivor validator appears dormant
`MomentumValidator` has substantial survivorship logic but baseline repository search did not find it wired into live CLI execution, despite HotScan help claiming Survivor Mode.

### DEF-P1-002 — EarlyMomentum provider appears unpopulated in live path
Provider exists and is registered, but its own `record_trade()` was not found wired from the live PumpPortal event path.

### DEF-P1-003 — EarlyMomentum baseline volume is not visibly learned
`baseline_volumes` is read/created, but reviewed provider code did not show a robust update path. Fallback behavior can make volume ratio artificial.

### DEF-P1-004 — Strategy execution feedback appears dormant
`record_execution` / `record_tx_failure` exist but were not found connected to CLI execution results.

### DEF-P1-005 — Strategy exit record divergence
`record_entry()` is wired after buys, while baseline review did not find live CLI wiring to `record_exit()`.

### DEF-P1-006 — Position sizer minimum clamp hazard
If capacity is reduced below configured minimum and final code clamps back to minimum, size can exceed remaining capacity.

### DEF-P1-007 — Strategy `portfolio_remaining_sol` placeholder
Baseline strategy entry context uses a placeholder rather than actual portfolio remaining capacity.

### DEF-P1-008 — Probe documentation/runtime drift
Comments describe Probe as a much smaller learning allocation while runtime multiplier was increased significantly. Recommendation semantics and capital policy disagree.

### DEF-P1-009 — Scoring thresholds were aggressively lowered
Current defaults allow low-confidence/low-completeness early entries. Do not tune further until correctness and replay exist.

### DEF-P1-010 — Signal config parser does not cover all newer signal types
Some signal types exist in scoring defaults but cannot be overridden via the current config parser.

### DEF-P1-011 — Wallet alpha P&L is not quantity-aware enough
Current profiling matches buys/sells simplistically and uses placeholders for several lifecycle statistics. Do not use as direct sizing truth.

### DEF-P1-012 — One-trade auto-entry bypass
Trade handler can buy an unseen token after one buy threshold and minimal liquidity check, bypassing the complete canonical decision pipeline.

### DEF-P1-013 — HotScan CLI/default drift
`src/dexscreener.rs` defaults were lowered, but `main.rs` CLI defaults remained older values and override several newer defaults.

### DEF-P1-014 — Randomization is active in capital-critical entry path
`RandomizationConfig` defaults to enabled and can add 50–200 ms entry delay, ±5% size jitter and a 2% random skip. Strategy evaluation and CLI entry delay use this behavior.

Required outcome:
disable/remove from capital-critical decision and execution until a replay experiment proves positive net benefit.

### DEF-P1-015 — Chain-health sampler not found wired
`ChainHealth::sample()` can query RPC performance/priority data, but baseline runtime search did not find it scheduled; normal execution-feedback recording is also not wired. The strategy can therefore read default-looking “normal” chain state.

Required outcome:
wire real sampling/feedback or expose chain health as unavailable/uninitialized.

### DEF-P1-016 — Creator privilege checker exists but is not integrated
`CreatorPrivilegeChecker` is present but baseline repository search found it only in its module/export. Fatal-risk authority inputs are separately populated with safe `false` placeholders.

Required outcome:
feed real tri-state authority data into fatal risk; unavailable must not mean false.

## P2

### DEF-P2-001 — `Cargo.lock` ignored
For a trading executable, reproducible dependency locking should be an explicit decision; current `.gitignore` ignores the lock file.

### DEF-P2-002 — Monolithic `commands.rs`
Runtime orchestration, trading, monitoring, filtering and wallet commands are tightly coupled, increasing regression risk.

### DEF-P2-003 — Backup source tracked
`src/filter/momentum.rs.backup` can confuse source-of-truth discovery.

### DEF-P2-004 — No validated replay/backtest harness found
Baseline search found no backtest/replay system capable of evaluating historical candidates with point-in-time features and executable costs.

### DEF-P2-005 — No root CI workflow found in baseline tree
Add deterministic build/test/lint/security checks once the immediate P0 patches stabilize.

### DEF-P2-006 — Strategy tactics package is dormant/experimental
`FrontRunDetector`, `SniperPiggyback`, and the tactic-level `RugPredictor` were not found in the live engine/CLI path during baseline search.

Do not wire them merely because they exist. Validate each feature family in replay first.

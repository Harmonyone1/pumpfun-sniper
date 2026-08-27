# Source Map

## Repository evidence baseline

Repository: `Harmonyone1/pumpfun-sniper`  
Commit: `2158c64426de0f0c532221c578348c1dad1e075d`

Important source paths:
- `src/main.rs`
- `src/cli/commands.rs`
- `src/config.rs`
- `src/stream/pumpportal.rs`
- `src/stream/shredstream.rs`
- `src/filter/adaptive/mod.rs`
- `src/filter/adaptive/config.rs`
- `src/filter/scoring/mod.rs`
- `src/filter/signals/mod.rs`
- `src/filter/signals/early_momentum.rs`
- `src/filter/momentum.rs`
- `src/filter/kill_switch.rs`
- `src/filter/holder_watcher.rs`
- `src/filter/bundled_detection.rs`
- `src/filter/smart_money/alpha_score.rs`
- `src/filter/smart_money/wallet_profiler.rs`
- `src/strategy/engine.rs`
- `src/strategy/fatal_risk.rs`
- `src/strategy/regime.rs`
- `src/strategy/sizing.rs`
- `src/strategy/portfolio_risk.rs`
- `src/strategy/execution_feedback.rs`
- `src/strategy/exit_manager.rs`
- `src/strategy/liquidity.rs`
- `src/strategy/randomization.rs`
- `src/strategy/chain_health.rs`
- `src/strategy/creator_privileges.rs`
- `src/trading/pumpportal_api.rs`
- `src/trading/jito.rs`
- `src/position/manager.rs`
- `src/position/price_feed.rs`
- `src/position/auto_sell.rs`
- `src/pump/accounts.rs`
- `src/pump/instruction.rs`
- `src/dexscreener.rs`
- `src/wallet/multi_wallet.rs`
- `src/wallet/safety.rs`

## Important baseline commit history

Recent commits show the system was rapidly retuned in January 2026:
- current head lowered HotScan thresholds;
- multi-wallet verification/sell bugs were fixed after many abandoned positions;
- Local API and fee-adjusted threshold changes were made;
- Probe sizing was increased substantially.

The broad strategy package was introduced primarily in commit `41dd14cd4ba41b14069ac29ad76f9b8194067323`.

Interpretation:
historical comments/defaults can be stale. Use current code and canonical decisions, not commit-message intent alone.

## External evidence

See `14_EXTERNAL_PROTOCOL_REGISTRY.md` for source URLs and current verification date.

# Revalidation Checklist

Run this after every implementation agent merge and before issuing the next packet.

## Repository state
- [ ] Fetch latest target branch SHA.
- [ ] Compare against previous knowledge baseline.
- [ ] Review every changed file.
- [ ] Recompute line numbers for next packet.
- [ ] Update known-defect statuses: Open / In Progress / Fixed / Regressed.
- [ ] Detect newly created duplicate pathways.
- [ ] Classify every newly added file: canonical-live / canonical-unwired / experimental / legacy / unsafe.
- [ ] Confirm no secrets were introduced.
- [ ] Confirm docs/comments do not contradict runtime.

## Units
- [ ] Re-search for `/ 1e9`, `* 1e9`, `1_000_000_000`.
- [ ] Classify each conversion by source/destination unit.
- [ ] Search slippage percent/bps conversions.
- [ ] Search raw token amount multiplied by price.
- [ ] Confirm quote-asset assumptions.

## Entry path
- [ ] Enumerate every call that can send a buy.
- [ ] Confirm all automated buys pass canonical risk/decision gate.
- [ ] Confirm no one-event bypass exists.
- [ ] Confirm missing critical data is not synthesized as safe.

## Execution
- [ ] Enumerate buy/sell routes.
- [ ] Confirm mode-specific route selection.
- [ ] Confirm dynamic fee/quote behavior if implemented.
- [ ] Confirm signatures do not immediately become fills.
- [ ] Confirm pending tx recovery exists/works for modified path.

## Positions
- [ ] Enumerate all position creation sites.
- [ ] Confirm wallet identity is correct.
- [ ] Confirm reconciled cost basis.
- [ ] Enumerate all position removal/close sites.
- [ ] Confirm failed sell cannot erase owned position.

## Exit
- [ ] Enumerate all auto-exit evaluators.
- [ ] Confirm one authoritative live path.
- [ ] Confirm price freshness.
- [ ] Confirm emergency signals have priority.
- [ ] Confirm strategy portfolio exit state reconciles.

## Market data
- [ ] Verify PumpPortal/API authentication behavior.
- [ ] Verify provider event schema.
- [ ] Verify trade subscriptions are scoped/cost-aware.
- [ ] Verify reconnect/resubscribe behavior.
- [ ] Verify event lag/backpressure telemetry.

## Protocol/web revalidation
Do when relevant:
- [ ] Pump fee schedule.
- [ ] Pump IDL/instruction changes.
- [ ] PumpSwap quote math.
- [ ] PumpPortal fees/auth/pools.
- [ ] Jito/DoubleZero migration.
- [ ] Helius Sender/priority API requirements.

## Tests/evidence
- [ ] Required targeted tests pass.
- [ ] Full tests pass.
- [ ] Build/check passes.
- [ ] Any expected test failure is documented and architecture-approved.

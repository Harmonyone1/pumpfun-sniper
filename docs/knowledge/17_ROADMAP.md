# Dependency-Ordered Roadmap

The roadmap prevents agents from implementing downstream intelligence on top of corrupted truth.

## P0-A — Security and canonical units
Objectives:
- rotate/remove exposed secrets;
- canonical unit types/boundaries;
- fix PumpPortal trade SOL interpretation;
- remove unit guessing;
- current official Pump account/instruction compatibility;
- replace virtual-reserve heuristic liquidity with executable, quote-aware exit feasibility.

Exit criteria:
- provider fixture tests prove units;
- no ambiguous `/1e9` in strategy/CLI business logic;
- no tracked live secret;
- account-layout fixtures match official protocol data.

## P0-B — Execution/position truth
Objectives:
- Pending/Confirmed/Failed/ReconcileRequired transaction states;
- exact wallet identity;
- reconciled fill and cost basis;
- never record open position from HTTP signature alone;
- startup reconciliation.

Exit criteria:
- idempotency tests;
- failed/unknown buy tests;
- multi-wallet tests;
- actual-fill position creation.

## P0-C — Exit truth
Objectives:
- one live price/state source;
- one authoritative exit engine;
- correct Local/Lightning route;
- sell reconciliation;
- failed sell remains owned/tracked.

Exit criteria:
- emergency and normal exit state tests;
- partial sell tests;
- route-mode tests;
- restart with pending sell.

## P0-D — Market-event truth
Objectives:
- authenticated/cost-aware PumpPortal trade stream or chosen replacement;
- one per-token state;
- creator/holder/order-flow events;
- remove/reroute one-buy bypass;
- provider readiness.

## P0-E — Fatal risk
Objectives:
- replace placeholder context with real/unknown values;
- enforce unavailable-data policy;
- real exit-feasibility input;
- real creator privilege/authority input;
- creator/cluster conditions.

## P1-A — Unified decision pipeline
Objectives:
- eliminate circular confidence/position multiplier use;
- remove synthetic strategy inputs;
- reconcile adaptive filter + strategy responsibilities;
- activate a real readiness gate;
- either integrate Survivor logic canonically or remove claims.

## P1-B — Execution V2
Objectives:
- fresh executable quote;
- deterministic pool when known;
- edge-based slippage ceiling;
- dynamic priority/tip;
- route benchmarking;
- execution feedback wired into state;
- remove unvalidated random entry delay/size/skip behavior.

## P1-C — Candidate observation engine
Objectives:
- rolling windows;
- second-wave features;
- reserve velocity;
- entity flow;
- survivorship state.

## P2 — Recorder and replay
Objectives:
- capture all candidates;
- feature snapshots;
- decision snapshots;
- contemporaneous quotes;
- future outcomes;
- deterministic replay.

## P3 — Predictive models
Objectives:
- survival model;
- opportunity/barrier model;
- calibrated probabilities;
- walk-forward validation;
- baseline comparison.

## P4 — Capital promotion
Sequence:
- shadow;
- tiny canary;
- limited capital;
- normal capital only if promotion gates remain satisfied.

No phase should be skipped simply because a live manual test looked promising.

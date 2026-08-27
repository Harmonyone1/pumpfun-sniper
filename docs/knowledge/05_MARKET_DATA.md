# Market Data Contract

## 1. Data-quality principle

The system's edge cannot be better than the timestamped market state it observes.

A feature is not just a number. Every feature requires:
- value;
- unit;
- source;
- source event time if available;
- local receipt time;
- Solana slot/block context if available;
- freshness;
- availability state;
- calculation version.

## 2. Canonical event categories

### TokenCreated
Minimum desired fields:
- mint;
- creator;
- bonding-curve address;
- base token program;
- quote mint / quote regime;
- initial base/quote reserves;
- initial creator buy;
- token metadata URI;
- Mayhem status when observable;
- creation signature;
- creation slot;
- receipt timestamp.

### TradeObserved
Minimum desired fields:
- mint;
- trader;
- buy/sell;
- normalized base amount;
- normalized quote amount;
- raw base atoms;
- raw quote units;
- post-trade reserves if provider supplies them;
- market cap only as provider metadata, not source-of-truth price;
- signature;
- slot if known;
- source.

### HolderSnapshot / HolderDelta
- mint;
- wallet/entity;
- raw and normalized balance;
- percentage of circulating/total supply with explicit denominator;
- funding-cluster/entity id when known;
- timestamp/slot.

### CreatorState
- creator address;
- current holdings;
- prior launch statistics;
- sell events;
- funding relationships;
- authority/privilege state;
- source/freshness.

### ExecutionMarketState
- pool;
- quote mint;
- reserves;
- fee schedule;
- quote timestamp;
- route;
- network priority estimate;
- expected tip;
- estimated impact for requested size.

## 3. Candidate lifecycle

Recommended lifecycle:

```text
Discovered
→ Observing
→ Ready
→ Rejected | Eligible
→ Quoting
→ Submitted
→ Confirmed | ReconcileRequired | Failed
→ Open
→ ExitPending
→ Closed | ReconcileRequired
```

`Observing` is not a failure state. It means enough evidence has not arrived.

## 4. Rolling windows

For each mint, preserve event-time windows such as:
- 1s;
- 3s;
- 5s;
- 10s;
- 15s;
- 30s;
- 60s;
- 120s.

Candidate features include:
- buy quote volume;
- sell quote volume;
- net quote flow;
- distinct buyers;
- distinct sellers;
- new-buyer arrival rate;
- repeat-buyer share;
- median/mean buy size;
- largest buy share;
- creator flow;
- clustered-entity flow;
- reserve velocity;
- reserve acceleration;
- price return;
- drawdown from local peak;
- realized volatility;
- buy/sell imbalance;
- first-wave vs second-wave participation.

Do not hard-code all of these as entry rules. Record them first; validate predictive value.

## 5. “Second wave” definition

Second-wave demand is intended to distinguish continuing independent demand from a launch burst.

A future implementation should define it using event state, not arbitrary elapsed time. Candidate measurements:
- fraction of buyers in later window not present in initial window;
- quote flow from new independent entities after first meaningful sell wave;
- price recovery after first drawdown;
- continued reserve growth after initial-buyer concentration declines.

The exact threshold is a **hypothesis** until replay establishes it.

## 6. Bundle/entity awareness

Wallet count is not equivalent to independent demand.

Entity clustering can use:
- common funding sources;
- same-slot/same-block transactions;
- near-identical amounts;
- transaction adjacency;
- fresh-wallet creation/funding timing;
- repeated launch co-occurrence.

Cluster inference should carry uncertainty. A suspected cluster can be a risk feature; it must not be treated as absolute identity without evidence.

## 7. Provider readiness

A provider has state:
- `Healthy`;
- `Degraded`;
- `Disconnected`;
- `Unauthorized`;
- `RateLimited`;
- `Stale`.

The decision layer must know provider state.

If critical creator/holder/trade state is unavailable, readiness should block or downgrade according to an explicit policy. Never silently substitute zeros.

## 8. PumpPortal-specific baseline correction

At the inspected baseline:
- `TradeEvent.sol_amount` is documented as already SOL.
- `commands.rs` divides it by `1e9`, which is incorrect.
- correcting the conversion can activate previously unreachable `>= 0.05 SOL` trade-trigger behavior.

Therefore the unit correction and removal/rerouting of the single-trade auto-entry path must be treated as one coordinated safety change, not two unrelated patches.

## 9. External source policy

PumpPortal is useful for rapid integration, but the canonical internal data model must not be PumpPortal-shaped. Providers may be swapped without rewriting decision logic.

Jito ShredStream is not a forward canonical data dependency because its published shutdown date is 2026-09-05. See `14_EXTERNAL_PROTOCOL_REGISTRY.md`.

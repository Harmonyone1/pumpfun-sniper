# Target Architecture

## 1. High-level topology

```text
External Sources
  ├─ Pump/PumpSwap on-chain state
  ├─ PumpPortal or low-latency trade stream
  ├─ Helius/RPC account + transaction data
  └─ DexScreener (discovery/fallback only)
          │
          ▼
Canonical Ingestion + Normalization
          │
          ▼
Per-Token MarketState / CandidateState
          │
          ├─ hard risk enrichment
          ├─ holder/entity graph
          ├─ creator history
          ├─ order-flow windows
          ├─ price/liquidity windows
          └─ execution quote state
          │
          ▼
Readiness Gate
          │
          ▼
Fatal Risk Gate ────── reject
          │
          ▼
Survival Model ─────── reject/observe
          │
          ▼
Opportunity Model
          │
          ▼
Net Edge + Portfolio Gate
          │
          ▼
Fresh Executable Quote
          │
          ├─ price impact budget
          ├─ route fee budget
          ├─ priority/tip estimate
          └─ quote freshness
          │
          ▼
Execution Router
          │
          ▼
Pending Transaction State
          │
          ▼
On-chain / Wallet Reconciliation
          │
      ┌───┴────┐
      ▼        ▼
Confirmed    Failed/Unknown
Position     Reconcile
      │
      ▼
Single Authoritative Exit Engine
      │
      ▼
Sell Execution + Reconciliation
      │
      ▼
Closed Position / Realized P&L
          │
          ▼
Event Recorder + Dataset + Replay
```

## 2. Service boundaries

### `MarketIngestion`
Responsibilities:
- maintain external connections;
- parse events;
- attach provider timestamp / local receipt timestamp / slot if known;
- convert provider representations into canonical typed domain events;
- never score or trade.

### `MarketStateStore`
Responsibilities:
- one state object per mint;
- append trades;
- maintain rolling windows;
- track creator/holder state;
- track candidate lifecycle;
- expose immutable feature snapshots.

### `RiskService`
Responsibilities:
- fatal-risk inputs;
- creator privileges;
- holder/entity concentration;
- exit feasibility;
- provider readiness requirements.

### `DecisionService`
Responsibilities:
- readiness;
- survival score/probability;
- opportunity score/probability;
- expected net edge;
- reasons;
- model version;
- no transaction sending.

### `QuoteService`
Responsibilities:
- current pool/regime detection;
- exact quote calculation or transaction build quote;
- expected price impact;
- fee schedule;
- minimum acceptable output / maximum quote input;
- quote expiration.

### `ExecutionService`
Responsibilities:
- select allowed route;
- build/send;
- dynamic priority/tip;
- tx lifecycle;
- no position creation until reconciliation.

### `ReconciliationService`
Responsibilities:
- signature status;
- wallet SOL/quote deltas;
- token balance deltas;
- fees;
- actual fill;
- idempotency;
- recovery after restart.

### `PositionService`
Responsibilities:
- confirmed position truth;
- wallet ownership;
- cost basis;
- partial fills/exits;
- persistence.

### `ExitService`
Responsibilities:
- fatal exits;
- liquidity/creator/holder exits;
- stop/trailing/profit logic;
- stale-data behavior;
- invoke `ExecutionService` for sell;
- never directly mark closed before reconciliation.

### `Recorder`
Responsibilities:
- every candidate;
- every feature snapshot;
- every decision;
- every quote;
- every send/confirm/failure;
- future labels/outcomes.

## 3. Dependency direction

Canonical dependency direction:

```text
providers → domain events → state → features → decision → quote → execution → reconciliation → position
```

Never:
```text
position multiplier → fake organic score → regime → confidence → position multiplier
```

No decision component should fabricate upstream evidence to satisfy another component.

## 4. One source of truth per concept

| Concept | Canonical source |
|---|---|
| token event units | normalized domain event |
| current token state | `MarketStateStore` |
| entry decision | `DecisionService` |
| fatal safety | `RiskService` |
| transaction state | `ExecutionService` + `ReconciliationService` |
| open positions | `PositionService` |
| exit decision | `ExitService` |
| realized P&L | reconciled fills |
| model outcome dataset | `Recorder` |

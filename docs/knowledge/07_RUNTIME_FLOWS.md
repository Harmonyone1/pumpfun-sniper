# Canonical Runtime Flows

## A. New-token flow

```text
1. Receive TokenCreated.
2. Normalize units and quote regime.
3. Create CandidateState with creation slot/time.
4. Start/selectively subscribe to trade stream.
5. Queue non-blocking enrichment.
6. Enter Observing state.
7. Update rolling market state on each event.
8. Evaluate readiness after relevant state transitions.
9. Run fatal-risk gate.
10. Run survival + opportunity decision.
11. If not eligible: remain Observing or Reject.
12. If eligible: request fresh executable quote.
13. Re-evaluate net edge after quote/costs.
14. Submit only if quote remains inside budget.
```

Creation itself is not sufficient evidence to buy.

## B. Trade-event flow

```text
Provider TradeEvent
→ normalize once
→ append to MarketState
→ update creator/holder/entity/order-flow windows
→ feed kill-switch state for owned positions
→ update candidate readiness
→ emit feature snapshot if decision boundary reached
```

There is **no separate “single buy >= X SOL → execute buy” branch**.

## C. Buy flow

```text
EligibleDecision
→ QuoteService
→ ExecutionPlan
→ portfolio reservation
→ tx build
→ dynamic fee/tip
→ send
→ PendingBuy
→ confirmation/reconciliation
```

Outcomes:
- Confirmed fill → create/open position from actual deltas.
- Explicit failure → release reservation; record failure.
- Unknown/timeout → `ReconcileRequired`; do not assume success or failure.

## D. Reconciliation flow

For a pending buy:
1. Query signature status.
2. Inspect transaction/meta when available.
3. Query the exact signing wallet token balance/delta.
4. Query quote/SOL delta.
5. Resolve actual base quantity.
6. Resolve fees.
7. Store fill.
8. Create position idempotently.

A second reconciliation pass must not duplicate the position.

## E. Open-position flow

Each position continuously receives:
- market price/reserve state;
- creator/holder sell events;
- liquidity/exit feasibility;
- execution-quality/network state;
- peak/drawdown state.

Exit evaluator returns a reasoned action:
- hold;
- partial exit;
- full normal exit;
- emergency exit.

## F. Sell flow

```text
ExitDecision
→ fresh sell quote
→ route selection
→ PendingSell
→ send
→ reconcile actual sold quantity and quote received
→ update/close position
```

A failed sell remains:
```text
Open or ExitPending + ReconcileRequired
```
never “closed with zero proceeds” solely because retry count was exhausted.

## G. Restart recovery

On startup:
1. load persisted pending executions;
2. load positions;
3. reconcile wallet balances;
4. reconcile pending signatures;
5. detect wallet-owned token balances not represented by positions;
6. surface discrepancies;
7. block new risk if position truth is unresolved beyond configured tolerance.

## H. Shutdown

Shutdown must:
- stop initiating new entries;
- persist state;
- keep or explicitly hand off unresolved pending transaction state;
- never silently discard owned positions.

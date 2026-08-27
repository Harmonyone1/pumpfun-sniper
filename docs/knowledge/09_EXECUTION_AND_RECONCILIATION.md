# Execution and Reconciliation

## 1. Execution objective

Maximize expected net value of *landed* trades, not raw landing probability.

A transaction that lands at a price outside the strategy's edge budget is an execution failure even if the chain marks it successful.

## 2. Pre-send execution plan

Each entry creates an immutable `ExecutionPlan` containing:
- decision id;
- mint;
- wallet;
- pool/regime;
- intended quote amount;
- fresh reference quote;
- minimum base output / maximum quote input as appropriate;
- max adverse slippage;
- predicted protocol fees;
- route fee;
- priority fee;
- tip;
- quote timestamp;
- quote expiration;
- expected total cost;
- maximum total cost allowed by edge.

## 3. Route selection

Potential routes may include:
- PumpPortal Lightning;
- PumpPortal Local + chosen sender;
- direct official Pump transaction construction + selected sender.

Do not assume one route is always superior. Record route-specific metrics and benchmark.

For a known brand-new Pump bonding-curve token, avoid `pool:auto` unless pool ambiguity actually exists. PumpPortal states that `auto` may add latency.

## 4. Dynamic priority fees

Static priority fees are not canonical.

Prefer transaction/account-aware estimation using current network conditions. Helius exposes transaction-specific priority fee estimation.

Execution policy may define:
- normal;
- urgent entry;
- emergency exit

priority levels, but all should remain bounded by the trade/risk budget.

## 5. Tips

Tips are a cost, not an independent magic speed knob.

Record:
- configured tip;
- sender requirement;
- actual transfer;
- route.

Emergency exit may rationally spend more than normal entry because downside risk differs.

## 6. Slippage

Never implement:
```text
failure → +15% slippage → retry → +15% ...
```
without an edge-based ceiling.

Instead:
1. get fresh quote;
2. compute updated expected edge;
3. if still positive above margin, rebuild and resend;
4. otherwise abandon the **entry**.

For an **exit**, policy differs: minimizing catastrophic loss can justify a larger execution tolerance, but it must be an explicit emergency-exit rule.

## 7. Preflight

Preflight strategy is route/mode-specific:
- debugging/canary may favor simulation;
- ultra-low-latency path may skip preflight after deterministic transaction validation;
- failed transactions must still be recorded by error class.

Do not globally equate `skip_preflight=true` with reliability.

## 8. Transaction state machine

```text
Planned
→ Built
→ Submitted
→ Landed
→ Reconciled
```

Alternative outcomes:
```text
BuildFailed
SendFailed
Expired
ChainFailed
Unknown
ReconcileRequired
```

A signature returned by an HTTP service maps to `Submitted`, not automatically `Reconciled`.

## 9. Buy reconciliation

Required:
- exact wallet used to sign/hold;
- token pre/post balance;
- quote/SOL pre/post delta;
- transaction fees;
- route fee if external;
- actual base received.

The old fixed “sleep 2 seconds then token balance” model is insufficient as the sole truth mechanism.

## 10. Sell reconciliation

A sell is complete only when actual wallet/token and quote deltas confirm the fill.

After partial sell:
- subtract actual base quantity sold;
- allocate proportional cost basis;
- record actual quote received;
- preserve remaining position.

## 11. Failure policy

Entry:
- bounded retry if quote and blockhash are fresh and edge remains valid;
- otherwise fail without position.

Exit:
- retry/rebuild according to urgency;
- if unresolved, remain owned and tracked;
- escalate state/alert;
- never remove the position merely to stop retrying.

## 12. Required execution telemetry

Per attempt:
- decision id;
- execution id;
- route;
- wallet;
- blockhash;
- build start/end;
- send start/end;
- signature;
- landing slot;
- final status;
- reference quote;
- realized fill;
- realized slippage;
- protocol/route/network/tip costs;
- failure class;
- retry linkage.

## 13. Baseline hazards to eliminate

- Local trader path attempting Lightning method first in some monitor logic.
- Static priority fee.
- `pool:auto` in paths where pool is known.
- unused retry method allowing 30–75% slippage.
- two-second balance verification as final truth.
- removing positions after exhausted sell retries.

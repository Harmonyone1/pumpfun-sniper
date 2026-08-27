# Position, Portfolio Risk, and Exit

## 1. Position identity

A position must contain:
- position id;
- mint;
- quote asset;
- wallet public key;
- entry execution ids;
- raw + normalized base quantity;
- token decimals;
- reconciled total cost basis;
- entry time/slot;
- current state;
- exit execution ids;
- realized/unrealized P&L components.

## 2. Position state

Recommended:
```text
PendingOpen
Open
Partial
ExitPending
ReconcileRequired
Closed
```

Do not persist a trade as `Open` before the entry is reconciled.

## 3. Portfolio truth

Portfolio risk must use the same position source of truth as live execution.

Metrics:
- open count;
- total cost exposure;
- current executable exit value;
- unrealized P&L;
- realized daily loss;
- unresolved execution exposure;
- per-wallet exposure;
- per-token exposure.

`portfolio_remaining` must be computed, not placeholder `1.0`.

## 4. Risk reservation

Before entry submission, reserve intended capital so concurrent candidates cannot each see the same free capacity.

Release on explicit failure. Convert reservation to position exposure on reconciliation.

## 5. Exit priority

Suggested hierarchy:

1. **Fatal/emergency**
   - creator/cluster dump;
   - sellability/exit collapse;
   - severe liquidity collapse;
   - authority/honeypot condition discovered;
   - catastrophic market-state trigger.

2. **Risk preservation**
   - hard downside/barrier;
   - rapidly deteriorating order flow;
   - failure of expected survivorship;
   - excessive drawdown.

3. **Profit protection**
   - trailing logic;
   - partial profit;
   - weakening momentum after positive move.

4. **Opportunity-cost/time**
   - only if validated; time alone is not proof of failure.

## 6. First-seconds protection

The baseline live monitor contains a 10-second confirmation wait before it evaluates ordinary position exits. That creates a dangerous blind interval for extremely short-lived Pump tokens.

Canonical solution is not “remove the delay and hope”.
Instead:
- do not declare the position open until buy reconciliation;
- once reconciled, subscribe/update live state immediately;
- emergency creator/liquidity events may act as soon as ownership is known;
- exit engine uses fresh market state, not a stale persisted default.

## 7. Price truth

For on-curve tokens:
- use current official account/event layout;
- quote with correct base/quote reserves and fees;
- do not rely on the baseline handwritten stale `BondingCurve` layout.

For graduated PumpSwap:
- use effective quote reserves where current protocol requires/anticipates them;
- DexScreener can be fallback/discovery, not the sole high-frequency exit truth if a direct on-chain quote is available.

## 8. Partial exits

Every partial exit:
- reconciles actual base sold;
- updates remaining base;
- allocates cost basis;
- records realized P&L;
- preserves remaining position;
- marks exit-level state idempotently.

## 9. Portfolio kill switches

Global limits can include:
- max concurrent positions;
- max total exposure;
- max per-token risk;
- max daily realized loss;
- consecutive-loss cooldown;
- network/route health pause;
- unresolved-position pause.

These should be deterministic and auditable.

## 10. Wallet ownership

Multi-wallet support makes wallet identity mandatory in:
- balance verification;
- ATA lookup;
- signing;
- sell routing;
- P&L;
- reconciliation.

Never fall back to an unrelated default wallet when parsing/storing wallet identity fails. That should be an error requiring reconciliation.

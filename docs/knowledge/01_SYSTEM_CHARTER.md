# System Charter

## 1. Mission

Build a Pump.fun trading system that selects and executes short-horizon trades with **positive net expectancy after all observable costs and failures**, while aggressively avoiding catastrophic loss modes.

This is not a generic “sniper” whose objective is to buy earliest. The system should be:

- fastest at rejecting structurally bad launches;
- patient when the evidence is incomplete;
- fast only after sufficient evidence of executable edge exists;
- exact about position ownership, fill state, and P&L;
- measurable through replay and shadow execution before capital promotion.

## 2. Primary optimization objective

The primary objective is not:
- raw win rate;
- number of trades;
- graduation rate;
- entry speed;
- gross percentage gain;
- model score.

The target is **risk-adjusted, cost-adjusted realized expectancy**.

For a candidate at decision time `t`:

```text
expected_net_edge =
    expected_gross_return
  - protocol_fees
  - route_fees
  - expected_priority_fee
  - expected_tip
  - expected_price_impact
  - expected_latency_adverse_move
  - expected_failure/retry_cost
```

The trade is eligible only if:
1. fatal safety gates pass;
2. catastrophic-loss/exit-failure probability is below the configured risk ceiling;
3. expected net edge exceeds a configured safety margin;
4. the quote is executable inside the edge budget;
5. portfolio risk allows the position.

## 3. Two separate prediction questions

The system must not collapse these into one score.

### Survival / catastrophic-risk question
“What is the probability this token suffers a severe drawdown, liquidity collapse, insider/deployer dump, exit failure, or rug-like outcome inside horizon H?”

### Opportunity question
“Conditional on passing hard safety, what is the expected executable net return and probability that the upside barrier is reached before the downside barrier inside horizon H?”

A token can have strong momentum and still fail survival. Momentum must never numerically cancel a fatal risk.

## 4. Definition of a winning trade

A trade is a win only from **actual reconciled wallet deltas and transaction costs**.

A hypothetical mark-to-market gain is not a realized win.

At minimum, realized P&L must account for:
- actual quote asset debited on entry;
- actual base token quantity received;
- Pump protocol fees;
- PumpPortal/route fees when applicable;
- Solana base/priority fees;
- Jito/Sender tips when applicable;
- actual quote asset received on exit;
- partial exits;
- unrecovered/dust balances.

## 5. Non-goals

The canonical system is not intended to:
- guarantee profit;
- guarantee fills;
- chase every launch;
- widen slippage until an order lands;
- infer “safe” from missing critical data;
- use unvalidated wallet-copy scores as a substitute for market state;
- use random aggressiveness as an edge;
- preserve legacy code for compatibility when it contradicts position truth or risk truth.

## 6. Capital-promotion philosophy

New decision behavior progresses through:

```text
offline replay
→ out-of-sample walk-forward validation
→ live shadow mode
→ tiny-capital canary
→ limited capital
→ normal capital
```

Promotion requires measured evidence. A successful compilation or a handful of profitable live trades is not evidence of durable edge.

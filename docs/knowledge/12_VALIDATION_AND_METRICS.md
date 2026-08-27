# Validation and Metrics

## 1. Validation ladder

### Level 0 — compile/static correctness
- `cargo fmt --check`
- `cargo check`
- `cargo clippy` with agreed warning policy
- unit tests

Necessary, never sufficient for trading correctness.

### Level 1 — deterministic fixtures
Provider fixtures test:
- units;
- parsing;
- account layouts;
- fee math;
- quote math;
- state transitions;
- reconciliation idempotency.

### Level 2 — event replay
Replay captured historical events in event-time order.

Must reproduce:
- candidate states;
- decisions;
- simulated executable quotes;
- hypothetical fills using conservative assumptions;
- future outcome labels.

### Level 3 — walk-forward model evaluation
Example:
```text
train past window → validate next window → test following window → roll forward
```

Never randomly shuffle launches across time for primary performance claims.

### Level 4 — live shadow
Consume live production feeds and make decisions without submitting trades.

Record contemporaneous quotes so results are not based on future/ideal prices.

### Level 5 — tiny-capital canary
Strict caps and automatic stop conditions.

### Level 6 — limited/normal capital
Only after sufficient sample and stable execution metrics.

## 2. Trading metrics

Always report net-of-cost metrics:
- realized net P&L;
- expectancy per trade;
- median/mean return;
- profit factor;
- max drawdown;
- loss-tail quantiles;
- catastrophic-loss frequency;
- win rate (secondary);
- average holding time.

## 3. Selection metrics

By decision score/probability bucket:
- precision of positive-net outcomes;
- barrier-hit rates;
- severe-loss rate;
- calibration;
- coverage/trade frequency;
- performance by token age;
- performance by quote asset;
- performance by protocol regime;
- performance by Mayhem status.

## 4. Execution metrics

- quote-to-send latency;
- send-to-land latency;
- slots to land;
- fill rate;
- chain-failure rate;
- unknown/reconciliation rate;
- realized slippage;
- price impact;
- priority fee;
- tip;
- route fee;
- total cost;
- failure reason distribution;
- metrics by route and network condition.

## 5. Data metrics

- provider uptime;
- reconnect count;
- event lag;
- dropped/backpressured event count;
- stale-feature rate;
- missing critical feature rate;
- duplicate event rate;
- reconciliation discrepancy rate.

## 6. Model calibration

If the model says “70% probability”, roughly 70% of comparable out-of-sample cases should satisfy the defined label.

Track:
- reliability diagrams;
- Brier score;
- log loss;
- calibration error;
- calibration drift over time.

## 7. Model comparison

Every new model must beat:
1. no-trade baseline;
2. current heuristic scoring baseline;
3. simple momentum/liquidity baseline.

Compare on **net expectancy and downside**, not only classification accuracy/AUC.

## 8. Promotion gate

A release cannot be promoted on “looks better”.

Promotion packet should state:
- exact code/model version;
- data window;
- number of candidates;
- number of eligible trades;
- out-of-sample performance;
- execution assumptions;
- worst drawdown/loss tail;
- known regime weaknesses;
- rollback threshold.

## 9. Avoiding selection bias

Record every candidate, including:
- rejected;
- observed;
- skipped for portfolio capacity;
- skipped for execution cost;
- failed sends.

If only bought tokens are retained, the model cannot learn the true opportunity set.

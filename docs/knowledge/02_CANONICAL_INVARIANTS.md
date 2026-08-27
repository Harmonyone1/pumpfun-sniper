# Canonical Invariants

These rules are architecture-level contracts. An implementation packet must explicitly call out any proposed exception.

## A. Money and units

**INV-UNIT-001** — A value may not cross a module boundary without a defined unit.

**INV-UNIT-002** — `SOL` and `Lamports` are distinct domain types. `1 SOL = 1_000_000_000 lamports` conversion occurs only at explicit boundary functions.

**INV-UNIT-003** — Raw token atoms and normalized token quantity are distinct.

**INV-UNIT-004** — Percent and basis points are distinct. Slippage expressed as percent cannot be passed into a bps field without an explicit conversion.

**INV-UNIT-005** — Prices must name both numerator and denominator, e.g. `SolPerToken`, not generic `f64 price`.

## B. Market data

**INV-DATA-001** — Every decision feature must have provenance and observation timestamp/slot.

**INV-DATA-002** — A feature snapshot must contain only information available at or before decision time.

**INV-DATA-003** — Missing critical risk data cannot default to a safe value such as `false`, `0`, empty creator, or zero slippage.

**INV-DATA-004** — Trade events must be ingested once into a canonical per-token state and then consumed by scoring, kill switches, model features, and telemetry. Parallel reinterpretations are forbidden.

**INV-DATA-005** — Provider disconnection or authentication failure must be observable and must change readiness state.

## C. Entry

**INV-ENTRY-001** — No trade may be initiated from a one-off event bypassing the canonical candidate pipeline.

**INV-ENTRY-002** — Fatal risk is a veto, not a weighted feature.

**INV-ENTRY-003** — Confidence is not position size, and position-size multiplier is not confidence.

**INV-ENTRY-004** — The strategy layer may not create synthetic “organic”, distribution, creator, or price-action values to satisfy an interface.

**INV-ENTRY-005** — An entry decision is valid only while its executable quote remains inside the decision's price-impact and cost budget.

## D. Execution

**INV-EXEC-001** — Failed landing is not solved by automatically increasing slippage beyond the expected edge.

**INV-EXEC-002** — A returned transaction signature is not a confirmed fill.

**INV-EXEC-003** — Buy state must progress through explicit pending/reconciliation states before becoming an open position.

**INV-EXEC-004** — Entry cost basis comes from reconciled transaction/wallet deltas, not estimated market cap or virtual reserve ratios.

**INV-EXEC-005** — Every route records build latency, send latency, confirmation latency, slot, configured and realized price impact, fees, and failure reason.

## E. Positions and exits

**INV-POS-001** — Wallet ownership is part of the position identity.

**INV-POS-002** — A failed sell never causes an owned position to disappear from tracking.

**INV-POS-003** — A partial sell must preserve remaining quantity and proportional cost basis using actual sold quantity.

**INV-POS-004** — There is one authoritative live exit decision path.

**INV-POS-005** — Emergency/fatal exits outrank profit optimization.

**INV-POS-006** — Price data used for an exit must have a freshness bound. Stale or unavailable price state must be explicit.

**INV-POS-007** — Strategy portfolio state and actual position-manager state must reconcile; they may not maintain divergent open-position truth.

## F. Modeling

**INV-MODEL-001** — Training data contains bought and skipped candidates.

**INV-MODEL-002** — Random train/test splits are forbidden for time-series market validation. Use chronological/walk-forward splits.

**INV-MODEL-003** — Labels use executable/reconciled prices and costs whenever available.

**INV-MODEL-004** — A model version cannot be promoted from replay directly to normal capital.

**INV-MODEL-005** — Mayhem and materially different protocol regimes must be represented explicitly, excluded, or modeled separately.

## G. Security

**INV-SEC-001** — No secret may exist in tracked config, docs, examples, tests, logs, prompts, or artifacts.

**INV-SEC-002** — A secret observed in public Git history must be treated as compromised even after removal from the current file.

**INV-SEC-003** — Agents never print secret values while explaining remediation.

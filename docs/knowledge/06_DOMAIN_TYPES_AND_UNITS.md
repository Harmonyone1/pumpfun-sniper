# Domain Types and Units

## 1. Why this document exists

The baseline repository contains contradictory assumptions about:
- SOL vs lamports;
- raw token units vs normalized tokens;
- percent vs basis points;
- virtual reserves vs real reserves;
- market-cap-derived price vs actual fill price.

These errors can alter thresholds by factors of `1e9` and corrupt both trading and training labels.

## 2. Required strong types

Exact Rust naming may differ, but the semantics must be explicit.

```rust
struct Lamports(u64);
struct Sol(f64);

struct RawTokenAmount(u64);
struct TokenAmount(f64);
struct TokenDecimals(u8);

struct BasisPoints(u32);
struct Percent(f64);

struct SolPerToken(f64);
struct QuotePerToken(f64);

struct Slot(u64);
```

For non-SOL quote assets, prefer:
```rust
enum QuoteAsset {
    Sol,
    SplToken { mint: Pubkey, decimals: u8 },
}
```

And:
```rust
struct RawQuoteAmount(u64);
struct QuoteAmount(f64);
```

## 3. Boundary conversions

Allowed only in named conversion functions.

```text
lamports_to_sol
sol_to_lamports
raw_token_to_token_amount
token_amount_to_raw
bps_to_fraction
percent_to_fraction
```

Do not scatter `/ 1e9` or `* 1e9` through strategy/CLI code.

## 4. Reserve semantics

Distinguish:
- virtual base reserves;
- virtual quote reserves;
- real base reserves;
- real quote reserves;
- PumpSwap raw vault reserves;
- PumpSwap effective quote reserves.

Price calculations must state which reserve model they use.

The current Pump public docs support quote-mint-aware V2 trading. The target system cannot assume every Pump launch is SOL-paired.

## 5. Token decimals

Never calculate:
```text
SOL_per_token = SOL / raw_token_atoms
```
without normalizing token decimals.

Persist both raw amount and decimals/normalized amount when available.

## 6. Fill price

Canonical entry price:

```text
effective_entry_quote_cost / normalized_base_received
```

where effective entry cost separately records:
- quote sent to swap;
- protocol/creator fees if measurable;
- route fee;
- network fee;
- priority fee;
- tip;
- ATA/rent if one-time infrastructure cost is intentionally included/excluded by reporting policy.

The dataset must preserve the components so analytics can choose economic vs trading-only cost views.

Canonical exit price:
```text
effective_quote_received / normalized_base_sold
```

## 7. Slippage language

Maintain three distinct values:

1. `expected_price_impact` — curve/pool impact of our order.
2. `max_adverse_slippage` — protection limit relative to a fresh quote.
3. `realized_slippage` — actual fill vs reference quote.

Do not call all three “slippage”.

## 8. Configuration units

Config names should expose units, e.g.:
- `max_adverse_slippage_bps`;
- `priority_fee_lamports`;
- `jito_tip_lamports`;
- `buy_amount_sol`;
- `max_quote_age_ms`.

Avoid ambiguous fields such as `fee`, `price`, `amount`, `slippage` at internal boundaries.

## 9. Assertions and tests

Every provider adapter should have fixture tests proving unit interpretation.

Required regression examples:
- `TradeEvent.sol_amount = 1.0` remains `1.0 SOL`;
- `50_000_000 lamports = 0.05 SOL`;
- token atom conversion uses token decimals;
- 2500 bps = 25%, but an internal percent field of `25.0` is not 2500 bps until explicitly converted.

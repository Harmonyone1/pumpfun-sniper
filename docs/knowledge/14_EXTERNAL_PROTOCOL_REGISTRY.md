# External Protocol and Provider Registry

**Last web verification:** 2026-08-27

This file is intentionally time-sensitive. Revalidate before implementing provider/protocol changes.

## Pump.fun bonding curve and fees

### Verified facts
- Pump bonding curve is constant-product style using virtual reserves.
- Current Pump docs state a **1.25% total bonding-curve trading fee**.
- Current fee page supports SOL- and USDC-paired launches.
- Current public docs expose quote-mint-aware V2 trading instructions.
- Newer Pump docs describe `virtual_quote_reserves` / `real_quote_reserves` semantics.

### Sources
- https://pump.fun/docs/bonding-curve
- https://pump.fun/docs/fees
- https://github.com/pump-fun/pump-public-docs
- https://github.com/pump-fun/pump-public-docs/blob/main/docs/instructions/BUY.md
- https://github.com/pump-fun/pump-public-docs/blob/main/docs/instructions/SELL.md
- https://github.com/pump-fun/pump-public-docs/blob/main/idl/pump.json

### Implementation policy
Use official current IDL/client definitions as primary protocol truth rather than handwritten stale layouts.

## PumpSwap

### Verified fact
Current public docs describe effective quote reserves as raw quote-vault balance plus `virtual_quote_reserves`, and recommend quoting against effective reserves.

Source:
- https://github.com/pump-fun/pump-public-docs/blob/main/docs/PUMP_SWAP_README.md

## Pump Mayhem Mode

### Verified facts
Pump docs describe a Mayhem agent that can trade eligible Mayhem-enabled coins during the first 24 hours, creating activity that should not automatically be interpreted as organic user demand.

Sources:
- https://pump.fun/docs/mayhem-mode
- https://pump.fun/docs/mayhem-mode-disclaimer

### Modeling policy
Mayhem must be explicit in the feature/regime model or excluded from the initial ordinary-launch training set.

## PumpPortal Data API

### Verified facts
Effective 2026-05-01:
- `subscribeNewToken` and `subscribeMigration` are free.
- `subscribeTokenTrade` and `subscribeAccountTrade` require an API key and are metered.
- Current documented WS URL includes `?api-key=...`.
- PumpPortal recommends one WebSocket connection with dynamic subscriptions.

Sources:
- https://pumpportal.fun/fees/
- https://pumpportal.fun/data-api/bonk-fun-data-api/
- https://pumpportal.fun/FAQ/

### Baseline mismatch
The inspected repo hard-codes unauthenticated:
`wss://pumpportal.fun/api/data`
and describes the API as free while subscribing to trade streams.

## PumpPortal Trading

### Verified fees
- Local Transaction API: 0.5% PumpPortal fee.
- Lightning Transaction API: 1% PumpPortal fee.
These are in addition to Pump/Solana costs.

Source:
- https://pumpportal.fun/fees/

### Verified execution notes
- Lightning supports explicit `pool`; default documented as `pump`.
- PumpPortal FAQ states `pool:auto` may add up to about 100ms because additional pool data may be needed.
- Local API returns a serialized transaction for caller signing/sending.

Sources:
- https://pumpportal.fun/trading-api/
- https://pumpportal.fun/local-trading-api/trading-api/
- https://pumpportal.fun/FAQ/

## Jito ShredStream

### Verified fact
Jito documentation states ShredStream will be completely shut down on **2026-09-05** and recommends migrating to DoubleZero Edge.

Source:
- https://docs.jito.wtf/lowlatencytxnfeed/

### Implementation policy
Do not spend new engineering effort completing baseline ShredStream integration.

## Helius priority fees / Sender

### Verified facts
Helius documents:
- transaction-specific priority fee estimation;
- account-lock-aware fee estimation;
- Sender low-latency transaction submission;
- Sender requirements involving priority fees and tips.

Sources:
- https://www.helius.dev/docs/priority-fee/estimating-fees-using-serialized-transaction
- https://www.helius.dev/docs/api-reference/priority-fee/getpriorityfeeestimate
- https://www.helius.dev/docs/sending-transactions/sender
- https://www.helius.dev/docs/faqs/sender

### Implementation policy
Treat Helius as one candidate execution/data provider, not a permanent dependency. Benchmark route performance and preserve provider abstraction.

## Revalidation triggers

Recheck this registry when:
- Pump changes fees;
- Pump modifies program/IDL/instructions;
- PumpPortal changes auth, fee, pool or WS semantics;
- Jito migration date passes;
- sender/priority requirements change;
- a new quote asset or launch regime appears;
- unexplained parsing/fill-rate changes occur.

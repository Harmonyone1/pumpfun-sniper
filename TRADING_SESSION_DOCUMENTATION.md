# Pump.fun Trading Bot - Session Documentation

## Executive Summary

**Starting Capital**: 0.3732 SOL
**Current Balance**: 0.7279 SOL (+95% from start)
**Goal**: 100x returns (37.32 SOL)
**Active Positions**: None (all positions closed)

### Latest Session Activity
The Rust bot executed several trades:
- HARRY: Bought 0.02 SOL, sold at loss (-0.02 SOL P&L)
- Multiple other positions opened/closed with small losses
- Auto-sell system is functioning (stop-loss triggered)

---

## Project Architecture

### Infrastructure Overview

The project has **two distinct trading systems**:

#### 1. JavaScript Scripts (Legacy - Slower)
- Location: `D:\pumpfun\scripts\`
- Files: `scan_hot.js`, `quick_buy.js`, `quick_sell.js`
- Method: HTTP polling every 15+ seconds
- Issue: **Too slow** - tokens move significantly before detection

#### 2. Rust Bot (Primary - Fast)
- Location: `D:\pumpfun\src\`
- Method: **WebSocket real-time streaming** via PumpPortal
- Detection: Sub-second token discovery
- Execution: Via PumpPortal Local API

### Key Rust Components

```
src/
├── stream/pumpportal.rs      # WebSocket client for real-time data
├── strategy/
│   ├── engine.rs             # Strategy coordinator
│   ├── liquidity.rs          # Slippage calculator
│   ├── regime.rs             # Token regime classification
│   └── fatal_risk.rs         # Kill switch for toxic tokens
├── filter/
│   ├── adaptive.rs           # Signal-based scoring
│   ├── momentum.rs           # Pre-entry activity validation
│   └── holder_watcher.rs     # Top holder monitoring
├── trading/pumpportal_api.rs # Trade execution via Local API
├── position/manager.rs       # Position tracking
└── cli/commands.rs           # Main trading loop
```

---

## Critical Findings

### 1. Slippage is Fundamental, Not a Bug

**The Problem**: Even with 0.05-0.08 SOL positions, we were seeing 30-50% slippage.

**Root Cause**: Pump.fun uses a bonding curve (constant product AMM: x * y = k). The math means:
- Early buyers get exponentially better prices
- Even $80k+ liquidity doesn't help if price has moved
- **Slippage is unavoidable** - it's how the AMM works

**Solution Applied**:
- Reduced position sizes to 0.02 SOL
- Accept higher loss rate on individual trades
- Rely on faster detection to catch tokens earlier in their lifecycle

### 2. Speed is Everything

**Discovery**: The 15+ second polling delay was fatal:
- By the time JS scripts detected tokens, prices had already moved 10-50%
- Other traders (snipers) front-run using WebSocket streams

**Solution**: Use the existing Rust WebSocket infrastructure:
- PumpPortal WebSocket: `wss://pumpportal.fun/api/data`
- Subscriptions: `subscribeNewToken`, `subscribeTokenTrade`
- Detection time: <1 second from token creation

### 3. Momentum Validation Prevents Buying Dead Tokens

The bot waits for **real activity** before entering:
```rust
// Minimum thresholds before entry
min_trades: 3,
min_volume_sol: 0.2,
min_price_change_pct: 2.0,
min_unique_traders: 2,
min_buy_ratio: 50%,
```

This prevents buying tokens that never gain traction.

### 4. B/S Ratios Are Often Fake

Many tokens show high buy/sell ratios but use **wash trading** (circular trades to inflate metrics). The adaptive filter attempts to detect this but it's an ongoing challenge.

---

## Current Configuration

### config.toml Settings

```toml
[trading]
buy_amount_sol = 0.02          # Reduced from 0.05 to minimize slippage impact
slippage_bps = 2500            # 25% slippage tolerance
simulate_before_send = false   # Skip simulation for speed

[auto_sell]
enabled = true
take_profit_pct = 50.0         # Sell at +50% profit
stop_loss_pct = 30.0           # Sell at -30% loss
price_poll_interval_ms = 1000  # Check price every second

[safety]
max_position_sol = 0.5         # Max single position
daily_loss_limit_sol = 1.0     # Pause if daily loss exceeds

[filters]
min_liquidity_sol = 0.5
max_dev_holdings_pct = 20.0
blocked_patterns = ["(?i)scam", "(?i)rug", "(?i)honeypot", "(?i)test"]
```

### Wallet Information

- **Hot Wallet**: `C9ibhqLMz68HewsMXiZyXVAiJ68uLg53vSsSuyLQWYA6`
- **Keypair Location**: `credentials/hot-trading.json`
- **RPC**: Helius mainnet with WebSocket support

---

## How to Run the Bot

### Start Rust Bot (Primary Method)
```bash
cd D:\pumpfun
cargo build --release
.\target\release\pumpfun-sniper.exe snipe start
```

### Start with Dry Run (Testing)
```bash
.\target\release\pumpfun-sniper.exe snipe start --dry-run
```

### Monitor Output
The bot outputs to console and can be monitored via temp files if run in background.

### JavaScript Scripts (Backup/Manual)
```bash
# Scan for opportunities
node scripts/scan_hot.js

# Manual buy
node scripts/quick_buy.js <MINT_ADDRESS> <SOL_AMOUNT>

# Manual sell
node scripts/quick_sell.js <MINT_ADDRESS> [PERCENTAGE]
```

---

## Trading Flow (Rust Bot)

1. **Token Detection** (WebSocket)
   - PumpPortal streams new token events
   - Bot receives within <1 second of creation

2. **Initial Filter**
   - Name/symbol pattern matching
   - Blocked pattern rejection

3. **Adaptive Scoring**
   - 11+ signals analyzed (holder concentration, liquidity, etc.)
   - Score determines: Skip, Probe, or Entry

4. **Momentum Validation** (5-second window)
   - Watch for real trades
   - Confirm volume and price movement
   - Verify buy ratio >50%

5. **Entry Decision**
   - If momentum confirmed → Execute 0.02 SOL buy
   - Position tracked in memory

6. **Top Holder Monitoring**
   - Watch top 3 holders for sells
   - Early warning for rugs

7. **Auto Exit**
   - Take profit at +50%
   - Stop loss at -30%
   - Price polled every second

---

## Current Position Status

### No Active Positions
All positions have been closed by the auto-sell system.

### Recent Closed Positions
1. **HARRY** (65B3rzGoGZzEUnhqY5kMRpQQXTCUUxLx78qxvgFgpump)
   - Entry: 0.02 SOL
   - Exit: Stop-loss triggered
   - P&L: -0.02 SOL (position went -66% before exit)

2. Multiple other tokens (AFE4bnyehAYN, etc.)
   - Bot opened positions but stopped out

### Observations
- Tokens are rugging faster than the old 30% stop-loss could save us
- The -66% loss on HARRY showed stop-loss was not triggering fast enough

### Config Changes Applied (Latest Session)
1. **Stop-loss tightened**: 30% → 15%
2. **Price polling faster**: 1000ms → 500ms
3. **Holder watching enabled**: `exit_on_any_sell: true` - exits when ANY top holder sells
4. **Emergency exit logic**: Already implemented in code - triggers immediately when top holder dumps

---

## Key Challenges & Lessons

### What Works
1. WebSocket detection catches tokens at creation
2. Momentum validation filters out dead tokens
3. Small position sizes (0.02 SOL) limit slippage damage
4. Auto-sell prevents catastrophic losses

### What Doesn't Work
1. **Chasing pumped tokens** - if price already moved 50%+, too late
2. **Large positions** - slippage eats profits
3. **Trusting B/S ratios** - often manipulated
4. **Manual trading** - too slow

### Remaining Challenges
1. **Slippage still significant** on exits
2. **Most tokens rug** within minutes
3. **Win rate is low** - need occasional big wins to cover losses
4. **Wash trading detection** needs improvement

---

## Strategy Plan (In Progress)

A comprehensive strategy plan exists at:
`C:\Users\DavidPorter\.claude\plans\twinkling-purring-sonnet.md`

Key modules planned:
- Fatal Risk Engine (kill switch)
- Liquidity Analyzer (exit feasibility)
- Regime Classifier (organic vs wash vs sniper flip)
- Portfolio Risk Governor (capital limits)
- Decision Arbitrator (conflict resolution)
- Randomization (adversarial resistance)

---

## Files to Know

| File | Purpose |
|------|---------|
| `config.toml` | All bot settings |
| `credentials/hot-trading.json` | Wallet keypair |
| `src/cli/commands.rs` | Main trading loop |
| `src/stream/pumpportal.rs` | WebSocket client |
| `src/filter/adaptive.rs` | Token scoring |
| `src/filter/momentum.rs` | Pre-entry validation |
| `src/trading/pumpportal_api.rs` | Trade execution |
| `scripts/*.js` | Manual trading tools |

---

## Next Steps for Continuation

1. **Monitor current HARRY position** - check if auto-sell triggers
2. **Track P&L** after several trades to assess strategy effectiveness
3. **Consider implementing** more of the strategy plan modules
4. **Adjust thresholds** if win rate is too low or slippage too high
5. **Consider whale following** - track profitable wallets and copy trades

---

## Commands Reference

```bash
# Check balance
node -e "const {Connection,PublicKey,LAMPORTS_PER_SOL}=require('@solana/web3.js');new Connection('https://mainnet.helius-rpc.com/?api-key=066a76e6-916f-4ef2-9194-c86676072933').getBalance(new PublicKey('C9ibhqLMz68HewsMXiZyXVAiJ68uLg53vSsSuyLQWYA6')).then(b=>console.log(b/LAMPORTS_PER_SOL,'SOL'))"

# Run bot
cd D:\pumpfun && .\target\release\pumpfun-sniper.exe snipe start

# Manual buy
node scripts/quick_buy.js <MINT> 0.02

# Manual sell
node scripts/quick_sell.js <MINT> 100
```

---

## Important Notes

- **RPC API Key** is in config.toml - Helius mainnet
- **Priority fee** is set to 100,000 lamports (0.0001 SOL)
- **Slippage tolerance** is 25% (2500 bps)
- **Auto-sell** is enabled with 50% TP / 30% SL
- **Position tracking** is in-memory only (lost on restart)

---

*Documentation created: 2026-01-11*
*For session continuation purposes*

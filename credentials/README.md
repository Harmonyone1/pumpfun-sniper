# Credentials Directory

This directory contains authentication credentials for external services. **These files should NEVER be committed to version control.**

## Directory Structure

```
credentials/
├── README.md                    # This file
└── shredstream/
    ├── README.md                # ShredStream-specific docs
    ├── generate-keypair.mjs     # Keypair generation script
    └── shredstream-auth-keypair.json  # Auth keypair (generated)
```

## Keypairs Overview

### 1. ShredStream Authentication Keypair
- **Location**: `shredstream/shredstream-auth-keypair.json`
- **Purpose**: Challenge-response authentication with Jito ShredStream
- **Public Key**: `FSQaaVdBu1iYA1XzaQJXAty7Vy2giQfZiw1mL8e4SGJY`
- **Funds**: Should have **NO FUNDS** - authentication only

### 2. Trading Keypair (Your Wallet)
- **Location**: Specified in `.env` as `KEYPAIR_PATH`
- **Purpose**: Signing transactions, paying fees
- **Funds**: Keep minimal SOL for trading
- **Security**: Use an isolated wallet, not your main wallet

## Security Checklist

- [ ] All keypair files have restrictive permissions (`chmod 600` on Unix)
- [ ] Keypair files are in `.gitignore`
- [ ] Trading keypair is an isolated wallet with minimal funds
- [ ] ShredStream keypair has zero balance
- [ ] Backup keypairs are stored securely offline

## Generating New Keypairs

### ShredStream Keypair
```bash
cd credentials/shredstream
node generate-keypair.mjs
```

### Trading Keypair (requires Solana CLI)
```bash
solana-keygen new --outfile /path/to/trading-keypair.json
```

## Environment Configuration

After setting up keypairs, update your `.env` file:

```env
# Trading keypair (with funds)
KEYPAIR_PATH=D:\path\to\your\trading-keypair.json

# ShredStream auth keypair (no funds)
SHREDSTREAM_AUTH_KEYPAIR=D:\pumpfun\credentials\shredstream\shredstream-auth-keypair.json
```

## Approval Status

| Service | Status | Public Key |
|---------|--------|------------|
| ShredStream | Pending | `FSQaaVdBu1iYA1XzaQJXAty7Vy2giQfZiw1mL8e4SGJY` |

Update this table when you receive approval confirmations.

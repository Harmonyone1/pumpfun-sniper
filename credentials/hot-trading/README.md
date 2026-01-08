# Hot Trading Wallet

This folder contains the keypair for the active trading wallet.

## Files

- `keypair.json` - Solana keypair (64-byte secret key array)

## Security

- **NEVER** commit this keypair to version control
- Set restrictive permissions: `chmod 600 keypair.json` (Unix)
- Use an isolated wallet with minimal SOL
- This wallet is used for active trading operations

## Generating a New Keypair

Using Solana CLI:
```bash
solana-keygen new --outfile keypair.json --no-bip39-passphrase
```

Or using the bot CLI:
```bash
snipe wallet add hot-trading --alias "Trading Wallet" --wallet_type hot --generate
```

## Balance Recommendations

- Keep only what you need for trading operations
- Extract profits to vault regularly
- The bot will warn if balance exceeds safety thresholds

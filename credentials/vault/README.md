# Vault Configuration

This folder documents the vault destination for profit extraction.

## Current Vault

**Name:** vault-robinhood
**Type:** External
**Address:** `6jMnuDRmRdANA2NmuKeW9YpYkR8WCov6JFjtxdz6kTWB`

## About External Vaults

External vaults are addresses the bot can send funds TO but cannot withdraw FROM.
This provides security - even if the bot is compromised, funds in the vault are safe.

Supported external vault types:
- Hardware wallets (Ledger, Trezor)
- Exchange deposit addresses
- Custodial wallets (Robinhood, etc.)
- Multi-sig wallets

## Changing Vault Address

The vault address is locked by default (`vault_address_locked = true` in config).
To change it:

1. Edit `credentials/wallets.json`
2. Update the `address` field for the vault entry
3. Restart the bot

**WARNING:** Verify the new address is correct before transferring funds.
SOL sent to wrong addresses cannot be recovered.

## Transfer Limits

Safety limits prevent accidental large transfers:
- `max_single_transfer_sol`: Maximum per transfer
- `max_daily_extraction_sol`: Maximum per day
- `confirm_above_sol`: Requires manual confirmation

See `config.toml` [wallet.safety] section for current values.

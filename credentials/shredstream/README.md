# ShredStream Authentication Credentials

## Overview

This folder contains the ed25519 keypair used for authenticating with Jito ShredStream. This keypair is **ONLY** for ShredStream challenge-response authentication - it should never hold funds or be used for signing transactions.

## Files

| File | Description |
|------|-------------|
| `shredstream-auth-keypair.json` | The 64-byte keypair in Solana CLI format |
| `generate-keypair.mjs` | Script to generate a new keypair |
| `README.md` | This documentation |

## Your Public Key

```
FSQaaVdBu1iYA1XzaQJXAty7Vy2giQfZiw1mL8e4SGJY
```

**Submit this public key to the ShredStream approval form:**
https://web.miniextensions.com/WV3gZjFwqNqITsMufIEp

## Security Notes

1. **No Funds**: This keypair should NEVER have any SOL or tokens
2. **Separate Identity**: This is separate from your trading keypair
3. **Do Not Share**: Never share the private key (the JSON file)
4. **Git Ignored**: The keypair file is excluded from version control

## How ShredStream Authentication Works

1. Your bot connects to ShredStream gRPC endpoint
2. Server sends a challenge (random bytes)
3. Your bot signs the challenge with this private key
4. Server verifies signature matches your registered public key
5. Connection is established

## Configuration

After ShredStream approves your public key, update your `.env` file:

```env
# Path to ShredStream authentication keypair
SHREDSTREAM_AUTH_KEYPAIR=D:\pumpfun\credentials\shredstream\shredstream-auth-keypair.json
```

## Regenerating the Keypair

If you need to generate a new keypair:

1. Delete `shredstream-auth-keypair.json`
2. Run: `node generate-keypair.mjs`
3. Submit the new public key to ShredStream
4. Wait for approval

**Note**: You'll need to wait for re-approval if you change your keypair.

## Approval Timeline

- Typical approval: 24-48 hours
- Check your email for confirmation
- If not approved after 72 hours, reach out to Jito support

## Troubleshooting

### "Authentication failed" errors
- Verify you're using the correct keypair file
- Confirm your public key was approved
- Check that the keypair file hasn't been corrupted

### "Invalid signature" errors
- The keypair file may be corrupted - regenerate it
- Ensure no modifications were made to the JSON file

## Related Documentation

- [Jito ShredStream Docs](https://docs.jito.wtf/lowlatencytxnfeed/)
- [ShredStream Proxy Setup](https://github.com/jito-labs/shredstream-proxy)

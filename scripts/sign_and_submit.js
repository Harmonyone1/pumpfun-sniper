const { Connection, Keypair, VersionedTransaction } = require('@solana/web3.js');
const fs = require('fs');

async function main() {
    // Load keypair
    const keypairPath = process.env.KEYPAIR_PATH || 'D:/pumpfun/credentials/hot-trading/keypair.json';
    const keypairData = JSON.parse(fs.readFileSync(keypairPath, 'utf8'));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(keypairData));
    console.log('Wallet:', keypair.publicKey.toBase58());

    // Load unsigned transaction
    const txPath = process.argv[2] || 'D:/pumpfun/analysis/unsigned_tx.bin';
    const txBytes = fs.readFileSync(txPath);
    console.log('Transaction size:', txBytes.length, 'bytes');

    // Deserialize transaction
    const tx = VersionedTransaction.deserialize(txBytes);
    console.log('Signatures required:', tx.message.header.numRequiredSignatures);

    // Sign transaction
    tx.sign([keypair]);
    console.log('Transaction signed');

    // Connect to RPC
    const rpcUrl = 'https://mainnet.helius-rpc.com/?api-key=066a76e6-916f-4ef2-9194-c86676072933';
    const connection = new Connection(rpcUrl, 'confirmed');

    // Check balance first
    const balance = await connection.getBalance(keypair.publicKey);
    console.log('Wallet balance:', balance / 1e9, 'SOL');

    if (balance < 0.04 * 1e9) {
        console.error('Insufficient balance for trade + fees');
        process.exit(1);
    }

    // Submit transaction
    console.log('Submitting transaction...');
    try {
        const signature = await connection.sendTransaction(tx, {
            skipPreflight: true,
            maxRetries: 3
        });
        console.log('SUCCESS! Signature:', signature);
        console.log('Solscan:', `https://solscan.io/tx/${signature}`);

        // Wait for confirmation
        console.log('Waiting for confirmation...');
        const confirmation = await connection.confirmTransaction(signature, 'confirmed');
        if (confirmation.value.err) {
            console.error('Transaction failed:', confirmation.value.err);
        } else {
            console.log('Transaction confirmed!');
        }
    } catch (err) {
        console.error('Submit error:', err.message);
        process.exit(1);
    }
}

main().catch(console.error);

const { Connection, Keypair, VersionedTransaction } = require('@solana/web3.js');
const fs = require('fs');

async function main() {
    // Config
    const mint = process.argv[2] || 'AZDrdnX8an4WoYHNgKswrfSSo622aTTEGja76sGqpump';
    const amount = process.argv[3] || '0.03';

    // Load keypair
    const keypairPath = 'D:/pumpfun/credentials/hot-trading/keypair.json';
    const keypairData = JSON.parse(fs.readFileSync(keypairPath, 'utf8'));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(keypairData));
    const publicKey = keypair.publicKey.toBase58();
    console.log('Wallet:', publicKey);

    // Connect to RPC and check balance
    const rpcUrl = 'https://mainnet.helius-rpc.com/?api-key=066a76e6-916f-4ef2-9194-c86676072933';
    const connection = new Connection(rpcUrl, 'confirmed');
    const balance = await connection.getBalance(keypair.publicKey);
    console.log('Balance:', (balance / 1e9).toFixed(4), 'SOL');

    if (balance < parseFloat(amount) * 1.1 * 1e9) {
        console.error('Insufficient balance');
        process.exit(1);
    }

    // Request unsigned transaction from PumpPortal
    console.log(`Buying ${amount} SOL of ${mint}...`);
    const response = await fetch('https://pumpportal.fun/api/trade-local', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            publicKey: publicKey,
            action: 'buy',
            mint: mint,
            amount: amount,
            denominatedInSol: 'true',
            slippage: 25,
            priorityFee: 0.0001,
            pool: 'auto'
        })
    });

    if (!response.ok) {
        const text = await response.text();
        console.error('API error:', response.status, text);
        process.exit(1);
    }

    // Get transaction bytes
    const txBuffer = await response.arrayBuffer();
    const txBytes = Buffer.from(txBuffer);
    console.log('Transaction size:', txBytes.length, 'bytes');

    // Deserialize and sign
    const tx = VersionedTransaction.deserialize(txBytes);
    tx.sign([keypair]);
    console.log('Transaction signed');

    // Submit
    console.log('Submitting transaction...');
    const signature = await connection.sendTransaction(tx, {
        skipPreflight: true,
        maxRetries: 3
    });
    console.log('SUCCESS! Signature:', signature);
    console.log('Solscan: https://solscan.io/tx/' + signature);

    // Wait for confirmation
    console.log('Waiting for confirmation...');
    const result = await connection.confirmTransaction(signature, 'confirmed');
    if (result.value.err) {
        console.error('Transaction failed:', JSON.stringify(result.value.err));
    } else {
        console.log('Transaction CONFIRMED!');
    }
}

main().catch(err => {
    console.error('Error:', err.message);
    process.exit(1);
});

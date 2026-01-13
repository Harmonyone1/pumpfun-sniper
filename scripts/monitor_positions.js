const { Connection, Keypair, VersionedTransaction, PublicKey } = require('@solana/web3.js');
const fs = require('fs');

const BOB_MINT = '6YEy3Em82fmm7QYiGH93pkQvtUCFbasrd8yuVpefpump';
const PENYS_MINT = '7epV32cdqGxa2rR6zMVYJpFpLVXh5SywuuhWTgw4pump';
const DEXBAN_MINT = '3ke3jNGfqKXCqdPtNsdYnDve91WHuHgD9Y2Ccvv3pump';

async function checkToken(mint, name) {
    const response = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${mint}`);
    const data = await response.json();
    const pair = data.pairs?.find(p => p.dexId === 'pumpswap') || data.pairs?.[0];

    if (!pair) return null;

    const m5 = pair.priceChange?.m5 || 0;
    const buys = pair.txns?.m5?.buys || 0;
    const sells = pair.txns?.m5?.sells || 0;
    const ratio = sells > 0 ? buys / sells : buys;

    return {
        name,
        price: parseFloat(pair.priceNative),
        m5,
        buys,
        sells,
        ratio,
        isDump: m5 < -15 && sells > buys,
        isHeavySell: sells > buys * 1.5
    };
}

async function sellToken(mint, keypair, connection) {
    console.log(`\n🔴 SELLING ${mint}...`);

    const response = await fetch('https://pumpportal.fun/api/trade-local', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            publicKey: keypair.publicKey.toBase58(),
            action: 'sell',
            mint: mint,
            amount: '100%',
            denominatedInSol: 'false',
            slippage: 25,
            priorityFee: 0.0001,
            pool: 'auto'
        })
    });

    if (!response.ok) {
        console.log('Sell API error:', await response.text());
        return false;
    }

    const txBytes = Buffer.from(await response.arrayBuffer());
    const tx = VersionedTransaction.deserialize(txBytes);
    tx.sign([keypair]);

    const sig = await connection.sendTransaction(tx, { skipPreflight: true });
    console.log('Sell TX:', sig);
    return true;
}

async function main() {
    const keypairData = JSON.parse(fs.readFileSync('D:/pumpfun/credentials/hot-trading/keypair.json', 'utf8'));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(keypairData));
    const connection = new Connection('https://mainnet.helius-rpc.com/?api-key=066a76e6-916f-4ef2-9194-c86676072933', 'confirmed');

    console.log('Monitoring positions...');
    console.log('Wallet:', keypair.publicKey.toBase58());

    while (true) {
        const timestamp = new Date().toLocaleTimeString();
        console.log(`\n[${timestamp}] Checking positions...`);

        try {
            const bob = await checkToken(BOB_MINT, 'BOB');
            const penys = await checkToken(PENYS_MINT, 'PENYS');

            if (bob) {
                console.log(`BOB: ${bob.price.toFixed(10)} SOL | 5m: ${bob.m5}% | B/S: ${bob.buys}/${bob.sells} (${bob.ratio.toFixed(2)})`);
                if (bob.isDump) {
                    console.log('🚨 BOB DUMPING - SELLING!');
                    await sellToken(BOB_MINT, keypair, connection);
                } else if (bob.isHeavySell) {
                    console.log('⚠️ BOB heavy sell pressure');
                }
            }

            if (penys) {
                console.log(`PENYS: ${penys.price.toFixed(10)} SOL | 5m: ${penys.m5}% | B/S: ${penys.buys}/${penys.sells} (${penys.ratio.toFixed(2)})`);
                if (penys.isDump) {
                    console.log('🚨 PENYS DUMPING - SELLING!');
                    await sellToken(PENYS_MINT, keypair, connection);
                }
            }

            const dexban = await checkToken(DEXBAN_MINT, 'DEXBAN');
            if (dexban) {
                console.log(`DEXBAN: ${dexban.price.toFixed(10)} SOL | 5m: ${dexban.m5}% | B/S: ${dexban.buys}/${dexban.sells} (${dexban.ratio.toFixed(2)})`);
                if (dexban.isDump) {
                    console.log('🚨 DEXBAN DUMPING - SELLING!');
                    await sellToken(DEXBAN_MINT, keypair, connection);
                }
            }

            // Check SOL balance
            const balance = await connection.getBalance(keypair.publicKey);
            console.log(`SOL Balance: ${(balance / 1e9).toFixed(4)}`);

        } catch (err) {
            console.log('Error:', err.message);
        }

        // Wait 10 seconds
        await new Promise(r => setTimeout(r, 10000));
    }
}

main().catch(console.error);

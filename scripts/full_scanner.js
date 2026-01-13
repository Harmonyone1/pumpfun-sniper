const { Connection, Keypair, VersionedTransaction, PublicKey } = require('@solana/web3.js');
const fs = require('fs');

// Tracked positions
const POSITIONS = {
    'B7ToiJNkQoFaPgRLZmEgvT9xZb9HLdVdKuNSBqospump': 'chicken',
    'Gp8kce3ZC7mhKDYy2RpDiDxLF7fYAPd8Vi1n2tqRpump': 'Charity',
    '5Nvwaf2r4rMJjTe53L4qMDNkN94keGPMGN56qfqapump': 'pube'
};

// Seen tokens to avoid duplicates
const seenTokens = new Set();

async function checkToken(mint) {
    const response = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${mint}`);
    const data = await response.json();
    const pair = data.pairs?.find(p => p.dexId === 'pumpswap') || data.pairs?.[0];
    if (!pair) return null;

    const m5 = pair.priceChange?.m5 || 0;
    const h1 = pair.priceChange?.h1 || 0;
    const buys = pair.txns?.m5?.buys || 0;
    const sells = pair.txns?.m5?.sells || 0;
    const ratio = sells > 0 ? buys / sells : buys;

    return {
        symbol: pair.baseToken?.symbol,
        name: pair.baseToken?.name,
        price: parseFloat(pair.priceNative),
        m5, h1,
        buys, sells, ratio,
        volume: pair.volume?.h1 || 0,
        mcap: pair.marketCap || 0,
        liquidity: pair.liquidity?.usd || 0,
        isDump: m5 < -15 && sells > buys,
        isHot: m5 > 10 && ratio > 1.3 && buys > 10
    };
}

async function sellToken(mint, keypair, connection) {
    console.log(`\n SELLING ${mint}...`);
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

async function scanNewTokens() {
    const opportunities = [];

    // Check latest token profiles
    try {
        const resp = await fetch('https://api.dexscreener.com/token-profiles/latest/v1');
        const profiles = await resp.json();
        const solanaTokens = profiles.filter(t => t.chainId === 'solana').slice(0, 20);

        for (const token of solanaTokens) {
            if (seenTokens.has(token.tokenAddress)) continue;
            seenTokens.add(token.tokenAddress);

            const data = await checkToken(token.tokenAddress);
            if (data && data.isHot) {
                opportunities.push({
                    mint: token.tokenAddress,
                    ...data
                });
            }
            await new Promise(r => setTimeout(r, 100));
        }
    } catch (e) {
        console.log('Scan error:', e.message);
    }

    // Check top boosted
    try {
        const resp = await fetch('https://api.dexscreener.com/token-boosts/top/v1');
        const boosts = await resp.json();
        const solanaBoosts = boosts.filter(t => t.chainId === 'solana').slice(0, 10);

        for (const token of solanaBoosts) {
            if (seenTokens.has(token.tokenAddress)) continue;
            seenTokens.add(token.tokenAddress);

            const data = await checkToken(token.tokenAddress);
            if (data && data.isHot) {
                opportunities.push({
                    mint: token.tokenAddress,
                    boosts: token.totalAmount,
                    ...data
                });
            }
            await new Promise(r => setTimeout(r, 100));
        }
    } catch (e) {}

    return opportunities;
}

async function main() {
    const keypairData = JSON.parse(fs.readFileSync('D:/pumpfun/credentials/hot-trading/keypair.json', 'utf8'));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(keypairData));
    const connection = new Connection('https://mainnet.helius-rpc.com/?api-key=066a76e6-916f-4ef2-9194-c86676072933', 'confirmed');

    console.log('=== FULL SCANNER STARTED ===');
    console.log('Wallet:', keypair.publicKey.toBase58());
    console.log('Monitoring positions:', Object.values(POSITIONS).join(', '));
    console.log('');

    while (true) {
        const timestamp = new Date().toLocaleTimeString();
        console.log(`\n[${timestamp}] === SCAN CYCLE ===`);

        try {
            // Check existing positions
            console.log('\n--- POSITIONS ---');
            for (const [mint, name] of Object.entries(POSITIONS)) {
                const data = await checkToken(mint);
                if (data) {
                    const status = data.isDump ? 'DUMP!' : (data.ratio > 1.3 ? 'OK' : 'watch');
                    console.log(`${name}: ${data.m5}% 5m | B/S: ${data.buys}/${data.sells} | ${status}`);

                    if (data.isDump) {
                        console.log(`>>> SELLING ${name}!`);
                        await sellToken(mint, keypair, connection);
                        delete POSITIONS[mint];
                    }
                }
            }

            // Check SOL balance
            const balance = await connection.getBalance(keypair.publicKey);
            console.log(`\nSOL: ${(balance / 1e9).toFixed(4)}`);

            // Scan for new opportunities
            console.log('\n--- SCANNING NEW TOKENS ---');
            const opportunities = await scanNewTokens();

            if (opportunities.length > 0) {
                console.log(`Found ${opportunities.length} hot opportunities:`);
                for (const opp of opportunities) {
                    console.log(`  ${opp.symbol}: ${opp.m5}% 5m, ${opp.h1}% 1h | B/S: ${opp.buys}/${opp.sells} | MCap: $${Math.round(opp.mcap).toLocaleString()}`);
                    console.log(`    Mint: ${opp.mint}`);
                }
            } else {
                console.log('No hot opportunities found');
            }

        } catch (err) {
            console.log('Error:', err.message);
        }

        // Wait 15 seconds
        await new Promise(r => setTimeout(r, 15000));
    }
}

main().catch(console.error);

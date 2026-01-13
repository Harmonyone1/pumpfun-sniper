// Whale/Smart Money Tracker
// Tracks known profitable wallets and their recent trades

const KNOWN_PROFITABLE_WALLETS = [
    // These would ideally be populated from on-chain analysis
    // For now, can add wallets that are known to be profitable
];

// Get recent trades for a wallet using Helius Enhanced API
async function getWalletTrades(walletAddress, heliusApiKey) {
    const url = `https://api.helius.xyz/v0/addresses/${walletAddress}/transactions?api-key=${heliusApiKey}&type=SWAP`;
    try {
        const resp = await fetch(url);
        const data = await resp.json();
        return data.slice(0, 20); // Last 20 swaps
    } catch (e) {
        console.log('Error fetching wallet trades:', e.message);
        return [];
    }
}

// Find top token holders for a pump.fun token
async function getTopHolders(mint) {
    try {
        // Using DexScreener to get holder info
        const resp = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${mint}`);
        const data = await resp.json();
        const pair = data.pairs?.find(p => p.dexId === 'pumpswap') || data.pairs?.[0];

        if (pair) {
            console.log('Token:', pair.baseToken?.symbol);
            console.log('MCap:', '$' + Math.round(pair.marketCap));
            console.log('Liquidity:', '$' + Math.round(pair.liquidity?.usd || 0));
            console.log('1h change:', pair.priceChange?.h1 + '%');
        }
    } catch (e) {
        console.log('Error:', e.message);
    }
}

// Search for recent pump.fun trades on Solscan (whale activity)
async function findWhaleActivity() {
    console.log('Looking for whale activity on pump.fun tokens...\n');

    // Get trending tokens
    const resp = await fetch('https://api.dexscreener.com/token-boosts/top/v1');
    const boosts = await resp.json();
    const solTokens = boosts.filter(t => t.chainId === 'solana').slice(0, 10);

    console.log('Top boosted Solana tokens:\n');

    for (const token of solTokens) {
        const r = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
        const d = await r.json();
        const p = d.pairs?.find(x => x.dexId === 'pumpswap') || d.pairs?.[0];

        if (p) {
            const m5 = p.priceChange?.m5 || 0;
            const h1 = p.priceChange?.h1 || 0;
            const volume = p.volume?.h1 || 0;

            console.log(`${p.baseToken?.symbol} | ${m5}% 5m | ${h1}% 1h | Vol: $${Math.round(volume)} | Boosts: ${token.totalAmount}`);
            console.log(`  ${token.tokenAddress}`);

            // High volume = whale activity
            if (volume > 50000) {
                console.log('  ^^^ HIGH VOLUME - WHALE ACTIVITY ^^^');
            }
            console.log('');
        }

        await new Promise(r => setTimeout(r, 100));
    }
}

// Main
findWhaleActivity().catch(console.error);

const fetch = require('node-fetch');

async function scan() {
    console.log('=== SCANNING FOR HOT TOKENS ===');

    // Check latest boosted tokens
    try {
        const boostsResp = await fetch('https://api.dexscreener.com/token-boosts/top/v1');
        const boosts = await boostsResp.json();
        const solanaBoosts = boosts.filter(t => t.chainId === 'solana').slice(0, 15);

        console.log('\nTop Boosted Solana Tokens:');
        for (const token of solanaBoosts) {
            try {
                const resp = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
                const data = await resp.json();
                const pair = data.pairs?.find(p => p.dexId === 'pumpswap' || p.dexId === 'pumpfun') || data.pairs?.[0];
                if (!pair) continue;

                const m5 = pair.priceChange?.m5 || 0;
                const h1 = pair.priceChange?.h1 || 0;
                const buys = pair.txns?.m5?.buys || 0;
                const sells = pair.txns?.m5?.sells || 0;
                const mcap = pair.marketCap || 0;
                const ratio = sells > 0 ? (buys/sells).toFixed(2) : buys;

                if (m5 > 5 && buys > sells) {
                    console.log(`${pair.baseToken?.symbol}: ${m5}% 5m, ${h1}% 1h | B/S: ${buys}/${sells} (${ratio}) | MCap: $${Math.round(mcap).toLocaleString()}`);
                    console.log(`  Mint: ${token.tokenAddress}`);
                }
                await new Promise(r => setTimeout(r, 100));
            } catch (e) {}
        }
    } catch (e) {
        console.log('Error fetching boosts:', e.message);
    }

    // Check latest profiles
    try {
        const profilesResp = await fetch('https://api.dexscreener.com/token-profiles/latest/v1');
        const profiles = await profilesResp.json();
        const solanaProfiles = profiles.filter(t => t.chainId === 'solana').slice(0, 15);

        console.log('\nLatest Token Profiles (hot):');
        for (const token of solanaProfiles) {
            try {
                const resp = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
                const data = await resp.json();
                const pair = data.pairs?.find(p => p.dexId === 'pumpswap' || p.dexId === 'pumpfun') || data.pairs?.[0];
                if (!pair) continue;

                const m5 = pair.priceChange?.m5 || 0;
                const h1 = pair.priceChange?.h1 || 0;
                const buys = pair.txns?.m5?.buys || 0;
                const sells = pair.txns?.m5?.sells || 0;
                const mcap = pair.marketCap || 0;
                const ratio = sells > 0 ? (buys/sells).toFixed(2) : buys;

                if (m5 > 10 && buys > sells * 1.3) {
                    console.log(`${pair.baseToken?.symbol}: ${m5}% 5m, ${h1}% 1h | B/S: ${buys}/${sells} (${ratio}) | MCap: $${Math.round(mcap).toLocaleString()}`);
                    console.log(`  Mint: ${token.tokenAddress}`);
                }
                await new Promise(r => setTimeout(r, 100));
            } catch (e) {}
        }
    } catch (e) {
        console.log('Error fetching profiles:', e.message);
    }
}

scan().catch(console.error);

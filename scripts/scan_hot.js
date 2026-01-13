// Scan for hot pump.fun tokens
async function scan() {
    console.log('Scanning for hot opportunities...');

    const resp = await fetch('https://api.dexscreener.com/token-profiles/latest/v1');
    const profiles = await resp.json();
    const solTokens = profiles.filter(t => t.chainId === 'solana').slice(0, 40);

    const hot = [];

    for (const token of solTokens) {
        try {
            const r = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
            const d = await r.json();
            const p = d.pairs?.find(x => x.dexId === 'pumpswap') || d.pairs?.[0];

            if (!p) continue;

            const m5 = p.priceChange?.m5 || 0;
            const h1 = p.priceChange?.h1 || 0;
            const buys = p.txns?.m5?.buys || 0;
            const sells = p.txns?.m5?.sells || 0;
            const ratio = sells > 0 ? buys / sells : buys;
            const liq = p.liquidity?.usd || 0;

            // Hot criteria: pumping, good ratio, HIGH LIQUIDITY
            if (m5 > 10 && ratio > 1.2 && buys > 10 && liq > 15000) {
                hot.push({
                    mint: token.tokenAddress,
                    symbol: p.baseToken?.symbol,
                    m5,
                    h1,
                    buys,
                    sells,
                    ratio: ratio.toFixed(2),
                    mcap: Math.round(p.marketCap),
                    liq: Math.round(liq)
                });
            }
        } catch (e) {}

        await new Promise(r => setTimeout(r, 50));
    }

    // Also check boosted tokens
    try {
        const bResp = await fetch('https://api.dexscreener.com/token-boosts/top/v1');
        const boosts = await bResp.json();
        const solBoosts = boosts.filter(t => t.chainId === 'solana').slice(0, 10);

        for (const token of solBoosts) {
            try {
                const r = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
                const d = await r.json();
                const p = d.pairs?.find(x => x.dexId === 'pumpswap') || d.pairs?.[0];

                if (!p) continue;

                const m5 = p.priceChange?.m5 || 0;
                const h1 = p.priceChange?.h1 || 0;
                const buys = p.txns?.m5?.buys || 0;
                const sells = p.txns?.m5?.sells || 0;
                const ratio = sells > 0 ? buys / sells : buys;
                const liq = p.liquidity?.usd || 0;

                if (m5 > 10 && ratio > 1.3 && buys > 10) {
                    // Check if already in list
                    if (!hot.find(h => h.mint === token.tokenAddress)) {
                        hot.push({
                            mint: token.tokenAddress,
                            symbol: p.baseToken?.symbol,
                            m5,
                            h1,
                            buys,
                            sells,
                            ratio: ratio.toFixed(2),
                            mcap: Math.round(p.marketCap),
                            liq: Math.round(liq),
                            boosted: true
                        });
                    }
                }
            } catch (e) {}

            await new Promise(r => setTimeout(r, 50));
        }
    } catch (e) {}

    // Sort by m5 momentum
    hot.sort((a, b) => b.m5 - a.m5);

    if (hot.length === 0) {
        console.log('No hot opportunities found');
    } else {
        console.log(`Found ${hot.length} hot opportunities:`);
        hot.forEach(h => {
            const boost = h.boosted ? ' [BOOSTED]' : '';
            console.log(`${h.symbol}: ${h.m5}% 5m, ${h.h1}% 1h | B/S: ${h.buys}/${h.sells} (${h.ratio}) | MCap: $${h.mcap} | Liq: $${h.liq}${boost}`);
            console.log(`  Mint: ${h.mint}`);
        });
    }
}

scan().catch(console.error);

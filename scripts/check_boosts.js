// Check boosted tokens for opportunities
async function checkBoosts() {
    const resp = await fetch('https://api.dexscreener.com/token-boosts/top/v1');
    const boosts = await resp.json();
    const sol = boosts.filter(t => t.chainId === 'solana').slice(0, 15);
    console.log('Checking', sol.length, 'boosted tokens...\n');

    const opportunities = [];

    for (const t of sol) {
        try {
            const r = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + t.tokenAddress);
            const d = await r.json();
            const p = d.pairs?.find(x => x.dexId === 'pumpswap') || d.pairs?.[0];
            if (!p) continue;

            const m5 = p.priceChange?.m5 || 0;
            const h1 = p.priceChange?.h1 || 0;
            const buys = p.txns?.m5?.buys || 0;
            const sells = p.txns?.m5?.sells || 0;
            const ratio = sells > 0 ? buys/sells : buys;
            const liq = p.liquidity?.usd || 0;

            if (m5 > 5 && ratio > 1.2 && buys > 5) {
                opportunities.push({
                    symbol: p.baseToken?.symbol,
                    mint: t.tokenAddress,
                    m5, h1, buys, sells,
                    ratio: ratio.toFixed(2),
                    mcap: Math.round(p.marketCap),
                    liq: Math.round(liq),
                    boosts: t.totalAmount
                });
            }
        } catch (e) {}
        await new Promise(r => setTimeout(r, 80));
    }

    opportunities.sort((a, b) => b.m5 - a.m5);

    if (opportunities.length === 0) {
        console.log('No good boosted opportunities found');
    } else {
        console.log('Found', opportunities.length, 'boosted opportunities:\n');
        for (const o of opportunities) {
            console.log(`${o.symbol}: ${o.m5}% 5m, ${o.h1}% 1h | B/S: ${o.buys}/${o.sells} (${o.ratio}) | MCap: $${o.mcap} | Liq: $${o.liq} | Boosts: ${o.boosts}`);
            console.log(`  ${o.mint}\n`);
        }
    }
}

checkBoosts().catch(console.error);

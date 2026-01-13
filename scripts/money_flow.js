// Money Flow Tracker - Track where wallets are actually moving money
const HELIUS_API_KEY = '066a76e6-916f-4ef2-9194-c86676072933';

// Get recent swaps for a token to see which wallets are buying/selling
async function getTokenSwaps(mint) {
    const url = `https://api.helius.xyz/v0/addresses/${mint}/transactions?api-key=${HELIUS_API_KEY}&type=SWAP`;
    try {
        const resp = await fetch(url);
        if (!resp.ok) return [];
        const txs = await resp.json();
        return txs.slice(0, 30);
    } catch (e) {
        return [];
    }
}

// Analyze recent transactions to find net buyers (wallets accumulating)
async function analyzeMoneyFlow(mint, symbol) {
    console.log(`\nAnalyzing ${symbol || mint.slice(0, 8)}...`);

    const txs = await getTokenSwaps(mint);
    if (txs.length === 0) {
        console.log('No swap data available');
        return null;
    }

    const walletActivity = {};

    for (const tx of txs) {
        if (!tx.tokenTransfers) continue;

        for (const transfer of tx.tokenTransfers) {
            if (transfer.mint === mint) {
                const amount = transfer.tokenAmount;
                const from = transfer.fromUserAccount;
                const to = transfer.toUserAccount;

                // Track sellers (from)
                if (from && from !== '' && !from.includes('pump')) {
                    if (!walletActivity[from]) walletActivity[from] = { bought: 0, sold: 0 };
                    walletActivity[from].sold += amount;
                }

                // Track buyers (to)
                if (to && to !== '' && !to.includes('pump')) {
                    if (!walletActivity[to]) walletActivity[to] = { bought: 0, sold: 0 };
                    walletActivity[to].bought += amount;
                }
            }
        }
    }

    // Find net buyers (wallets accumulating)
    const netBuyers = [];
    const netSellers = [];

    for (const [wallet, activity] of Object.entries(walletActivity)) {
        const net = activity.bought - activity.sold;
        if (net > 0) {
            netBuyers.push({ wallet, net, bought: activity.bought, sold: activity.sold });
        } else if (net < 0) {
            netSellers.push({ wallet, net: Math.abs(net), bought: activity.bought, sold: activity.sold });
        }
    }

    // Sort by net position
    netBuyers.sort((a, b) => b.net - a.net);
    netSellers.sort((a, b) => b.net - a.net);

    console.log(`Net Buyers: ${netBuyers.length} | Net Sellers: ${netSellers.length}`);

    if (netBuyers.length > 0) {
        console.log('Top Buyers:');
        netBuyers.slice(0, 3).forEach(b => {
            console.log(`  ${b.wallet.slice(0, 8)}... +${b.net.toFixed(0)} tokens`);
        });
    }

    if (netSellers.length > 0) {
        console.log('Top Sellers:');
        netSellers.slice(0, 3).forEach(s => {
            console.log(`  ${s.wallet.slice(0, 8)}... -${s.net.toFixed(0)} tokens`);
        });
    }

    return {
        buyerCount: netBuyers.length,
        sellerCount: netSellers.length,
        totalBuyers: netBuyers.reduce((sum, b) => sum + b.net, 0),
        totalSellers: netSellers.reduce((sum, s) => sum + s.net, 0),
        flow: netBuyers.length > netSellers.length ? 'ACCUMULATING' : 'DISTRIBUTING'
    };
}

// Find tokens with strong accumulation
async function findAccumulation() {
    console.log('=== MONEY FLOW ANALYSIS ===\n');

    // Get latest tokens
    const resp = await fetch('https://api.dexscreener.com/token-profiles/latest/v1');
    const profiles = await resp.json();
    const solTokens = profiles.filter(t => t.chainId === 'solana').slice(0, 15);

    const results = [];

    for (const token of solTokens) {
        // First get token info from DexScreener
        const r = await fetch('https://api.dexscreener.com/latest/dex/tokens/' + token.tokenAddress);
        const d = await r.json();
        const p = d.pairs?.find(x => x.dexId === 'pumpswap') || d.pairs?.[0];

        if (!p) continue;

        const m5 = p.priceChange?.m5 || 0;
        const h1 = p.priceChange?.h1 || 0;

        // Only analyze tokens with some positive momentum
        if (m5 > 0 && h1 > 0) {
            const flow = await analyzeMoneyFlow(token.tokenAddress, p.baseToken?.symbol);
            if (flow && flow.flow === 'ACCUMULATING' && flow.buyerCount >= 3) {
                results.push({
                    mint: token.tokenAddress,
                    symbol: p.baseToken?.symbol,
                    m5, h1,
                    ...flow
                });
            }
        }

        await new Promise(r => setTimeout(r, 200));
    }

    console.log('\n=== ACCUMULATING TOKENS ===');
    if (results.length === 0) {
        console.log('No clear accumulation patterns found');
    } else {
        results.sort((a, b) => b.buyerCount - a.buyerCount);
        for (const r of results) {
            console.log(`\n${r.symbol}: ${r.m5}% 5m, ${r.h1}% 1h`);
            console.log(`  Buyers: ${r.buyerCount} | Sellers: ${r.sellerCount} | Flow: ${r.flow}`);
            console.log(`  ${r.mint}`);
        }
    }
}

findAccumulation().catch(console.error);

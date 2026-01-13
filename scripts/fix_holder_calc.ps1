$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Fix holder calculation types
$content = $content.Replace(
    'let total_supply: f64 = holders.iter().map(|h| h.amount).sum();',
    'let total_supply: f64 = holders.iter().map(|h| h.amount as f64).sum();'
)

$content = $content.Replace(
    'holders[0].amount / total_supply',
    'holders[0].amount as f64 / total_supply'
)

# Also remove unused HotToken import
$content = $content.Replace(
    'use crate::dexscreener::{DexScreenerClient, HotScanConfig, HotToken};',
    'use crate::dexscreener::{DexScreenerClient, HotScanConfig};'
)

Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Fixed holder calculation types"

$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Fix 1: Remove borrow from helius_api_key
$content = $content.Replace('Some(HeliusClient::new(&helius_api_key))', 'Some(HeliusClient::new(helius_api_key))')

# Fix 2: Fix PumpPortalTrader::new signature
$content = $content.Replace(
    'Some((PumpPortalTrader::new(keypair.pubkey().to_string()), keypair, rpc_client))',
    'Some((PumpPortalTrader::new(None, true), keypair, rpc_client))'
)

# Fix 3: Fix PositionManager::new signature
$content = $content.Replace(
    'let position_manager = crate::position::manager::PositionManager::new("data/positions.json".to_string());',
    'let position_manager = crate::position::manager::PositionManager::new(config.safety.clone(), Some("data/positions.json".to_string()));'
)

# Fix 4: Replace has_position with get_position check
$content = $content.Replace(
    'if position_manager.has_position(&token.mint).await {',
    'if position_manager.get_position(&token.mint).await.is_some() {'
)

# Fix 5: Replace get_largest_holders with get_token_holders
$content = $content.Replace(
    'match helius.get_largest_holders(&token.mint, 10).await {',
    'match helius.get_token_holders(&token.mint, 10).await {'
)

# Fix 6: Add type annotation for holders
$content = $content.Replace(
    'Ok(holders) => {',
    'Ok(holders) => { let holders: Vec<_> = holders;'
)

# Fix 7: Fix slippage type
$content = $content.Replace(
    'let slippage = config.trading.slippage_bps as f64 / 100.0;',
    'let slippage = config.trading.slippage_bps / 100;'
)

# Fix 8: Replace EntryType::Momentum with EntryType::Opportunity
$content = $content.Replace(
    'entry_type: crate::position::manager::EntryType::Momentum, // Hot scan entries',
    'entry_type: crate::position::manager::EntryType::Opportunity, // Hot scan entries'
)

Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Fixed all hot_scan errors"

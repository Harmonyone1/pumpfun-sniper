$content = Get-Content 'D:\pumpfun\src\main.rs' -Raw

$scanHandler = @'
        ).await,
        Commands::Wallet { action } => match action {
'@

$hotScanHandler = @'
        ).await,
        Commands::HotScan {
            min_m5,
            min_ratio,
            min_liquidity,
            max_mcap,
            auto_buy,
            buy_amount,
            dry_run,
            watch,
            interval,
        } => commands::hot_scan(
            &config,
            min_m5,
            min_ratio,
            min_liquidity,
            max_mcap,
            auto_buy,
            buy_amount,
            dry_run,
            watch,
            interval,
        ).await,
        Commands::Wallet { action } => match action {
'@

$content = $content.Replace($scanHandler, $hotScanHandler)
Set-Content 'D:\pumpfun\src\main.rs' $content -NoNewline
Write-Output "Added HotScan handler"

$content = Get-Content 'D:\pumpfun\src\config.rs' -Raw

# Fix AutoSellConfig default impl - add trailing stop fields
$oldText = @'
            auto_sell: AutoSellConfig {
                enabled: true,
                take_profit_pct: default_take_profit_pct(),
                stop_loss_pct: default_stop_loss_pct(),
                partial_take_profit: false,
                price_poll_interval_ms: default_price_poll_interval_ms(),
            },
'@

$newText = @'
            auto_sell: AutoSellConfig {
                enabled: true,
                take_profit_pct: default_take_profit_pct(),
                stop_loss_pct: default_stop_loss_pct(),
                partial_take_profit: false,
                price_poll_interval_ms: default_price_poll_interval_ms(),
                trailing_stop_enabled: true,
                trailing_stop_activation_pct: default_trailing_activation(),
                trailing_stop_distance_pct: default_trailing_distance(),
            },
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\src\config.rs' $content -NoNewline
Write-Output "Fixed config.rs default impl"

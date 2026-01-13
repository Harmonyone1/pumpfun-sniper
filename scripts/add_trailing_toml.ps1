$content = Get-Content 'D:\pumpfun\config.toml' -Raw

$oldText = @'
[auto_sell]
# Enable automatic take-profit and stop-loss
enabled = true
# Sell when profit reaches this percentage
take_profit_pct = 50.0
# Sell when loss reaches this percentage (TIGHTENED from 30%)
stop_loss_pct = 15.0
# Enable partial take-profit (sell portion at TP, hold rest)
partial_take_profit = false
'@

$newText = @'
[auto_sell]
# Enable automatic take-profit and stop-loss
enabled = true
# Sell when profit reaches this percentage
take_profit_pct = 50.0
# Sell when loss reaches this percentage (TIGHTENED from 30%)
stop_loss_pct = 15.0
# Enable partial take-profit (sell portion at TP, hold rest)
partial_take_profit = false
# TRAILING STOP: Lock in profits by selling if price drops from peak
trailing_stop_enabled = true
# Profit % to activate trailing stop (only starts trailing after reaching this profit)
trailing_stop_activation_pct = 10.0
# Sell if price drops this % from the peak (e.g., 15 = sell if drops 15% from highest point)
trailing_stop_distance_pct = 15.0
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\config.toml' $content -NoNewline
Write-Output "Added trailing stop settings to config.toml"

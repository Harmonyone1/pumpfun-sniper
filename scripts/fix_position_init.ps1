$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Add peak_price: 0.0 after current_price: 0.0 in Position initializers
$content = $content -replace '(quick_profit_taken: false,\r?\n\s+current_price: 0\.0,\r?\n\s+)\}', @'
quick_profit_taken: false,
                                        current_price: 0.0,
                                        peak_price: 0.0,
                                    }
'@

Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Fixed Position initializers"

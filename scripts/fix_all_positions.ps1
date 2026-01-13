$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Pattern to match Position initializer and add peak_price
# Match: current_price: followed by any value and };
$content = $content -replace '(current_price: [^,]+,)(\r?\n\s*)\};(\r?\n\s*if let Err\(e\) = position_manager\.open_position)', '$1$2peak_price: 0.0,$2};$3'

# Also fix cases where current_price is a variable like estimated_price
$content = $content -replace 'current_price: estimated_price,\r?\n(\s+)\};\r?\n(\s+)if let Err', "current_price: estimated_price,`r`n`$1peak_price: estimated_price,`r`n`$1};`r`n`$2if let Err"

Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Fixed all Position initializers"

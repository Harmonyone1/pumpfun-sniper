$lines = Get-Content 'D:\pumpfun\src\cli\commands.rs'

# Fix at lines 922, 1066, 1417 (0-indexed: 921, 1065, 1416)
# Need to insert peak_price: estimated_price, after current_price line

$linesToFix = @(921, 1065, 1416)

# Process in reverse order so line numbers don't shift
$linesToFix = $linesToFix | Sort-Object -Descending

foreach ($lineIdx in $linesToFix) {
    if ($lineIdx -lt $lines.Count) {
        $currentLine = $lines[$lineIdx]
        # Get indentation from current line
        if ($currentLine -match '^(\s+)') {
            $indent = $Matches[1]
            # Insert peak_price line after current_price
            $newLine = "${indent}peak_price: estimated_price,"
            $newLines = @($lines[0..$lineIdx]) + $newLine + @($lines[($lineIdx+1)..($lines.Count-1)])
            $lines = $newLines
        }
    }
}

$lines -join "`r`n" | Set-Content 'D:\pumpfun\src\cli\commands.rs' -NoNewline
Write-Output "Fixed Position initializers at lines 922, 1066, 1417"

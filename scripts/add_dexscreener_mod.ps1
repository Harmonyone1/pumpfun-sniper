$content = Get-Content 'D:\pumpfun\src\lib.rs' -Raw
$content = $content -replace 'pub mod cli;', "pub mod cli;`r`npub mod dexscreener;"
Set-Content 'D:\pumpfun\src\lib.rs' $content -NoNewline
Write-Output "Added dexscreener module"

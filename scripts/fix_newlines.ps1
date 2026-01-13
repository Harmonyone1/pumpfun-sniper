$content = Get-Content 'D:\pumpfun\src\filter\momentum.rs' -Raw

# Fix the malformed line with 'n' instead of newlines
$content = $content -replace 'holder_concentration: f64,n    /// SURVIVOR: Whether holder data has been fetched \(must be true before entry\)n    holder_data_fetched: bool,n\}', "holder_concentration: f64,`r`n    /// SURVIVOR: Whether holder data has been fetched (must be true before entry)`r`n    holder_data_fetched: bool,`r`n}"

Set-Content 'D:\pumpfun\src\filter\momentum.rs' $content -NoNewline
Write-Output "Fixed newline issue!"

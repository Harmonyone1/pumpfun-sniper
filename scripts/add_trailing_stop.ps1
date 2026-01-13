$content = Get-Content 'D:\pumpfun\src\position\manager.rs' -Raw

# 1. Add peak_price field to Position struct
$oldText = @'
    /// Current price (updated by price feed)
    #[serde(skip)]
    pub current_price: f64,
}

impl Position {
'@

$newText = @'
    /// Current price (updated by price feed)
    #[serde(skip)]
    pub current_price: f64,
    /// Peak price seen since entry (for trailing stop)
    #[serde(default)]
    pub peak_price: f64,
}

impl Position {
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\src\position\manager.rs' $content -NoNewline
Write-Output "Added peak_price field to Position struct"

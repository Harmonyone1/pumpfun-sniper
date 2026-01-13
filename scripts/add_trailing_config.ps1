$content = Get-Content 'D:\pumpfun\src\config.rs' -Raw

# Add trailing stop fields to AutoSellConfig
$oldText = @'
#[derive(Debug, Clone, Deserialize)]
pub struct AutoSellConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,
    #[serde(default)]
    pub partial_take_profit: bool,
    #[serde(default = "default_price_poll_interval_ms")]
    pub price_poll_interval_ms: u64,
}
'@

$newText = @'
#[derive(Debug, Clone, Deserialize)]
pub struct AutoSellConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,
    #[serde(default)]
    pub partial_take_profit: bool,
    #[serde(default = "default_price_poll_interval_ms")]
    pub price_poll_interval_ms: u64,
    /// Enable trailing stop to lock in profits
    #[serde(default = "default_true")]
    pub trailing_stop_enabled: bool,
    /// Profit % to activate trailing stop (e.g., 10 = activate at 10% profit)
    #[serde(default = "default_trailing_activation")]
    pub trailing_stop_activation_pct: f64,
    /// Distance from peak to trigger sell (e.g., 15 = sell if drops 15% from peak)
    #[serde(default = "default_trailing_distance")]
    pub trailing_stop_distance_pct: f64,
}

fn default_trailing_activation() -> f64 { 10.0 }
fn default_trailing_distance() -> f64 { 15.0 }
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\src\config.rs' $content -NoNewline
Write-Output "Added trailing stop config to AutoSellConfig"

$content = Get-Content 'D:\pumpfun\src\position\manager.rs' -Raw

# Update update_price to also track peak_price
$oldText = @'
    /// Update current price for a position
    pub async fn update_price(&self, mint: &str, price: f64) {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.current_price = price;
        }
    }
'@

$newText = @'
    /// Update current price for a position (also tracks peak price for trailing stop)
    pub async fn update_price(&self, mint: &str, price: f64) {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.current_price = price;
            // Track peak price for trailing stop
            if price > position.peak_price {
                position.peak_price = price;
            }
            // Initialize peak_price if not set (first update or loaded from disk)
            if position.peak_price == 0.0 {
                position.peak_price = price;
            }
        }
    }

    /// Get peak price for a position
    pub async fn get_peak_price(&self, mint: &str) -> Option<f64> {
        let positions = self.positions.read().await;
        positions.get(mint).map(|p| p.peak_price)
    }
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\src\position\manager.rs' $content -NoNewline
Write-Output "Updated update_price to track peak price"

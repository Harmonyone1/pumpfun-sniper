$content = Get-Content 'D:\pumpfun\src\filter\momentum.rs' -Raw

# 1. Add holder_data_fetched field to WatchedToken struct
$content = $content -replace '(    /// SURVIVOR: Top holder concentration \(set via Helius API\)\r?\n    holder_concentration: f64,\r?\n\})', @'
    /// SURVIVOR: Top holder concentration (set via Helius API)
    holder_concentration: f64,
    /// SURVIVOR: Whether holder data has been fetched (must be true before entry)
    holder_data_fetched: bool,
}
'@

# 2. Update WatchedToken::new() to initialize holder_data_fetched
$content = $content -replace '(            holder_concentration: 0\.0, // Set via set_holder_concentration\(\)\r?\n        \})', @'
            holder_concentration: 0.0, // Set via set_holder_concentration()
            holder_data_fetched: false, // Will be set true when Helius data arrives
        }
'@

# 3. Update set_holder_concentration to set holder_data_fetched = true
$content = $content -replace '(pub async fn set_holder_concentration\(&self, mint: &str, concentration: f64\) \{\r?\n        let mut watchlist = self\.watchlist\.write\(\)\.await;\r?\n        if let Some\(token\) = watchlist\.get_mut\(mint\) \{\r?\n            token\.holder_concentration = concentration;)', @'
pub async fn set_holder_concentration(&self, mint: &str, concentration: f64) {
        let mut watchlist = self.watchlist.write().await;
        if let Some(token) = watchlist.get_mut(mint) {
            token.holder_concentration = concentration;
            token.holder_data_fetched = true;
'@

# 4. Add holder_data_fetched to MomentumMetrics struct (after holder_concentration)
$content = $content -replace '(    pub holder_concentration: f64, // top holder as % of supply \(set externally\)\r?\n    pub second_wave_buy_ratio: f64,)', @'
    pub holder_concentration: f64, // top holder as % of supply (set externally)
    pub holder_data_fetched: bool, // whether holder data has been fetched
    pub second_wave_buy_ratio: f64,
'@

# 5. Update calculate_metrics to include holder_data_fetched
$content = $content -replace '(            holder_concentration: self\.holder_concentration,\r?\n            second_wave_buy_ratio,)', @'
            holder_concentration: self.holder_concentration,
            holder_data_fetched: self.holder_data_fetched,
            second_wave_buy_ratio,
'@

# 6. Update meets_thresholds to require holder_data_fetched
$content = $content -replace '(        // SURVIVOR: No whale dominance \(holder concentration check\)\r?\n        // Note: holder_concentration is 0\.0 if not set, which passes\r?\n        if self\.holder_concentration > config\.max_holder_concentration && self\.holder_concentration > 0\.0 \{\r?\n            return false;\r?\n        \})', @'
        // SURVIVOR: Require holder data to be fetched before entry
        if !self.holder_data_fetched {
            return false;
        }

        // SURVIVOR: No whale dominance (holder concentration check)
        if self.holder_concentration > config.max_holder_concentration {
            return false;
        }
'@

# 7. Update status_string to show holder data status
$content = $content -replace '(        // SURVIVOR: Holder concentration check\r?\n        if self\.holder_concentration > config\.max_holder_concentration && self\.holder_concentration > 0\.0 \{\r?\n            missing\.push\(format!\("whale:\{:\.0\}%>\{:\.0\}%", self\.holder_concentration \* 100\.0, config\.max_holder_concentration \* 100\.0\)\);\r?\n        \})', @'
        // SURVIVOR: Holder data must be fetched
        if !self.holder_data_fetched {
            missing.push("holder_data:pending".to_string());
        } else if self.holder_concentration > config.max_holder_concentration {
            // SURVIVOR: Holder concentration check (only if data fetched)
            missing.push(format!("whale:{:.0}%>{:.0}%", self.holder_concentration * 100.0, config.max_holder_concentration * 100.0));
        }
'@

Set-Content 'D:\pumpfun\src\filter\momentum.rs' $content -NoNewline
Write-Output "Holder race condition fix applied!"

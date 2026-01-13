$content = Get-Content 'D:\pumpfun\src\main.rs' -Raw

# Add HotScan command after Scan command
$scanEnd = @'
        interval: u64,
    },
}
'@

$hotScanCmd = @'
        interval: u64,
    },

    /// Scan DexScreener for hot tokens with momentum (uses Survivor Mode validation)
    HotScan {
        /// Minimum 5-minute price change percentage
        #[arg(long, default_value = "10.0")]
        min_m5: f64,

        /// Minimum buy/sell ratio
        #[arg(long, default_value = "1.3")]
        min_ratio: f64,

        /// Minimum liquidity in USD
        #[arg(long, default_value = "10000")]
        min_liquidity: f64,

        /// Maximum market cap in USD (avoid late entries)
        #[arg(long, default_value = "500000")]
        max_mcap: f64,

        /// Auto-buy tokens that pass all filters
        #[arg(long)]
        auto_buy: bool,

        /// Buy amount in SOL for auto-buy mode
        #[arg(long, default_value = "0.05")]
        buy_amount: f64,

        /// Run in dry-run mode (no real trades)
        #[arg(long)]
        dry_run: bool,

        /// Watch mode - continuously scan
        #[arg(long)]
        watch: bool,

        /// Scan interval in seconds for watch mode
        #[arg(long, default_value = "30")]
        interval: u64,
    },
}
'@

$content = $content.Replace($scanEnd, $hotScanCmd)
Set-Content 'D:\pumpfun\src\main.rs' $content -NoNewline
Write-Output "Added HotScan command"

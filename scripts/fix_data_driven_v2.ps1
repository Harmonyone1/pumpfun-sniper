$lines = Get-Content 'D:\pumpfun\src\cli\commands.rs'
$output = @()
$inserted = $false

for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]
    $output += $line

    # Insert after the liquidity check continue statement, right before "Use configured buy amount"
    if (-not $inserted -and $line -match '^\s+// Use configured buy amount for trade-based entries') {
        # Check if line before is empty (after continue;)
        if ($lines[$i-1].Trim() -eq '') {
            # Insert momentum validation before this line
            $indent = "                            "
            $newLines = @(
                "$indent// SURVIVOR MODE: Only follow whale trades if token passed momentum validation",
                "$indent// This prevents following whales into unvalidated tokens (no holder data, no observation window)",
                "${indent}let momentum_status = momentum_validator.check_momentum(&trade.mint).await;",
                "${indent}match momentum_status {",
                "$indent    crate::filter::momentum::MomentumStatus::Ready { metrics: _ } => {",
                "$indent        info!(",
                "$indent            `"DATA-DRIVEN ENTRY approved - {} passed momentum validation`",",
                "$indent            &trade.mint[..12]",
                "$indent        );",
                "$indent    }",
                "$indent    crate::filter::momentum::MomentumStatus::NotWatched => {",
                "$indent        debug!(",
                "$indent            `"DATA-DRIVEN ENTRY skipped - {} not in watchlist`",",
                "$indent            &trade.mint[..12]",
                "$indent        );",
                "$indent        continue;",
                "$indent    }",
                "$indent    crate::filter::momentum::MomentumStatus::Observing { metrics, reason } => {",
                "$indent        info!(",
                "$indent            `"DATA-DRIVEN ENTRY blocked - {} still observing: {} (survival: {:.0}%, holders: {})`",",
                "$indent            &trade.mint[..12], reason, metrics.survival_ratio * 100.0,",
                "$indent            if metrics.holder_data_fetched { `"ready`" } else { `"pending`" }",
                "$indent        );",
                "$indent        continue;",
                "$indent    }",
                "$indent    crate::filter::momentum::MomentumStatus::Expired { metrics: _ } => {",
                "$indent        debug!(",
                "$indent            `"DATA-DRIVEN ENTRY skipped - {} expired`",",
                "$indent            &trade.mint[..12]",
                "$indent        );",
                "$indent        continue;",
                "$indent    }",
                "${indent}}",
                ""
            )
            # Insert before current line (at index -1 from current position in output)
            $output = $output[0..($output.Count-2)] + $newLines + $output[-1]
            $inserted = $true
        }
    }
}

if ($inserted) {
    $output -join "`r`n" | Set-Content 'D:\pumpfun\src\cli\commands.rs' -NoNewline
    Write-Output "Inserted momentum validation for DATA-DRIVEN ENTRY"
} else {
    Write-Output "Could not find insertion point"
}

const fs = require('fs');
let content = fs.readFileSync('src/cli/commands.rs', 'utf8');

// Check if already applied
if (content.includes('SURVIVOR: Fetch holder concentration')) {
    console.log('Code already present');
    process.exit(0);
}

// Find the marker and insert after it
const marker = '// Don\'t buy immediately - wait for momentum validation';
const insertPoint = content.indexOf(marker);

if (insertPoint === -1) {
    console.log('ERROR: Could not find insert marker');
    process.exit(1);
}

// Find the info! line before the marker
const beforeMarker = content.substring(0, insertPoint);
const infoLine = beforeMarker.lastIndexOf('info!(');
const blockEnd = beforeMarker.lastIndexOf('}');

// We need to insert before info!(
const insertCode = `
                            // SURVIVOR: Fetch holder concentration from Helius
                            if let Some(ref helius) = helius_client {
                                let mint_for_holders = token.mint.clone();
                                let symbol_for_log = token.symbol.clone();
                                let helius_clone = helius.clone();
                                let validator_clone = momentum_validator.clone();
                                tokio::spawn(async move {
                                    match helius_clone.get_token_holders(&mint_for_holders, 10).await {
                                        Ok(holders) => {
                                            let top_holder_pct = holders.first()
                                                .map(|h| h.percentage / 100.0)
                                                .unwrap_or(0.0);
                                            validator_clone.set_holder_concentration(&mint_for_holders, top_holder_pct).await;
                                            debug!(
                                                "SURVIVOR: {} top holder owns {:.1}% of supply",
                                                symbol_for_log, top_holder_pct * 100.0
                                            );
                                        }
                                        Err(e) => {
                                            warn!("Could not fetch holder data for {}: {} - holder check will pass by default",
                                                &mint_for_holders[..12], e);
                                        }
                                    }
                                });
                            }
`;

// Find where to insert - just before "info!("Token {} added"
const pattern = /\n(\s+)info!\(\s*\n\s+"Token \{\} added to momentum watchlist/;
const match = content.match(pattern);

if (match) {
    const insertAt = content.indexOf(match[0]);
    content = content.substring(0, insertAt) + insertCode + content.substring(insertAt);
    fs.writeFileSync('src/cli/commands.rs', content);
    console.log('Successfully inserted holder fetch code');
} else {
    console.log('ERROR: Could not find insert location pattern');
    process.exit(1);
}

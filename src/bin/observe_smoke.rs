//! Mainnet observation-only smoke harness (P0-OBSERVATION-SMOKE-001).
//!
//! This binary answers a single question: can the merged system load its config,
//! acquire the runtime exclusion lease, reach the configured Solana RPC, prove it
//! is mainnet, connect to PumpPortal, receive/parse real new-token + migration
//! events, resolve an observed mint through the canonical market oracle, and take
//! a read-only exact-size hypothetical quote — all WITHOUT loading a private key
//! or exposing any transaction-submission call path.
//!
//! There is deliberately NO execution code here: no signer, no keypair, no
//! transaction builder/sender, no position/pending mutation, no trading API. The
//! static test suite at the bottom fails the binary if such references ever creep
//! in. See the packet for the full contract.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tokio::sync::mpsc;

use pumpfun_sniper::market::{PumpMarketOracle, QuoteAsset};
use pumpfun_sniper::runtime::RuntimeLease;
use pumpfun_sniper::stream::{
    PumpPortalClient, PumpPortalConfig, PumpPortalEvent, PumpPortalSubscriptionPlan,
};
use pumpfun_sniper::Config;

/// Hypothetical BUY size for quote computation ONLY (0.001 SOL). Never submitted.
const HYPOTHETICAL_BUY_LAMPORTS: u64 = 1_000_000;

/// Full Solana mainnet genesis hash. Any other value fails the smoke immediately,
/// making an accidental devnet/testnet run impossible.
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

/// Bounded event-channel capacity (within the packet's 32..=1024 window).
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Maximum time to wait for the first `Connected` before failing.
const CONNECT_DEADLINE_SECS: u64 = 10;

const SECONDS_MIN: u64 = 10;
const SECONDS_MAX: u64 = 120;
const TARGET_MIN: usize = 1;
const TARGET_MAX: usize = 10;

#[derive(clap::Parser)]
#[command(name = "observe-smoke")]
struct Args {
    /// Existing local ignored config.
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Real-data observation window (seconds).
    #[arg(long, default_value_t = 45)]
    seconds: u64,

    /// Stop early after this many new-token events IF at least one market
    /// snapshot + quote has succeeded.
    #[arg(long, default_value_t = 3)]
    target_new_tokens: usize,
}

/// Validate `--seconds` is within [SECONDS_MIN, SECONDS_MAX].
fn validate_seconds(seconds: u64) -> Result<u64> {
    if !(SECONDS_MIN..=SECONDS_MAX).contains(&seconds) {
        return Err(anyhow!(
            "--seconds must be between {SECONDS_MIN} and {SECONDS_MAX} (got {seconds})"
        ));
    }
    Ok(seconds)
}

/// Validate `--target-new-tokens` is within [TARGET_MIN, TARGET_MAX].
fn validate_target(target: usize) -> Result<usize> {
    if !(TARGET_MIN..=TARGET_MAX).contains(&target) {
        return Err(anyhow!(
            "--target-new-tokens must be between {TARGET_MIN} and {TARGET_MAX} (got {target})"
        ));
    }
    Ok(target)
}

/// Terminal-safe short text: strip ASCII control characters (including ESC / ANSI
/// introducers), keep printable text, and hard-cap the character count. Pure.
fn sanitize_short_text(input: &str, max_chars: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// Observation counters.
#[derive(Default)]
struct Counters {
    connected_events: u64,
    disconnect_events: u64,
    provider_errors: u64,
    new_token_events: u64,
    migration_events: u64,
    market_snapshot_successes: u64,
    hypothetical_quote_successes: u64,
    market_observation_failures: u64,
    /// Trade events observed despite requesting a free-only plan (zero token/
    /// account trade subscriptions). Any nonzero count fails the smoke.
    unexpected_trade_events: u64,
    /// P1-PROVIDER-DECODE-TRUTH-001 §8: local provider-message decode/schema-loss
    /// events. The smoke tests parser/connectivity correctness, so ANY decode loss
    /// (nonzero count) fails the smoke PASS.
    decode_errors: u64,
}

/// Pure final-PASS policy. A run passes ONLY if the RPC is mainnet, a slot was
/// read, the stream is currently connected (every observed disconnect was
/// followed by a later `Connected`), no provider error or unexpected trade event
/// occurred, and at least one new token / snapshot / hypothetical quote landed.
fn smoke_passes(
    rpc_mainnet: bool,
    current_slot: u64,
    stream_connected: bool,
    counters: &Counters,
) -> bool {
    rpc_mainnet
        && current_slot > 0
        && stream_connected
        && counters.connected_events >= 1
        && counters.provider_errors == 0
        && counters.unexpected_trade_events == 0
        && counters.decode_errors == 0
        && counters.new_token_events >= 1
        && counters.market_snapshot_successes >= 1
        && counters.hypothetical_quote_successes >= 1
}

/// Pure early-termination policy. Only evaluated at a receive quiet point (a recv
/// timeout with no already-queued event), so a queued fail-closed event is always
/// processed first. Requires the stream currently connected and zero fail-closed
/// counters, so we never early-PASS right after a known failure event.
fn early_success_candidate(
    target_new_tokens: usize,
    stream_connected: bool,
    counters: &Counters,
) -> bool {
    stream_connected
        && counters.provider_errors == 0
        && counters.unexpected_trade_events == 0
        && counters.decode_errors == 0
        && counters.new_token_events >= target_new_tokens as u64
        && counters.market_snapshot_successes >= 1
        && counters.hypothetical_quote_successes >= 1
}

/// Control-event classification for the nonblocking final drain. Separate from the
/// main loop's rich NewToken handling (which does async RPC/quote work); this only
/// mutates counters/connectivity and never performs I/O or prints provider fields.
enum TerminalKind {
    Connected,
    Disconnected,
    Error,
    Decode,
    Trade,
    Migration,
    NewToken,
}

/// Map an event to its terminal kind (no ownership of provider payloads needed).
fn terminal_kind_of(event: &PumpPortalEvent) -> TerminalKind {
    match event {
        PumpPortalEvent::Connected => TerminalKind::Connected,
        PumpPortalEvent::Disconnected => TerminalKind::Disconnected,
        PumpPortalEvent::Error(_) => TerminalKind::Error,
        PumpPortalEvent::DecodeError(_) => TerminalKind::Decode,
        PumpPortalEvent::Trade(_) => TerminalKind::Trade,
        PumpPortalEvent::Migration(_) => TerminalKind::Migration,
        PumpPortalEvent::NewToken(_) => TerminalKind::NewToken,
        // §19: a drained partial is still a new-token observation, never a decode
        // loss. In the terminal drain (window closed) it only bumps the counter.
        PumpPortalEvent::PartialNewToken(_) => TerminalKind::NewToken,
    }
}

/// Pure application of a terminal-kind to observed state. No I/O, no printing.
fn apply_terminal_kind(kind: TerminalKind, stream_connected: &mut bool, counters: &mut Counters) {
    match kind {
        TerminalKind::Connected => {
            counters.connected_events += 1;
            *stream_connected = true;
        }
        TerminalKind::Disconnected => {
            counters.disconnect_events += 1;
            *stream_connected = false;
        }
        TerminalKind::Error => {
            counters.provider_errors += 1;
        }
        TerminalKind::Decode => {
            // P1 §8: any decode/schema loss fails the smoke.
            counters.decode_errors += 1;
        }
        TerminalKind::Trade => {
            counters.unexpected_trade_events += 1;
        }
        TerminalKind::Migration => {
            counters.migration_events += 1;
        }
        TerminalKind::NewToken => {
            // The observation window is over: count it, but launch NO RPC/market work.
            counters.new_token_events += 1;
        }
    }
}

/// Nonblocking drain of events already delivered to the harness channel. Consumes
/// only what is already queued (never waits for new events), so a queued
/// Disconnected / provider Error / unexpected Trade is reflected in state/counters
/// before the final PASS decision. No RPC, no quote calls, no provider output.
fn drain_queued_terminal_events(
    event_rx: &mut mpsc::Receiver<PumpPortalEvent>,
    stream_connected: &mut bool,
    counters: &mut Counters,
) {
    while let Ok(event) = event_rx.try_recv() {
        apply_terminal_kind(terminal_kind_of(&event), stream_connected, counters);
    }
}

/// Confirmed-state lag retry: attempt a fresh snapshot at 0/250/500/1000ms.
/// Returns the first successful snapshot, or None if all attempts fail. Read-only.
async fn snapshot_with_retry(
    oracle: &PumpMarketOracle,
    mint: &Pubkey,
) -> Option<pumpfun_sniper::market::MarketSnapshot> {
    let backoffs = [0u64, 250, 500, 1000];
    for (i, delay_ms) in backoffs.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }
        match oracle.snapshot(mint).await {
            Ok(snap) => return Some(snap),
            Err(_) if i + 1 < backoffs.len() => continue,
            Err(_) => return None,
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();
    let seconds = validate_seconds(args.seconds)?;
    let target_new_tokens = validate_target(args.target_new_tokens)?;

    let config = Config::load(&args.config).context("failed to load configuration")?;

    // --- Preflight: runtime exclusion lease, held for the whole binary. ---
    let _lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "observe_smoke")
        .context("failed to acquire runtime lease")?;

    // --- Preflight: PumpPortal must be enabled with a rotated key configured. ---
    if !config.pumpportal.enabled {
        return Err(anyhow!(
            "config.pumpportal.enabled must be true for the smoke harness"
        ));
    }
    if config.pumpportal.api_key.trim().is_empty() {
        // Never print the key, its length, or any prefix/suffix.
        return Err(anyhow!(
            "config.pumpportal.api_key must be set (rotated) for the smoke harness"
        ));
    }
    if config.rpc.endpoint.trim().is_empty() {
        return Err(anyhow!("config.rpc.endpoint must be non-empty"));
    }

    // --- Read-only RPC mainnet proof (never print the endpoint). ---
    let rpc = Arc::new(RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        Duration::from_millis(config.rpc.timeout_ms),
    ));

    let genesis = {
        let rpc = rpc.clone();
        tokio::task::spawn_blocking(move || rpc.get_genesis_hash())
            .await
            .context("RPC genesis task join failed")?
            // Fixed context; never interpolate the configured endpoint.
            .map_err(|_| anyhow!("configured Solana RPC request failed"))?
    };
    if genesis.to_string() != MAINNET_GENESIS {
        return Err(anyhow!(
            "configured RPC is not Solana mainnet (genesis mismatch)"
        ));
    }

    let current_slot = {
        let rpc = rpc.clone();
        tokio::task::spawn_blocking(move || rpc.get_slot())
            .await
            .context("RPC slot task join failed")?
            .map_err(|_| anyhow!("configured Solana RPC request failed"))?
    };
    if current_slot == 0 {
        return Err(anyhow!("RPC returned slot 0"));
    }
    println!("RPC: mainnet verified");
    println!("RPC current slot: {current_slot}");

    // --- PumpPortal client (single socket, free subscriptions only). ---
    let pp_config = PumpPortalConfig {
        ws_url: config.pumpportal.ws_url.clone(),
        api_key: config.pumpportal.api_key.clone(),
        reconnect_delay_ms: config.pumpportal.reconnect_delay_ms,
        max_reconnect_attempts: config.pumpportal.max_reconnect_attempts,
        ping_interval_secs: config.pumpportal.ping_interval_secs,
    };

    let (event_tx, mut event_rx) = mpsc::channel::<PumpPortalEvent>(EVENT_CHANNEL_CAPACITY);
    let client = PumpPortalClient::new(pp_config, event_tx);

    let plan = free_only_plan();
    client
        .start(plan)
        .await
        .context("failed to start PumpPortal client")?;

    let oracle = PumpMarketOracle::new(rpc.clone());

    // --- Observation loop. ---
    let mut counters = Counters::default();
    let start = Instant::now();
    let overall_deadline = start + Duration::from_secs(seconds);
    let connect_deadline = start + Duration::from_secs(CONNECT_DEADLINE_SECS);
    let mut ever_connected = false;
    // Current observed connectivity: only `Connected` sets it true, only
    // `Disconnected` sets it false. Used so an unresolved disconnect cannot PASS.
    let mut stream_connected = false;

    loop {
        let now = Instant::now();
        // Stop when the overall window closes.
        if now >= overall_deadline {
            break;
        }
        // Fail fast if never connected within the initial window.
        if !ever_connected && now >= connect_deadline {
            return Err(anyhow!(
                "no PumpPortal Connected event within {CONNECT_DEADLINE_SECS}s"
            ));
        }

        // Bounded wait, capped by the remaining window. If an event is ALREADY
        // queued, recv returns it immediately, so early-success can never run
        // ahead of a queued fail-closed event.
        let remaining = overall_deadline.saturating_duration_since(now);
        let wait = remaining.min(Duration::from_millis(500));
        let event = match tokio::time::timeout(wait, event_rx.recv()).await {
            Ok(Some(ev)) => ev,
            // Channel closed: fail-closed for connectivity, then stop observing.
            Ok(None) => {
                stream_connected = false;
                break;
            }
            // Quiet point (no queued event during this window): ONLY here may
            // early success be evaluated.
            Err(_) => {
                if early_success_candidate(target_new_tokens, stream_connected, &counters) {
                    break;
                }
                continue;
            }
        };

        match event {
            PumpPortalEvent::Connected => {
                counters.connected_events += 1;
                ever_connected = true;
                stream_connected = true;
                println!("PumpPortal: connected + free subscriptions synchronized");
            }
            PumpPortalEvent::Disconnected => {
                counters.disconnect_events += 1;
                stream_connected = false;
                eprintln!("WARN PumpPortal: disconnected (will attempt reconnect)");
            }
            PumpPortalEvent::Error(category) => {
                counters.provider_errors += 1;
                // `category` is already a sanitized fixed category from the stream.
                let safe = sanitize_short_text(&category, 80);
                eprintln!("WARN PumpPortal provider error: {safe}");
            }
            PumpPortalEvent::DecodeError(_) => {
                // P1 §8: local decode/schema loss fails the smoke. Fixed warning
                // only — never expose the decode category or any provider value.
                counters.decode_errors += 1;
                eprintln!("WARN PumpPortal decode/schema loss (message dropped)");
            }
            PumpPortalEvent::Migration(ev) => {
                counters.migration_events += 1;
                // The stream already validated the mint as a Pubkey.
                print!("Migration: mint={}", ev.mint);
                if let Some(pool_id) = ev.pool_id.as_deref() {
                    if Pubkey::from_str(pool_id).is_ok() {
                        print!(" pool_id={pool_id}");
                    }
                }
                println!();
            }
            PumpPortalEvent::Trade(_) => {
                // We request a free-only plan (zero trade subscriptions). A Trade
                // arriving anyway means the effective stream was NOT free-only, so
                // it must fail the smoke. Do not print/interpret trade fields.
                counters.unexpected_trade_events += 1;
                eprintln!("WARN unexpected PumpPortal Trade event on free-only smoke plan");
            }
            PumpPortalEvent::NewToken(ev) => {
                counters.new_token_events += 1;
                let n = counters.new_token_events;
                let symbol = sanitize_short_text(&ev.symbol, 32);
                // The stream contract says the mint was validated; re-parse it. A
                // parse failure here is a smoke failure.
                let mint = Pubkey::from_str(&ev.mint).map_err(|_| {
                    anyhow!("NewToken mint failed re-validation (stream contract violated)")
                })?;
                println!("NewToken #{n} mint={mint} symbol={symbol}");

                observe_mint(&oracle, &mint, &mut counters).await;
            }
            PumpPortalEvent::PartialNewToken(ev) => {
                // P1-OBSERVATION-SCHEMA-V2 §19: an incomplete provider create is a
                // valid discovery identity, NOT a decode error. Treat it exactly like
                // a NewToken for the smoke's canonical snapshot + hypothetical quote:
                // the mint was already validated by the stream. A partial that
                // resolves to a SOL market may count toward candidate success; a
                // partial that resolves unsupported simply does not increment the
                // hypothetical-quote success counter (observe_mint handles that). A
                // partial ALONE is never a decode error.
                counters.new_token_events += 1;
                let n = counters.new_token_events;
                let symbol = sanitize_short_text(&ev.symbol, 32);
                let mint = Pubkey::from_str(&ev.mint).map_err(|_| {
                    anyhow!("PartialNewToken mint failed re-validation (stream contract violated)")
                })?;
                println!("PartialNewToken #{n} mint={mint} symbol={symbol}");

                observe_mint(&oracle, &mint, &mut counters).await;
            }
        }
    }

    // --- Drain any events already queued (e.g. a Disconnected / Error / Trade
    // that arrived while the last NewToken's snapshot+quote RPC work ran) so a
    // known fail-closed event can't be bypassed. Nonblocking; no RPC. ---
    drain_queued_terminal_events(&mut event_rx, &mut stream_connected, &mut counters);

    // --- Clean shutdown. Give the worker a brief grace, then a second drain
    // (stronger), then drop the lease. ---
    client.stop();
    tokio::time::sleep(Duration::from_millis(250)).await;
    drain_queued_terminal_events(&mut event_rx, &mut stream_connected, &mut counters);

    let rpc_mainnet = genesis.to_string() == MAINNET_GENESIS;
    let result_pass = smoke_passes(rpc_mainnet, current_slot, stream_connected, &counters);

    println!("=== OBSERVATION SMOKE SUMMARY ===");
    println!("rpc_mainnet: PASS");
    println!("current_slot: {current_slot}");
    println!("connected_events: {}", counters.connected_events);
    println!("disconnect_events: {}", counters.disconnect_events);
    println!("stream_connected_at_end: {stream_connected}");
    println!("provider_errors: {}", counters.provider_errors);
    println!(
        "unexpected_trade_events: {}",
        counters.unexpected_trade_events
    );
    println!("decode_errors: {}", counters.decode_errors);
    println!("new_token_events: {}", counters.new_token_events);
    println!("migration_events: {}", counters.migration_events);
    println!(
        "market_snapshot_successes: {}",
        counters.market_snapshot_successes
    );
    println!(
        "hypothetical_quote_successes: {}",
        counters.hypothetical_quote_successes
    );
    println!(
        "market_observation_failures: {}",
        counters.market_observation_failures
    );
    println!("transaction_capability: ABSENT FROM SMOKE BINARY");
    println!("RESULT: {}", if result_pass { "PASS" } else { "FAIL" });

    if result_pass {
        Ok(())
    } else {
        Err(anyhow!("observation smoke did not meet PASS criteria"))
    }
}

/// The EXACT free-only subscription plan the smoke uses.
fn free_only_plan() -> PumpPortalSubscriptionPlan {
    PumpPortalSubscriptionPlan {
        new_tokens: true,
        migrations: true,
        token_trades: vec![],
        account_trades: vec![],
    }
}

/// Resolve an observed mint through the oracle: snapshot (with bounded retry) and,
/// if the quote asset is SOL, a hypothetical read-only quote. Updates counters.
async fn observe_mint(oracle: &PumpMarketOracle, mint: &Pubkey, counters: &mut Counters) {
    let snapshot = match snapshot_with_retry(oracle, mint).await {
        Some(snap) => snap,
        None => {
            counters.market_observation_failures += 1;
            return;
        }
    };
    counters.market_snapshot_successes += 1;

    let quote_asset_str = match snapshot.quote_asset {
        QuoteAsset::Sol => "SOL",
        QuoteAsset::Unsupported(_) => "unsupported",
    };
    let mark = match snapshot.mark_price_sol_per_token {
        Some(v) => format!("{v}"),
        None => "unavailable".to_string(),
    };
    println!("Market snapshot:");
    println!("  venue={:?}", snapshot.venue);
    println!("  quote_asset={quote_asset_str}");
    println!("  decimals={}", snapshot.base_decimals);
    println!("  mark_sol_per_token={mark}");
    println!("  slot={}", snapshot.slot);
    println!("  mayhem={}", snapshot.is_mayhem_mode);
    println!("  cashback={}", snapshot.is_cashback_coin);

    // Hypothetical quote ONLY for SOL-quoted markets.
    if snapshot.quote_asset != QuoteAsset::Sol {
        return;
    }

    match oracle.quote_buy_sol(mint, HYPOTHETICAL_BUY_LAMPORTS).await {
        Ok(q) => {
            counters.hypothetical_quote_successes += 1;
            let price = match q.expected_price_sol_per_token() {
                Some(v) => format!("{v}"),
                None => "unavailable".to_string(),
            };
            println!("Hypothetical quote (NOT SUBMITTED):");
            println!("  input_sol=0.001");
            println!("  venue={:?}", q.venue);
            println!("  expected_token_ui={}", q.base_amount_ui());
            println!("  expected_price_sol_per_token={price}");
            println!("  protocol_fee_bps={}", q.protocol_fee_bps);
            println!("  creator_fee_bps={}", q.creator_fee_bps);
            println!("  lp_fee_bps={}", q.lp_fee_bps);
            println!("  slot={}", q.slot);
        }
        Err(_) => {
            // A hypothetical-quote failure is not by itself fatal; the final PASS
            // criteria require at least one success across the run.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Static execution-absence guard. The needles are assembled from split
    /// fragments via `concat!()` so this test's own source never contains the
    /// forbidden contiguous token.
    #[test]
    fn test_source_has_no_execution_capability_references() {
        let src = include_str!("observe_smoke.rs");

        let forbidden: &[&str] = &[
            concat!("pumpfun_sniper::", "trading"),
            concat!("pumpfun_sniper::", "wallet"),
            concat!("pumpfun_sniper::", "position"),
            concat!("pumpfun_sniper::", "strategy"),
            concat!("pumpfun_sniper::", "cli"),
            concat!("PumpPortal", "Trader"),
            concat!("Key", "pair"),
            concat!("Sign", "er"),
            concat!("Pending", "Execution"),
            concat!("Position", "Manager"),
            concat!("Execution", "WalletRegistry"),
            concat!("WalletOwnership", "Probe"),
            concat!("send_", "transaction"),
            concat!("send_and_", "confirm_transaction"),
            concat!("send_raw_", "transaction"),
            concat!("simulate_", "transaction"),
            concat!("partial_", "sign"),
            concat!("try_", "sign"),
            concat!(".bu", "y("),
            concat!(".sel", "l("),
            concat!(".transf", "er("),
            // Forbidden execution/transaction types (future-regression guard).
            concat!("Versioned", "Trans", "action"),
            concat!("Trans", "action"),
            concat!("Instruc", "tion"),
            concat!("Mess", "age"),
            concat!("solana_sdk::", "signature"),
            concat!("solana_sdk::", "transaction"),
            concat!("solana_sdk::", "instruction"),
            concat!("solana_sdk::", "message"),
            concat!("Ji", "to"),
            concat!("ji", "to"),
        ];

        for needle in forbidden {
            assert!(
                !src.contains(needle),
                "forbidden execution reference present: {needle}"
            );
        }
    }

    #[test]
    fn test_source_does_not_reference_keypair_path() {
        let src = include_str!("observe_smoke.rs");
        let needle = concat!("KEYPAIR", "_PATH");
        assert!(!src.contains(needle), "source references keypair path env");
    }

    #[test]
    fn test_smoke_subscription_plan_is_free_only() {
        let plan = free_only_plan();
        assert!(plan.new_tokens);
        assert!(plan.migrations);
        assert!(plan.token_trades.is_empty());
        assert!(plan.account_trades.is_empty());
    }

    #[test]
    fn test_duration_bounds() {
        assert!(validate_seconds(10).is_ok());
        assert!(validate_seconds(120).is_ok());
        assert!(validate_seconds(9).is_err());
        assert!(validate_seconds(121).is_err());
    }

    #[test]
    fn test_target_event_bounds() {
        assert!(validate_target(1).is_ok());
        assert!(validate_target(10).is_ok());
        assert!(validate_target(0).is_err());
        assert!(validate_target(11).is_err());
    }

    #[test]
    fn test_mainnet_genesis_constant_is_full_hash() {
        assert_eq!(
            MAINNET_GENESIS,
            "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
        );
    }

    #[test]
    fn test_symbol_sanitizer_removes_controls() {
        // NUL, ESC, and newline are control chars and must be stripped; the
        // remaining printable characters (including the ANSI "[31m" body once its
        // ESC introducer is gone) are retained.
        let dirty = "AB\x00C\x1b[31mD\nE";
        let clean = sanitize_short_text(dirty, 64);
        assert_eq!(clean, "ABC[31mDE");
        assert!(!clean.contains('\x00'));
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\n'));
    }

    #[test]
    fn test_symbol_sanitizer_caps_length() {
        let long = "A".repeat(100);
        let capped = sanitize_short_text(&long, 32);
        assert_eq!(capped.chars().count(), 32);
    }

    #[test]
    fn test_hypothetical_quote_lamports_is_fixed() {
        assert_eq!(HYPOTHETICAL_BUY_LAMPORTS, 1_000_000);
    }

    /// Counters that satisfy every data-side PASS criterion, for policy tests.
    fn data_criteria_met() -> Counters {
        Counters {
            connected_events: 1,
            disconnect_events: 0,
            provider_errors: 0,
            new_token_events: 1,
            migration_events: 0,
            market_snapshot_successes: 1,
            hypothetical_quote_successes: 1,
            market_observation_failures: 0,
            unexpected_trade_events: 0,
            decode_errors: 0,
        }
    }

    #[test]
    fn test_pass_policy_requires_stream_connected_at_end() {
        let counters = data_criteria_met();
        // All data criteria true, but the stream is not currently connected.
        assert!(!smoke_passes(true, 42, false, &counters));
        // Same counters, connected at end => PASS.
        assert!(smoke_passes(true, 42, true, &counters));
    }

    #[test]
    fn test_pass_policy_allows_disconnect_then_reconnect() {
        // A disconnect that was later recovered: >=2 connects, >=1 disconnect,
        // and stream_connected true at the end.
        let mut counters = data_criteria_met();
        counters.connected_events = 2;
        counters.disconnect_events = 1;
        assert!(smoke_passes(true, 42, true, &counters));
    }

    #[test]
    fn test_pass_policy_rejects_unexpected_trade_event() {
        let mut counters = data_criteria_met();
        counters.unexpected_trade_events = 1;
        assert!(!smoke_passes(true, 42, true, &counters));
    }

    #[test]
    fn test_free_only_plan_still_has_zero_trade_subscriptions() {
        let plan = free_only_plan();
        assert_eq!(plan.token_trades.len(), 0);
        assert_eq!(plan.account_trades.len(), 0);
    }

    #[test]
    fn test_early_success_candidate_requires_clean_connected_state() {
        let counters = data_criteria_met();
        // Connected + all data criteria + zero errors/trades => candidate true.
        assert!(early_success_candidate(1, true, &counters));
        // Not currently connected => false.
        assert!(!early_success_candidate(1, false, &counters));
        // A provider error present => false (do not early-terminate after failure).
        let mut c_err = data_criteria_met();
        c_err.provider_errors = 1;
        assert!(!early_success_candidate(1, true, &c_err));
        // An unexpected trade present => false.
        let mut c_trade = data_criteria_met();
        c_trade.unexpected_trade_events = 1;
        assert!(!early_success_candidate(1, true, &c_trade));
    }

    #[test]
    fn test_queued_disconnect_drain_blocks_pass() {
        // Counters that would otherwise PASS, connected at the top.
        let mut counters = data_criteria_met();
        let mut stream_connected = true;
        // A Disconnected is already sitting in the channel.
        let (tx, mut rx) = mpsc::channel::<PumpPortalEvent>(4);
        tx.try_send(PumpPortalEvent::Disconnected).unwrap();

        drain_queued_terminal_events(&mut rx, &mut stream_connected, &mut counters);

        assert!(
            !stream_connected,
            "queued disconnect must clear connectivity"
        );
        assert_eq!(counters.disconnect_events, 1);
        assert!(
            !smoke_passes(true, 42, stream_connected, &counters),
            "a queued unresolved disconnect must block PASS"
        );
    }

    #[test]
    fn test_queued_unexpected_trade_blocks_pass() {
        // Uses the pure control-event classifier so no fake TradeEvent is needed.
        let mut counters = data_criteria_met();
        let mut stream_connected = true;
        apply_terminal_kind(TerminalKind::Trade, &mut stream_connected, &mut counters);
        assert_eq!(counters.unexpected_trade_events, 1);
        assert!(
            !smoke_passes(true, 42, stream_connected, &counters),
            "a queued unexpected trade must block PASS"
        );
    }

    #[test]
    fn test_channel_closed_is_fail_closed_for_connection() {
        // A closed channel (sender dropped) yields Ok(None); the harness must set
        // stream_connected=false rather than preserve a stale true.
        let (tx, mut rx) = mpsc::channel::<PumpPortalEvent>(1);
        drop(tx);
        let mut stream_connected = true;
        let mut counters = data_criteria_met();
        // Draining a closed+empty channel makes no state change...
        drain_queued_terminal_events(&mut rx, &mut stream_connected, &mut counters);
        // ...but the loop's Ok(None) arm is what clears connectivity. Model that
        // fail-closed transition explicitly here (the production loop does the same).
        stream_connected = false;
        assert!(!smoke_passes(true, 42, stream_connected, &counters));
    }

    // -----------------------------------------------------------------------
    // P1-PROVIDER-DECODE-TRUTH-001 §15 — decode-loss fails smoke.
    // -----------------------------------------------------------------------

    #[test]
    fn test_decode_error_blocks_smoke_pass() {
        // Counters that would otherwise PASS, but one decode/schema loss occurred.
        let mut counters = data_criteria_met();
        // Apply a decode event via the pure terminal classifier (no fake payload).
        let mut stream_connected = true;
        apply_terminal_kind(TerminalKind::Decode, &mut stream_connected, &mut counters);
        assert_eq!(counters.decode_errors, 1);
        assert!(
            !smoke_passes(true, 42, stream_connected, &counters),
            "any decode/schema loss must block PASS"
        );
        // And early success must not fire after a decode loss.
        assert!(!early_success_candidate(1, stream_connected, &counters));
    }

    #[test]
    fn test_decode_error_does_not_expose_payload_values() {
        // The smoke's decode handling is a fixed warning + a counter bump; it never
        // carries a decode category or provider value. Prove the source's decode
        // handling emits only the fixed message and no category interpolation.
        let src = include_str!("observe_smoke.rs");
        let fixed = "PumpPortal decode/schema loss (message dropped)";
        assert!(src.contains(fixed), "fixed decode warning must be present");
        // The DecodeError arms bind `_` (no payload), so no category can be
        // printed. Assert we never bind or format a decode payload. Needles are
        // assembled via concat!() so this assertion never self-triggers.
        let bind_e = concat!("Decode", "Error(e)");
        let bind_cat = concat!("Decode", "Error(category)");
        assert!(
            !src.contains(bind_e) && !src.contains(bind_cat),
            "smoke must not bind/expose the decode payload"
        );
    }

    // -----------------------------------------------------------------------
    // P1-OBSERVATION-SCHEMA-V2 §24 — partial-create smoke handling.
    // -----------------------------------------------------------------------

    #[test]
    fn test_partial_new_token_has_explicit_handler() {
        // The main loop must have an explicit PartialNewToken arm that observes the
        // mint through the oracle (snapshot + hypothetical quote), NOT a decode arm.
        let src = include_str!("observe_smoke.rs");
        let arm = concat!("PumpPortalEvent::", "PartialNewToken(ev) =>");
        assert!(
            src.contains(arm),
            "explicit PartialNewToken handler required"
        );
        // The terminal drain classifier must also handle it (exhaustive match).
        let drain_arm = concat!("PumpPortalEvent::", "PartialNewToken(_) =>");
        assert!(
            src.contains(drain_arm),
            "PartialNewToken must be classified in the terminal drain"
        );
    }

    #[test]
    fn test_partial_new_token_is_not_a_decode_error() {
        // A partial create maps to the NewToken terminal kind, so applying it never
        // touches decode_errors. Prove via the pure classifier/applier.
        let mut counters = data_criteria_met();
        counters.decode_errors = 0;
        counters.new_token_events = 0;
        let mut stream_connected = true;
        apply_terminal_kind(TerminalKind::NewToken, &mut stream_connected, &mut counters);
        assert_eq!(
            counters.decode_errors, 0,
            "partial must not be a decode loss"
        );
        assert_eq!(counters.new_token_events, 1);
    }

    #[test]
    fn test_partial_with_unsupported_quote_does_not_count_candidate_success() {
        // The smoke's candidate-success counters are market_snapshot_successes and
        // hypothetical_quote_successes; observe_mint only bumps the hypothetical-quote
        // success for a SOL-quoted market and returns early on unsupported. Model the
        // counting predicate: a partial whose canonical snapshot resolved but whose
        // quote asset is unsupported yields a snapshot success but NO hypothetical
        // quote success, which alone does not satisfy the data PASS criteria.
        let mut counters = Counters::default();
        // Snapshot resolved (unsupported quote asset => no quote attempt).
        counters.new_token_events = 1;
        counters.market_snapshot_successes = 1;
        counters.hypothetical_quote_successes = 0;
        // Without a hypothetical quote success the run cannot PASS.
        assert!(
            !smoke_passes(true, 42, true, &counters),
            "unsupported-quote partial must not satisfy candidate success alone"
        );
        // A partial that DID resolve to a SOL quote (quote success present) may PASS.
        counters.hypothetical_quote_successes = 1;
        counters.connected_events = 1;
        assert!(smoke_passes(true, 42, true, &counters));
    }

    #[test]
    fn test_true_decode_error_still_blocks_pass_with_partials_present() {
        // Even with partial-driven new-token/snapshot/quote successes, a genuine
        // DecodeError still blocks PASS (unchanged §19 contract).
        let mut counters = data_criteria_met();
        let mut stream_connected = true;
        apply_terminal_kind(TerminalKind::Decode, &mut stream_connected, &mut counters);
        assert_eq!(counters.decode_errors, 1);
        assert!(!smoke_passes(true, 42, stream_connected, &counters));
    }
}

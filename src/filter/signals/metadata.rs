//! Metadata signal provider for name/symbol quality analysis
//!
//! This provider analyzes token metadata (name, symbol, URI) for quality signals.
//! These are hot-path signals that can be computed quickly without RPC calls.

use async_trait::async_trait;
use regex::Regex;
use std::sync::OnceLock;

use crate::filter::signals::{Signal, SignalProvider, SignalType};
use crate::filter::types::SignalContext;

/// Static regex patterns for efficient matching
static SCAM_KEYWORDS: OnceLock<Regex> = OnceLock::new();
static SPAM_PATTERNS: OnceLock<Regex> = OnceLock::new();
static TRENDING_PATTERNS: OnceLock<Regex> = OnceLock::new();
static SUSPICIOUS_CHARS: OnceLock<Regex> = OnceLock::new();

fn scam_keywords() -> &'static Regex {
    SCAM_KEYWORDS.get_or_init(|| {
        Regex::new(r"(?i)(scam|rug|honeypot|free\s*money|100+x|1000+x|guaranteed|send.*sol|airdrop.*claim)")
            .expect("Invalid scam keywords regex")
    })
}

fn spam_patterns() -> &'static Regex {
    SPAM_PATTERNS.get_or_init(|| {
        Regex::new(r"(?i)(test|testing|asdf|qwerty|aaaa|1234|abcd)")
            .expect("Invalid spam patterns regex")
    })
}

fn trending_patterns() -> &'static Regex {
    TRENDING_PATTERNS.get_or_init(|| {
        Regex::new(r"(?i)(trump|biden|elon|musk|doge|pepe|shib|bonk|wojak|cat|dog|moon|rocket|ai\s*bot)")
            .expect("Invalid trending patterns regex")
    })
}

fn suspicious_chars() -> &'static Regex {
    SUSPICIOUS_CHARS.get_or_init(|| {
        // Unicode lookalikes, invisible chars (backreferences not supported)
        Regex::new(r"[\x{200B}-\x{200D}\x{FEFF}\x{00A0}]")
            .expect("Invalid suspicious chars regex")
    })
}

/// Metadata signal provider for name/symbol quality
pub struct MetadataSignalProvider {
    /// Minimum acceptable name length
    min_name_length: usize,
    /// Maximum acceptable name length
    max_name_length: usize,
    /// Minimum acceptable symbol length
    min_symbol_length: usize,
    /// Maximum acceptable symbol length
    max_symbol_length: usize,
}

impl Default for MetadataSignalProvider {
    fn default() -> Self {
        Self {
            min_name_length: 2,
            max_name_length: 32,
            min_symbol_length: 2,
            max_symbol_length: 10,
        }
    }
}

impl MetadataSignalProvider {
    /// Create a new metadata signal provider
    pub fn new() -> Self {
        Self::default()
    }

    /// Analyze token name quality
    fn analyze_name(&self, name: &str) -> Signal {
        // Empty or whitespace-only name
        if name.trim().is_empty() {
            return Signal::new(
                SignalType::NameQuality,
                -0.8,
                0.95,
                "Empty or whitespace-only name",
            );
        }

        let name_trimmed = name.trim();

        // Check for scam keywords (high confidence negative)
        if scam_keywords().is_match(name_trimmed) {
            return Signal::new(
                SignalType::NameQuality,
                -0.9,
                0.95,
                format!("Name contains scam keyword pattern"),
            );
        }

        // Check for spam/test patterns
        if spam_patterns().is_match(name_trimmed) {
            return Signal::new(
                SignalType::NameQuality,
                -0.5,
                0.8,
                "Name appears to be test/spam token",
            );
        }

        // Check for suspicious unicode/invisible chars
        if suspicious_chars().is_match(name_trimmed) {
            return Signal::new(
                SignalType::NameQuality,
                -0.6,
                0.85,
                "Name contains suspicious characters",
            );
        }

        // Length checks
        if name_trimmed.len() < self.min_name_length {
            return Signal::new(
                SignalType::NameQuality,
                -0.4,
                0.7,
                format!("Name too short: {} chars", name_trimmed.len()),
            );
        }

        if name_trimmed.len() > self.max_name_length {
            return Signal::new(
                SignalType::NameQuality,
                -0.2,
                0.6,
                format!("Name unusually long: {} chars", name_trimmed.len()),
            );
        }

        // All caps check (often spam)
        if name_trimmed.len() > 4
            && name_trimmed
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(|c| c.is_uppercase())
        {
            return Signal::new(
                SignalType::NameQuality,
                -0.2,
                0.5,
                "All caps name (often spam)",
            );
        }

        // Check for trending meme patterns (slightly positive - higher engagement)
        if trending_patterns().is_match(name_trimmed) {
            return Signal::new(
                SignalType::NameQuality,
                0.1,
                0.4, // Low confidence - memes can go either way
                "Name matches trending pattern",
            );
        }

        // Normal name
        Signal::neutral(SignalType::NameQuality, "Name appears normal")
    }

    /// Analyze token symbol quality
    fn analyze_symbol(&self, symbol: &str) -> Signal {
        // Empty or whitespace-only symbol
        if symbol.trim().is_empty() {
            return Signal::new(
                SignalType::SymbolQuality,
                -0.7,
                0.95,
                "Empty or whitespace-only symbol",
            );
        }

        let symbol_trimmed = symbol.trim();

        // Check for scam keywords in symbol
        if scam_keywords().is_match(symbol_trimmed) {
            return Signal::new(
                SignalType::SymbolQuality,
                -0.8,
                0.9,
                "Symbol contains scam keyword",
            );
        }

        // Length checks
        if symbol_trimmed.len() < self.min_symbol_length {
            return Signal::new(
                SignalType::SymbolQuality,
                -0.3,
                0.7,
                format!("Symbol too short: {} chars", symbol_trimmed.len()),
            );
        }

        if symbol_trimmed.len() > self.max_symbol_length {
            return Signal::new(
                SignalType::SymbolQuality,
                -0.2,
                0.6,
                format!("Symbol unusually long: {} chars", symbol_trimmed.len()),
            );
        }

        // Check for non-alphanumeric characters (unusual for symbols)
        if symbol_trimmed
            .chars()
            .any(|c| !c.is_alphanumeric() && c != '$')
        {
            return Signal::new(
                SignalType::SymbolQuality,
                -0.3,
                0.6,
                "Symbol contains unusual characters",
            );
        }

        // Normal symbol
        Signal::neutral(SignalType::SymbolQuality, "Symbol appears normal")
    }

    /// Analyze metadata URI quality
    fn analyze_uri(&self, uri: &str) -> Signal {
        // Empty URI
        if uri.trim().is_empty() {
            return Signal::new(
                SignalType::UriAnalysis,
                -0.3,
                0.6,
                "No metadata URI provided",
            );
        }

        let uri_trimmed = uri.trim();

        // Check for suspicious URI patterns
        if uri_trimmed.contains("bit.ly")
            || uri_trimmed.contains("tinyurl")
            || uri_trimmed.contains("t.co")
        {
            return Signal::new(
                SignalType::UriAnalysis,
                -0.5,
                0.7,
                "URI uses URL shortener (suspicious)",
            );
        }

        // Check for common legitimate hosting
        if uri_trimmed.contains("arweave.net")
            || uri_trimmed.contains("ipfs.io")
            || uri_trimmed.contains("nftstorage.link")
            || uri_trimmed.contains("pinata.cloud")
        {
            return Signal::new(
                SignalType::UriAnalysis,
                0.1,
                0.6,
                "URI uses established hosting",
            );
        }

        // Check for HTTPS
        if !uri_trimmed.starts_with("https://")
            && !uri_trimmed.starts_with("ipfs://")
            && !uri_trimmed.starts_with("ar://")
        {
            return Signal::new(
                SignalType::UriAnalysis,
                -0.2,
                0.5,
                "URI doesn't use secure protocol",
            );
        }

        // Default neutral
        Signal::neutral(SignalType::UriAnalysis, "URI appears standard")
    }
}

#[async_trait]
impl SignalProvider for MetadataSignalProvider {
    fn name(&self) -> &'static str {
        "metadata"
    }

    fn signal_types(&self) -> &[SignalType] {
        &[
            SignalType::NameQuality,
            SignalType::SymbolQuality,
            SignalType::UriAnalysis,
        ]
    }

    fn is_hot_path(&self) -> bool {
        true // All metadata signals are fast (no RPC)
    }

    fn max_latency_ms(&self) -> u64 {
        5 // Very fast, just string analysis
    }

    async fn compute_token_signals(&self, context: &SignalContext) -> Vec<Signal> {
        let start = std::time::Instant::now();

        let mut signals = Vec::with_capacity(3);

        // Analyze name
        let mut name_signal = self.analyze_name(&context.name);
        name_signal.latency = start.elapsed();
        signals.push(name_signal);

        // Analyze symbol
        let mut symbol_signal = self.analyze_symbol(&context.symbol);
        symbol_signal.latency = start.elapsed();
        signals.push(symbol_signal);

        // Analyze URI
        let mut uri_signal = self.analyze_uri(&context.uri);
        uri_signal.latency = start.elapsed();
        signals.push(uri_signal);

        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(name: &str, symbol: &str, uri: &str) -> SignalContext {
        SignalContext::from_new_token(
            "TestMint".to_string(),
            name.to_string(),
            symbol.to_string(),
            uri.to_string(),
            "Creator".to_string(),
            "BondingCurve".to_string(),
            1000,
            1_000_000_000,
            100_000_000,
            1.0,
        )
    }

    #[tokio::test]
    async fn test_scam_name_detection() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("FREE MONEY SCAM", "SCAM", "https://example.com");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value < -0.5, "Scam name should have negative value");
    }

    #[tokio::test]
    async fn test_spam_name_detection() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("test token asdf", "TEST", "https://example.com");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value < 0.0, "Spam name should have negative value");
    }

    #[tokio::test]
    async fn test_normal_name() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("Solana Dog Token", "SDOG", "https://arweave.net/metadata.json");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value >= 0.0, "Normal name should be neutral or positive");
    }

    #[tokio::test]
    async fn test_trending_name() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("Trump Pepe", "TPEPE", "https://example.com");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value > 0.0, "Trending name should be slightly positive");
    }

    #[tokio::test]
    async fn test_uri_shortener_detection() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("Token", "TKN", "https://bit.ly/abc123");
        let signals = provider.compute_token_signals(&context).await;

        let uri_signal = signals.iter().find(|s| s.signal_type == SignalType::UriAnalysis).unwrap();
        assert!(uri_signal.value < 0.0, "URL shortener should be suspicious");
    }

    #[tokio::test]
    async fn test_arweave_uri() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("Token", "TKN", "https://arweave.net/abc123");
        let signals = provider.compute_token_signals(&context).await;

        let uri_signal = signals.iter().find(|s| s.signal_type == SignalType::UriAnalysis).unwrap();
        assert!(uri_signal.value > 0.0, "Arweave URI should be positive");
    }

    #[tokio::test]
    async fn test_empty_name() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("", "TKN", "https://example.com");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value < -0.5, "Empty name should be very negative");
    }

    #[tokio::test]
    async fn test_all_caps_name() {
        let provider = MetadataSignalProvider::new();
        let context = make_context("SUPER TOKEN MOON", "STM", "https://example.com");
        let signals = provider.compute_token_signals(&context).await;

        let name_signal = signals.iter().find(|s| s.signal_type == SignalType::NameQuality).unwrap();
        assert!(name_signal.value < 0.0, "All caps name should be negative");
    }
}

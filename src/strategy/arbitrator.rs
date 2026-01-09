//! Decision Arbitrator
//!
//! Explicit priority ordering when multiple systems give conflicting signals.
//! Priority chain (highest to lowest):
//! 1. Fatal risks (absolute veto)
//! 2. Chain health (network conditions)
//! 3. Portfolio risk (capital protection)
//! 4. Rug predictor (position safety)
//! 5. Exit manager (position management)
//! 6. Strategy signals (entry/exit)
//! 7. Regime optimizations (sizing/style)

use super::fatal_risk::FatalRisk;
use super::portfolio_risk::PortfolioBlock;
use super::types::{
    ArbitratedDecision, ChainAction, DecisionSource,
    EntrySignal, ExitSignal, TradingAction, TokenRegime,
};

/// Rug prediction result
#[derive(Debug, Clone)]
pub struct RugPrediction {
    pub mint: String,
    pub probability: f64,
    pub warnings: Vec<String>,
    pub recommendation: &'static str,
}

/// Decision Arbitrator
pub struct DecisionArbitrator {
    /// Log override decisions
    log_overrides: bool,
}

impl DecisionArbitrator {
    /// Create a new decision arbitrator
    pub fn new() -> Self {
        Self { log_overrides: true }
    }

    /// Create without override logging
    pub fn quiet() -> Self {
        Self { log_overrides: false }
    }

    /// Arbitrate a new token entry decision
    pub fn arbitrate_entry(
        &self,
        mint: &str,
        fatal_result: Option<FatalRisk>,
        chain_action: &ChainAction,
        portfolio_result: Result<(), PortfolioBlock>,
        strategy_signal: Option<EntrySignal>,
        regime: &TokenRegime,
    ) -> ArbitratedDecision {
        let mut overridden = vec![];

        // Priority 1: Fatal risks are absolute
        if let Some(fatal) = fatal_result {
            if let Some(entry) = &strategy_signal {
                self.log_override(
                    DecisionSource::Strategy,
                    &format!("Entry signal for {} at {:.3} SOL", mint, entry.suggested_size_sol),
                    DecisionSource::FatalRisk,
                    &fatal.description(),
                );
                overridden.push((
                    DecisionSource::Strategy,
                    format!("Entry: {} at {:.3} SOL", entry.strategy, entry.suggested_size_sol),
                    format!("Overridden by fatal risk: {}", fatal.description()),
                ));
            }
            return ArbitratedDecision {
                action: TradingAction::FatalReject {
                    reason: fatal.description(),
                },
                source: DecisionSource::FatalRisk,
                overridden,
                confidence: 1.0,
            };
        }

        // Priority 2: Chain health blocks entries
        match chain_action {
            ChainAction::ExitOnlyMode => {
                if let Some(entry) = &strategy_signal {
                    self.log_override(
                        DecisionSource::Strategy,
                        &format!("Entry signal for {}", mint),
                        DecisionSource::ChainHealth,
                        "Exit-only mode due to chain congestion",
                    );
                    overridden.push((
                        DecisionSource::Strategy,
                        format!("Entry: {:?}", entry.strategy),
                        "Overridden by chain health: exit-only mode".to_string(),
                    ));
                }
                return ArbitratedDecision {
                    action: TradingAction::Skip {
                        reason: "Chain congestion critical - exit-only mode".to_string(),
                    },
                    source: DecisionSource::ChainHealth,
                    overridden,
                    confidence: 1.0,
                };
            }
            ChainAction::PauseNewEntries => {
                if let Some(entry) = &strategy_signal {
                    self.log_override(
                        DecisionSource::Strategy,
                        &format!("Entry signal for {}", mint),
                        DecisionSource::ChainHealth,
                        "New entries paused due to congestion",
                    );
                    overridden.push((
                        DecisionSource::Strategy,
                        format!("Entry: {:?}", entry.strategy),
                        "Overridden by chain health: entries paused".to_string(),
                    ));
                }
                return ArbitratedDecision {
                    action: TradingAction::Skip {
                        reason: "Chain congestion severe - new entries paused".to_string(),
                    },
                    source: DecisionSource::ChainHealth,
                    overridden,
                    confidence: 1.0,
                };
            }
            _ => {}
        }

        // Priority 3: Portfolio risk blocks new entries
        if let Err(block) = portfolio_result {
            if let Some(entry) = &strategy_signal {
                self.log_override(
                    DecisionSource::Strategy,
                    &format!("Entry signal for {}", mint),
                    DecisionSource::PortfolioRisk,
                    &block.description(),
                );
                overridden.push((
                    DecisionSource::Strategy,
                    format!("Entry: {:?} at {:.3} SOL", entry.strategy, entry.suggested_size_sol),
                    format!("Overridden by portfolio risk: {}", block.description()),
                ));
            }
            return ArbitratedDecision {
                action: TradingAction::Skip {
                    reason: block.description(),
                },
                source: DecisionSource::PortfolioRisk,
                overridden,
                confidence: 1.0,
            };
        }

        // Priority 4: Regime blocks (wash trade, deployer bleed)
        if regime.should_avoid() {
            if let Some(entry) = &strategy_signal {
                let regime_name = match regime {
                    TokenRegime::WashTrade { wash_pct, .. } => {
                        format!("Wash trade detected ({:.0}%)", wash_pct)
                    }
                    TokenRegime::DeployerBleed { deployer_holdings_pct, .. } => {
                        format!("Deployer bleed ({:.0}% holdings)", deployer_holdings_pct)
                    }
                    _ => "Toxic regime".to_string(),
                };
                self.log_override(
                    DecisionSource::Strategy,
                    &format!("Entry signal for {}", mint),
                    DecisionSource::RegimeOptimization,
                    &regime_name,
                );
                overridden.push((
                    DecisionSource::Strategy,
                    format!("Entry: {:?}", entry.strategy),
                    format!("Overridden by regime: {}", regime_name),
                ));
            }
            return ArbitratedDecision {
                action: TradingAction::Skip {
                    reason: format!("Regime indicates avoid: {:?}", regime),
                },
                source: DecisionSource::RegimeOptimization,
                overridden,
                confidence: regime.confidence(),
            };
        }

        // Priority 5: Strategy entry signal
        if let Some(entry) = strategy_signal {
            return ArbitratedDecision {
                action: TradingAction::Enter {
                    mint: entry.mint.clone(),
                    size_sol: entry.suggested_size_sol,
                    strategy: entry.strategy,
                },
                source: DecisionSource::Strategy,
                overridden,
                confidence: entry.confidence,
            };
        }

        // No entry signal
        ArbitratedDecision {
            action: TradingAction::Hold,
            source: DecisionSource::Strategy,
            overridden,
            confidence: 1.0,
        }
    }

    /// Arbitrate an exit decision for an existing position
    pub fn arbitrate_exit(
        &self,
        mint: &str,
        rug_prediction: Option<RugPrediction>,
        exit_signal: Option<ExitSignal>,
        chain_action: &ChainAction,
    ) -> ArbitratedDecision {
        let overridden = vec![];

        // Priority 1: Rug prediction overrides everything
        if let Some(rug) = rug_prediction {
            if rug.probability > 0.6 {
                if let Some(exit) = &exit_signal {
                    // Only log if it's a different exit reason
                    self.log_override(
                        DecisionSource::ExitManager,
                        &format!("Exit signal: {:?}", exit.reason),
                        DecisionSource::RugPredictor,
                        &format!("Rug predicted at {:.0}% probability", rug.probability * 100.0),
                    );
                }
                return ArbitratedDecision {
                    action: TradingAction::Exit {
                        mint: mint.to_string(),
                        pct: 100.0,
                        reason: format!("Rug predicted ({:.0}%): {}", rug.probability * 100.0, rug.warnings.join(", ")),
                    },
                    source: DecisionSource::RugPredictor,
                    overridden,
                    confidence: rug.probability,
                };
            }
        }

        // Priority 2: Chain health may force exit
        if matches!(chain_action, ChainAction::ExitOnlyMode) {
            // Don't force exit, but allow it
        }

        // Priority 3: Exit manager signal
        if let Some(exit) = exit_signal {
            return ArbitratedDecision {
                action: TradingAction::Exit {
                    mint: exit.mint.clone(),
                    pct: exit.pct_to_sell,
                    reason: format!("{:?}", exit.reason),
                },
                source: DecisionSource::ExitManager,
                overridden,
                confidence: 0.9,
            };
        }

        // Default: Hold
        ArbitratedDecision {
            action: TradingAction::Hold,
            source: DecisionSource::Strategy,
            overridden,
            confidence: 1.0,
        }
    }

    /// Arbitrate with all inputs
    pub fn arbitrate_full(
        &self,
        mint: &str,
        fatal_result: Option<FatalRisk>,
        chain_action: &ChainAction,
        portfolio_result: Result<(), PortfolioBlock>,
        rug_prediction: Option<RugPrediction>,
        exit_signal: Option<ExitSignal>,
        strategy_signal: Option<EntrySignal>,
        regime: &TokenRegime,
        has_position: bool,
    ) -> ArbitratedDecision {
        // If we have a position, prioritize exit checks
        if has_position {
            let exit_decision = self.arbitrate_exit(mint, rug_prediction, exit_signal, chain_action);
            if !matches!(exit_decision.action, TradingAction::Hold) {
                return exit_decision;
            }
        }

        // Otherwise, check entry
        self.arbitrate_entry(mint, fatal_result, chain_action, portfolio_result, strategy_signal, regime)
    }

    /// Log an override for debugging
    fn log_override(
        &self,
        overridden_source: DecisionSource,
        overridden_action: &str,
        override_source: DecisionSource,
        override_reason: &str,
    ) {
        if self.log_overrides {
            tracing::debug!(
                "Decision override: {:?} ({}) overridden by {:?} ({})",
                overridden_source,
                overridden_action,
                override_source,
                override_reason
            );
        }
    }
}

impl Default for DecisionArbitrator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{TradingStrategy, Urgency};

    fn make_entry_signal(mint: &str, size: f64) -> EntrySignal {
        EntrySignal {
            mint: mint.to_string(),
            strategy: TradingStrategy::SnipeAndScalp,
            confidence: 0.8,
            suggested_size_sol: size,
            urgency: Urgency::High,
            max_price: None,
            reason: "Test signal".to_string(),
        }
    }

    #[test]
    fn test_fatal_risk_overrides_entry() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            Some(FatalRisk::MintAuthorityActive),
            &ChainAction::ProceedNormally,
            Ok(()),
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::default(),
        );

        assert!(matches!(decision.action, TradingAction::FatalReject { .. }));
        assert_eq!(decision.source, DecisionSource::FatalRisk);
        assert!(!decision.overridden.is_empty());
    }

    #[test]
    fn test_portfolio_block_overrides_entry() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            None,
            &ChainAction::ProceedNormally,
            Err(PortfolioBlock::MaxPositionsReached { current: 5, max: 5 }),
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::default(),
        );

        assert!(matches!(decision.action, TradingAction::Skip { .. }));
        assert_eq!(decision.source, DecisionSource::PortfolioRisk);
    }

    #[test]
    fn test_chain_health_blocks_entry() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            None,
            &ChainAction::ExitOnlyMode,
            Ok(()),
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::default(),
        );

        assert!(matches!(decision.action, TradingAction::Skip { .. }));
        assert_eq!(decision.source, DecisionSource::ChainHealth);
    }

    #[test]
    fn test_wash_trade_regime_blocks_entry() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            None,
            &ChainAction::ProceedNormally,
            Ok(()),
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::WashTrade {
                wash_pct: 0.8,
                real_volume_sol: 0.5,
            },
        );

        assert!(matches!(decision.action, TradingAction::Skip { .. }));
        assert_eq!(decision.source, DecisionSource::RegimeOptimization);
    }

    #[test]
    fn test_entry_signal_passes() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            None,
            &ChainAction::ProceedNormally,
            Ok(()),
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::OrganicPump {
                confidence: 0.8,
                expected_duration_secs: 60,
            },
        );

        assert!(matches!(decision.action, TradingAction::Enter { .. }));
        assert_eq!(decision.source, DecisionSource::Strategy);
    }

    #[test]
    fn test_rug_prediction_forces_exit() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_exit(
            "test_mint",
            Some(RugPrediction {
                mint: "test_mint".to_string(),
                probability: 0.8,
                warnings: vec!["Creator selling".to_string()],
                recommendation: "EXIT NOW",
            }),
            None,
            &ChainAction::ProceedNormally,
        );

        assert!(matches!(decision.action, TradingAction::Exit { pct, .. } if pct == 100.0));
        assert_eq!(decision.source, DecisionSource::RugPredictor);
    }

    #[test]
    fn test_low_rug_probability_ignored() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_exit(
            "test_mint",
            Some(RugPrediction {
                mint: "test_mint".to_string(),
                probability: 0.3,
                warnings: vec![],
                recommendation: "MONITOR",
            }),
            None,
            &ChainAction::ProceedNormally,
        );

        assert!(matches!(decision.action, TradingAction::Hold));
    }

    #[test]
    fn test_no_signals_holds() {
        let arbitrator = DecisionArbitrator::quiet();

        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            None,
            &ChainAction::ProceedNormally,
            Ok(()),
            None,
            &TokenRegime::default(),
        );

        assert!(matches!(decision.action, TradingAction::Hold));
    }

    #[test]
    fn test_priority_ordering() {
        let arbitrator = DecisionArbitrator::quiet();

        // Fatal risk has highest priority
        let decision = arbitrator.arbitrate_entry(
            "test_mint",
            Some(FatalRisk::HoneypotDetected { failed_sells: 3 }),
            &ChainAction::ExitOnlyMode, // Chain also bad
            Err(PortfolioBlock::MaxPositionsReached { current: 5, max: 5 }), // Portfolio also blocked
            Some(make_entry_signal("test_mint", 0.1)),
            &TokenRegime::WashTrade { wash_pct: 0.9, real_volume_sol: 0.1 }, // Regime also bad
        );

        // Should be fatal risk, not any of the others
        assert_eq!(decision.source, DecisionSource::FatalRisk);
    }
}

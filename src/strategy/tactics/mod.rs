//! Cunning Tactics Module
//!
//! Advanced trading tactics for aggressive strategies:
//! - Front-run detection (accumulation pattern recognition)
//! - Rug prediction (early warning system)
//! - Sniper piggyback (follow profitable snipers)

pub mod frontrun;
pub mod piggyback;
pub mod rug_predict;

pub use frontrun::{AccumulationSignal, FrontRunDetector, FrontRunDetectorConfig};
pub use piggyback::{PiggybackSignal, SniperPiggyback, SniperPiggybackConfig, SniperStat};
pub use rug_predict::{RugPrediction, RugPredictor, RugPredictorConfig, RugWarningSignal};

use std::path::PathBuf;

use crate::common::cancellation::CancellationEvent;

mod input;
mod platform;
mod runner;

pub use runner::run_pty_command;

#[derive(Debug, Clone)]
pub struct PtyCommandConfig {
    pub shell: PathBuf,
    pub command: String,
    pub env_vars: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub cancellation: Option<CancellationEvent>,
}

use std::path::PathBuf;

use crate::common::cancellation::CancellationEvent;

//TODO: Depricated after `render_v2` standardizes on `PersistentShellSession`.
mod input;
mod platform;
//TODO: Depricated after `render_v2` standardizes on `PersistentShellSession`.
mod runner;
mod session;

//TODO: Depricated after `render_v2` standardizes on `PersistentShellSession`.
pub use runner::run_pty_command;
pub use session::{PersistentShellConfig, PersistentShellSession};

//TODO: Depricated after `render_v2` standardizes on `PersistentShellSession`.
#[derive(Debug, Clone)]
pub struct PtyCommandConfig {
    pub shell: PathBuf,
    pub command: String,
    pub env_vars: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub cancellation: Option<CancellationEvent>,
}

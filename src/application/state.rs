//! Execution is independent of editor/picker state; derive it from ownership
//! rather than maintaining another set of flags that can diverge on errors.
use super::Application;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecutionState {
    Editing,
    RunningAgent,
    Cancelling,
    ShellPassthrough,
    Stopping,
}

impl Application {
    pub(super) fn execution_state(&self) -> ExecutionState {
        if self.exit_requested {
            ExecutionState::Stopping
        } else if self.pending_shell.is_some()
            || self
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.is_leased())
        {
            ExecutionState::ShellPassthrough
        } else if let Some(active) = &self.active_operation {
            if active.cancellation.is_cancelled() {
                ExecutionState::Cancelling
            } else {
                ExecutionState::RunningAgent
            }
        } else {
            ExecutionState::Editing
        }
    }
}

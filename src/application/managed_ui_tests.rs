//! End-to-end PTY tests of a replaceable backend, without a production test mode.

use super::{Application, event_loop::run_interactive_application};
use crate::{agent::AgentConfig, common};
use crate::{
    agent::worker::AgentWorker,
    common::{
        cancellation::CancellationEvent,
        events::{BlockKind, EventSink, Outcome, OutputEvent},
    },
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

mod lifecycle_tests;
mod mixed_output_tests;

mod fixture;
mod support;
use support::{UiPty, assert_responsive, waiting_spinner_is_visible};
mod markdown_tests;
mod operation_tests;
mod responsiveness_tests;
mod shell_tests;

#[test]
#[ignore = "subprocess entry point for the managed UI PTY tests"]
fn fixture_child() {
    fixture::run_fixture();
}

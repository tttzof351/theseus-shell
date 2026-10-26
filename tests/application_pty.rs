use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::{Duration, Instant},
};

use portable_pty::PtySize;

#[path = "application_pty/streaming.rs"]
mod streaming;

#[path = "application_pty/terminal.rs"]
mod terminal;

#[path = "application_pty/xterm.rs"]
mod xterm;

#[path = "application_pty/shell.rs"]
mod shell;

#[path = "application_pty/editor.rs"]
mod editor;

#[path = "application_pty/agent.rs"]
mod agent;

#[path = "application_pty/plain.rs"]
mod plain;

#[path = "application_pty/support/mod.rs"]
mod support;
use support::*;

#[cfg(target_os = "linux")]
#[path = "application_pty/performance.rs"]
mod performance;

use std::{fs, io, path::Path};

#[cfg(not(test))]
use std::env;

use crate::common::output::CommandOutput;

const MAX_PERSISTED_COMMAND_HISTORY: usize = 99;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub input: String,
    pub output: CommandOutput,
}

#[cfg(not(test))]
pub(super) fn default_command_history_path() -> io::Result<std::path::PathBuf> {
    home_dir()
        .map(|home| {
            home.join(".theseus")
                .join("persist")
                .join("history_command.json")
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

pub(super) fn load_command_history(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;
    let history: Vec<String> = serde_json::from_str(&text).map_err(io::Error::other)?;

    Ok(trim_command_history(history))
}

pub(super) fn save_command_history(path: impl AsRef<Path>, history: &[String]) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let history = trim_command_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&history).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

pub(super) fn push_command_history(history: &mut Vec<String>, input: &str) {
    let input = input.trim();
    if input.is_empty() {
        return;
    }

    if history.last().is_some_and(|last| last == input) {
        return;
    }

    history.push(input.to_string());
    trim_command_history_in_place(history);
}

pub(super) fn format_history(history: &[CommandRecord]) -> String {
    let history = history
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let status = record
                .output
                .status_code
                .map_or("signal".to_string(), |code| code.to_string());
            let mut entry = format!("{}  {}  [status: {}]", index + 1, record.input, status);

            if !record.output.transcript.is_empty() {
                entry.push_str("\noutput:\n");
                entry.push_str(&record.output.transcript_lossy());
                if !entry.ends_with('\n') {
                    entry.push('\n');
                }
            }

            entry
        })
        .collect::<Vec<_>>()
        .join("\n");

    if history.is_empty() {
        history
    } else {
        format!("{history}\n")
    }
}

fn trim_command_history(mut history: Vec<String>) -> Vec<String> {
    history.retain(|entry| !entry.trim().is_empty());
    trim_command_history_in_place(&mut history);
    history
}

fn trim_command_history_in_place(history: &mut Vec<String>) {
    if history.len() > MAX_PERSISTED_COMMAND_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_COMMAND_HISTORY);
    }
}

#[cfg(not(test))]
fn home_dir() -> Option<std::path::PathBuf> {
    env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_command_history_keeps_latest_entries() {
        let mut history = (0..105)
            .map(|index| format!("command-{index}"))
            .collect::<Vec<_>>();

        push_command_history(&mut history, "latest");

        assert_eq!(history.len(), MAX_PERSISTED_COMMAND_HISTORY);
        assert_eq!(history.first().map(String::as_str), Some("command-7"));
        assert_eq!(history.last().map(String::as_str), Some("latest"));
    }

    #[test]
    fn persisted_command_history_skips_empty_and_adjacent_duplicates() {
        let mut history = vec!["ls".to_string()];

        push_command_history(&mut history, "  ");
        push_command_history(&mut history, "ls");
        push_command_history(&mut history, "pwd");

        assert_eq!(history, vec!["ls", "pwd"]);
    }
}

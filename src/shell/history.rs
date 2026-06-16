use std::{collections::HashSet, fs, io, path::Path};

#[cfg(not(test))]
use std::env;

use crate::common::output::CommandOutput;

const MAX_PERSISTED_HISTORY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub input: String,
    pub output: CommandOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub(super) struct InputHistoryEntry {
    pub text: String,
    pub kind: InputHistoryKind,
    pub mode: InputHistoryMode,
}

impl InputHistoryEntry {
    pub(super) fn new(
        text: impl Into<String>,
        kind: InputHistoryKind,
        mode: InputHistoryMode,
    ) -> Self {
        Self {
            text: text.into(),
            kind,
            mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum InputHistoryKind {
    Shell,
    Agent,
    Special,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum InputHistoryMode {
    SingleLine,
    SingleLineAsk,
    MultiLineAsk,
    MultiLineShell,
}

#[cfg(not(test))]
pub(super) fn default_command_history_v2_path() -> io::Result<std::path::PathBuf> {
    default_history_path("history_command_v2.json")
}

#[cfg(not(test))]
fn default_history_path(file_name: &str) -> io::Result<std::path::PathBuf> {
    home_dir()
        .map(|home| home.join(".theseus").join("persist").join(file_name))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

#[cfg(test)]
pub(super) fn load_string_history(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;
    let history: Vec<String> = serde_json::from_str(&text).map_err(io::Error::other)?;

    Ok(trim_command_history(history))
}

#[cfg(test)]
pub(super) fn save_string_history(path: impl AsRef<Path>, history: &[String]) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let history = trim_command_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&history).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

#[cfg(test)]
pub(super) fn push_string_history(history: &mut Vec<String>, input: &str) {
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

pub(super) fn load_input_history(path: impl AsRef<Path>) -> io::Result<Vec<InputHistoryEntry>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;
    let history: Vec<InputHistoryEntry> = serde_json::from_str(&text).map_err(io::Error::other)?;

    Ok(trim_input_history(history))
}

pub(super) fn save_input_history(
    path: impl AsRef<Path>,
    history: &[InputHistoryEntry],
) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let history = trim_input_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&history).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

pub(super) fn push_input_history(history: &mut Vec<InputHistoryEntry>, entry: InputHistoryEntry) {
    let Some(entry) = normalize_input_history_entry(entry) else {
        return;
    };

    history.push(entry);
    normalize_input_history_in_place(history);
}

/// Update the live draft entry and return the slot where that draft ended up.
///
/// The returned slot must be used for the next draft update because
/// normalization may remove duplicates and trim old entries when history is at
/// capacity. For an empty draft, entries from `slot` onward are removed and the
/// returned slot is the next append position.
pub(super) fn update_input_history_draft(
    history: &mut Vec<InputHistoryEntry>,
    slot: usize,
    entry: InputHistoryEntry,
) -> usize {
    let Some(entry) = normalize_input_history_entry(entry) else {
        history.truncate(slot);
        return history.len();
    };

    if history.len() > slot {
        history[slot] = entry;
        history.truncate(slot + 1);
    } else {
        history.push(entry);
    }

    normalize_input_history_in_place(history);
    history.len().saturating_sub(1)
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

#[cfg(test)]
fn trim_command_history(mut history: Vec<String>) -> Vec<String> {
    history.retain(|entry| !entry.trim().is_empty());
    trim_command_history_in_place(&mut history);
    history
}

#[cfg(test)]
fn trim_command_history_in_place(history: &mut Vec<String>) {
    if history.len() > MAX_PERSISTED_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_HISTORY);
    }
}

fn trim_input_history(mut history: Vec<InputHistoryEntry>) -> Vec<InputHistoryEntry> {
    history = history
        .into_iter()
        .filter_map(normalize_input_history_entry)
        .collect();
    normalize_input_history_in_place(&mut history);
    history
}

fn normalize_input_history_in_place(history: &mut Vec<InputHistoryEntry>) {
    dedupe_input_history_in_place(history);
    trim_input_history_in_place(history);
}

fn dedupe_input_history_in_place(history: &mut Vec<InputHistoryEntry>) {
    let mut seen = HashSet::with_capacity(history.len());
    history.retain_mut(normalize_input_history_entry_in_place);
    history.reverse();
    history.retain(|entry| seen.insert(entry.clone()));
    history.reverse();
}

fn trim_input_history_in_place(history: &mut Vec<InputHistoryEntry>) {
    if history.len() > MAX_PERSISTED_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_HISTORY);
    }
}

fn normalize_input_history_entry(mut entry: InputHistoryEntry) -> Option<InputHistoryEntry> {
    entry.text = entry.text.trim().to_string();
    (!entry.text.is_empty()).then_some(entry)
}

fn normalize_input_history_entry_in_place(entry: &mut InputHistoryEntry) -> bool {
    entry.text = entry.text.trim().to_string();
    !entry.text.is_empty()
}

#[cfg(not(test))]
fn home_dir() -> Option<std::path::PathBuf> {
    env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_string_history_keeps_latest_entries() {
        let mut history = (0..105)
            .map(|index| format!("command-{index}"))
            .collect::<Vec<_>>();

        push_string_history(&mut history, "latest");

        assert_eq!(history.len(), MAX_PERSISTED_HISTORY);
        assert_eq!(history.first().map(String::as_str), Some("command-6"));
        assert_eq!(history.last().map(String::as_str), Some("latest"));
    }

    #[test]
    fn persisted_string_history_skips_empty_and_adjacent_duplicates() {
        let mut history = vec!["ls".to_string()];

        push_string_history(&mut history, "  ");
        push_string_history(&mut history, "ls");
        push_string_history(&mut history, "pwd");

        assert_eq!(history, vec!["ls", "pwd"]);
    }

    #[test]
    fn string_history_round_trips_with_limit() {
        let path = std::env::temp_dir().join(format!(
            "theseus-string-history-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let history = (0..105)
            .map(|index| format!("ask-{index}"))
            .collect::<Vec<_>>();

        save_string_history(&path, &history).unwrap();
        let loaded = load_string_history(&path).unwrap();

        assert_eq!(loaded.len(), MAX_PERSISTED_HISTORY);
        assert_eq!(loaded.first().map(String::as_str), Some("ask-5"));
        assert_eq!(loaded.last().map(String::as_str), Some("ask-104"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn typed_history_skips_empty_and_adjacent_duplicates() {
        let mut history = vec![InputHistoryEntry::new(
            "ls",
            InputHistoryKind::Shell,
            InputHistoryMode::SingleLine,
        )];

        push_input_history(
            &mut history,
            InputHistoryEntry::new("  ", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
        );
        push_input_history(
            &mut history,
            InputHistoryEntry::new("ls", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
        );
        push_input_history(
            &mut history,
            InputHistoryEntry::new("ls", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
        );

        assert_eq!(
            history,
            vec![
                InputHistoryEntry::new("ls", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
                InputHistoryEntry::new("ls", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
            ]
        );
    }

    #[test]
    fn typed_history_moves_existing_duplicate_to_latest_position() {
        let mut history = vec![
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
        ];

        push_input_history(
            &mut history,
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
        );

        assert_eq!(
            history,
            vec![
                InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
                InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            ]
        );
    }

    #[test]
    fn typed_history_load_save_keeps_only_latest_duplicate_entries() {
        let path = std::env::temp_dir().join(format!(
            "theseus-typed-history-dedupe-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let history = vec![
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
        ];

        save_input_history(&path, &history).unwrap();
        let loaded = load_input_history(&path).unwrap();

        assert_eq!(
            loaded,
            vec![
                InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
                InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            ]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn typed_history_draft_update_removes_older_duplicate_before_slot() {
        let mut history = vec![
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
        ];

        update_input_history_draft(
            &mut history,
            2,
            InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
        );

        assert_eq!(
            history,
            vec![
                InputHistoryEntry::new("B", InputHistoryKind::Agent, InputHistoryMode::SingleLine),
                InputHistoryEntry::new("A", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            ]
        );
    }

    #[test]
    fn typed_history_round_trips_with_limit() {
        let path = std::env::temp_dir().join(format!(
            "theseus-typed-history-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let history = (0..105)
            .map(|index| {
                InputHistoryEntry::new(
                    format!("command-{index}"),
                    InputHistoryKind::Shell,
                    InputHistoryMode::SingleLine,
                )
            })
            .collect::<Vec<_>>();

        save_input_history(&path, &history).unwrap();
        let loaded = load_input_history(&path).unwrap();

        assert_eq!(loaded.len(), MAX_PERSISTED_HISTORY);
        assert_eq!(
            loaded.first().map(|entry| entry.text.as_str()),
            Some("command-5")
        );
        assert_eq!(
            loaded.last().map(|entry| entry.text.as_str()),
            Some("command-104")
        );

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"kind\": \"shell\""));
        assert!(saved.contains("\"mode\": \"single_line\""));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn typed_history_draft_updates_slot_and_truncates_following_entries() {
        let mut history = vec![
            InputHistoryEntry::new("old", InputHistoryKind::Shell, InputHistoryMode::SingleLine),
            InputHistoryEntry::new(
                "stale",
                InputHistoryKind::Shell,
                InputHistoryMode::SingleLine,
            ),
        ];

        let slot = update_input_history_draft(
            &mut history,
            1,
            InputHistoryEntry::new(
                "draft",
                InputHistoryKind::Agent,
                InputHistoryMode::MultiLineAsk,
            ),
        );

        assert_eq!(
            history,
            vec![
                InputHistoryEntry::new(
                    "old",
                    InputHistoryKind::Shell,
                    InputHistoryMode::SingleLine
                ),
                InputHistoryEntry::new(
                    "draft",
                    InputHistoryKind::Agent,
                    InputHistoryMode::MultiLineAsk,
                ),
            ]
        );
        assert_eq!(slot, 1);

        let slot = update_input_history_draft(
            &mut history,
            1,
            InputHistoryEntry::new(" ", InputHistoryKind::Agent, InputHistoryMode::MultiLineAsk),
        );

        assert_eq!(
            history,
            vec![InputHistoryEntry::new(
                "old",
                InputHistoryKind::Shell,
                InputHistoryMode::SingleLine,
            )]
        );
        assert_eq!(slot, 1);
    }

    #[test]
    fn typed_history_draft_update_returns_trimmed_slot_for_full_history() {
        let mut history = (0..MAX_PERSISTED_HISTORY)
            .map(|index| {
                InputHistoryEntry::new(
                    format!("command-{index}"),
                    InputHistoryKind::Shell,
                    InputHistoryMode::SingleLine,
                )
            })
            .collect::<Vec<_>>();
        let slot = history.len();

        let slot = update_input_history_draft(
            &mut history,
            slot,
            InputHistoryEntry::new(
                "draft-1",
                InputHistoryKind::Agent,
                InputHistoryMode::MultiLineAsk,
            ),
        );
        let slot = update_input_history_draft(
            &mut history,
            slot,
            InputHistoryEntry::new(
                "draft-2",
                InputHistoryKind::Agent,
                InputHistoryMode::MultiLineAsk,
            ),
        );

        assert_eq!(history.len(), MAX_PERSISTED_HISTORY);
        assert_eq!(slot, MAX_PERSISTED_HISTORY - 1);
        assert_eq!(
            history.first().map(|entry| entry.text.as_str()),
            Some("command-1")
        );
        assert_eq!(
            history.last(),
            Some(&InputHistoryEntry::new(
                "draft-2",
                InputHistoryKind::Agent,
                InputHistoryMode::MultiLineAsk
            ))
        );
    }
}

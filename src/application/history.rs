//! Persisted command history and live multiline drafts.
use super::{Application, Interaction, editor::EditorMode};
use crate::{commands::parse_slash_command, input};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io, path::Path};

const MAX_PERSISTED_HISTORY: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoryKind {
    Shell,
    Agent,
    Special,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoryMode {
    SingleLine,
    SingleLineAsk,
    MultiLineAsk,
    MultiLineShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub(super) struct HistoryEntry {
    pub(super) text: String,
    pub(super) kind: HistoryKind,
    pub(super) mode: HistoryMode,
}
impl Application {
    pub(super) fn sync_multiline_draft(&mut self) {
        let Some((mut slot, kind, mode)) = self.active_draft else {
            return;
        };
        let text = match &self.interaction {
            Interaction::Editor(editor)
                if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell) =>
            {
                multiline_history_text(&editor.buffer.text()).to_string()
            }
            _ => return,
        };

        self.history.truncate(slot.min(self.history.len()));
        if text.is_empty() {
            self.active_draft = Some((self.history.len(), kind, mode));
        } else {
            let entry = HistoryEntry { text, kind, mode };
            self.history.retain(|existing| existing != &entry);
            self.history.push(entry);
            if self.history.len() > MAX_PERSISTED_HISTORY {
                let removed = self.history.len() - MAX_PERSISTED_HISTORY;
                self.history.drain(..removed);
            }
            slot = self.history.len().saturating_sub(1);
            self.active_draft = Some((slot, kind, mode));
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    pub(super) fn store_special_history_if_needed(&mut self, input: &str) {
        if parse_slash_command(input).is_some() {
            self.store_special_history(input);
        }
    }

    pub(super) fn store_special_history(&mut self, input: &str) {
        self.store_history(HistoryEntry {
            text: input.to_string(),
            kind: HistoryKind::Special,
            mode: HistoryMode::SingleLine,
        });
    }

    pub(super) fn store_history(&mut self, mut entry: HistoryEntry) {
        entry.text = entry.text.trim().to_string();
        if entry.text.is_empty() {
            return;
        }
        self.history.retain(|existing| existing != &entry);
        self.history.push(entry);
        if self.history.len() > MAX_PERSISTED_HISTORY {
            self.history
                .drain(..self.history.len() - MAX_PERSISTED_HISTORY);
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    pub(super) fn formatted_history(&self) -> String {
        if self.command_records.is_empty() {
            return String::new();
        }
        self.command_records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let status = record
                    .status_code
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                format!(
                    "{}  {}  [status: {}]\noutput:\n{}",
                    index + 1,
                    record.input,
                    status,
                    record.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }
}

pub(super) fn load_history(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let history: Vec<HistoryEntry> =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    Ok(normalize_history(history))
}

fn multiline_history_text(text: &str) -> &str {
    let text = text.trim();
    let (body, last_line) = text.rsplit_once('\n').unwrap_or(("", text));
    if last_line.trim() == input::MULTILINE_SUBMIT_COMMAND {
        body.trim()
    } else {
        text
    }
}

fn normalize_history(history: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut history = history
        .into_iter()
        .filter_map(|mut entry| {
            entry.text = match entry.mode {
                HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell => {
                    multiline_history_text(&entry.text).to_string()
                }
                _ => entry.text.trim().to_string(),
            };
            (!entry.text.is_empty()).then_some(entry)
        })
        .collect::<Vec<_>>();
    let mut seen = HashSet::with_capacity(history.len());
    history.reverse();
    history.retain(|entry| seen.insert(entry.clone()));
    history.reverse();
    if history.len() > MAX_PERSISTED_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_HISTORY);
    }
    history
}

fn save_history(path: &Path, history: &[HistoryEntry]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let normalized = normalize_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&normalized).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_history_normalization_keeps_latest_trimmed_duplicate() {
        let shell = |text: &str| HistoryEntry {
            text: text.to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        };

        let normalized = normalize_history(vec![
            shell(" first "),
            shell("second"),
            shell("first"),
            shell("  "),
        ]);

        assert_eq!(normalized, vec![shell("second"), shell("first")]);
    }

    #[test]
    fn multiline_history_normalization_removes_legacy_submit_markers_and_duplicates() {
        for (kind, mode) in [
            (HistoryKind::Agent, HistoryMode::MultiLineAsk),
            (HistoryKind::Shell, HistoryMode::MultiLineShell),
        ] {
            let entry = |text: &str| HistoryEntry {
                text: text.into(),
                kind,
                mode,
            };
            assert_eq!(
                normalize_history(vec![
                    entry("first\n\nsecond\n /end \n"),
                    entry("another"),
                    entry("first\n\nsecond"),
                    entry("/end"),
                ]),
                vec![entry("another"), entry("first\n\nsecond")]
            );
            for text in [
                "explain /end",
                "/end\nmore text",
                "echo /end",
                "question\n/en",
            ] {
                assert_eq!(normalize_history(vec![entry(text)]), vec![entry(text)]);
            }
        }
        let entry = HistoryEntry {
            text: "/end".into(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::SingleLineAsk,
        };
        assert_eq!(normalize_history(vec![entry.clone()]), vec![entry]);
    }
}

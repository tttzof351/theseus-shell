//! Resumable trajectory discovery and selection.
use super::{
    Application, Interaction,
    picker::{PickerItem, PickerOutcome, PickerState, truncate_for_width},
};
use crate::logging::default_logs_dir;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub(super) struct ResumeSession {
    path: PathBuf,
    date: String,
    question: String,
}
impl Application {
    pub(super) fn open_resume(&mut self) -> io::Result<()> {
        let sessions = resume_sessions(self.config.agent_settings.max_resume_traj)?;
        if sessions.is_empty() {
            self.append_text("No resumable sessions found.\n");
            self.return_to_command_editor();
            return Ok(());
        }
        let items = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| PickerItem {
                id: index.to_string(),
                label: session.date.clone(),
                detail: truncate_for_width(
                    &session
                        .question
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    96,
                ),
            })
            .collect();
        self.interaction =
            Interaction::Resume(PickerState::new("Resume session", items, true), sessions);
        Ok(())
    }

    pub(super) fn finish_resume_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) => {
                let index = id.parse::<usize>().ok();
                let session = match (&self.interaction, index) {
                    (Interaction::Resume(_, sessions), Some(index)) => sessions.get(index).cloned(),
                    _ => None,
                };
                if let Some(session) = session {
                    self.start_operation(
                        crate::agent::worker::Operation::Resume(session.path),
                        "/resume".into(),
                    )?;
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Resume cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }
}
fn resume_sessions(limit: usize) -> io::Result<Vec<ResumeSession>> {
    let directory = default_logs_dir()?;
    resume_sessions_in(&directory, limit)
}

fn resume_sessions_in(directory: &Path, limit: usize) -> io::Result<Vec<ResumeSession>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_trajectory.json"))
        })
        .collect::<Vec<_>>();
    paths.sort_by_cached_key(|path| std::cmp::Reverse(resume_sort_key(path)));
    Ok(paths
        .into_iter()
        .take(limit)
        .filter_map(|path| resume_session_from_path(&path).ok())
        .collect())
}

fn resume_sort_key(path: &Path) -> (String, u64) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let timestamp = name.strip_suffix("_trajectory.json").unwrap_or(&name);
    if let Some((base, suffix)) = timestamp.rsplit_once('-')
        && base.split('-').count() == 6
        && let Ok(sequence) = suffix.parse()
    {
        return (base.into(), sequence);
    }
    (timestamp.into(), 0)
}

fn resume_session_from_path(path: &Path) -> io::Result<ResumeSession> {
    #[derive(Deserialize)]
    struct Snapshot {
        messages: Vec<ResumeMessage>,
    }
    #[derive(Deserialize)]
    struct ResumeMessage {
        role: Option<String>,
        content: Option<Value>,
    }
    let snapshot: Snapshot =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    let question = snapshot
        .messages
        .iter()
        .filter(|message| message.role.as_deref() == Some("user"))
        .filter_map(|message| message.content.as_ref())
        .filter_map(content_value_to_string)
        .map(|text| text.trim().to_string())
        .rfind(|text| !text.is_empty() && !text.starts_with("Last shell command:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no user question"))?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-date");
    let timestamp = file.strip_suffix("_trajectory.json").unwrap_or(file);
    let parts = timestamp.split('-').collect::<Vec<_>>();
    let date = if parts.len() >= 6 {
        format!(
            "{}-{}-{} {}:{}:{}",
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
        )
    } else {
        timestamp.to_string()
    };
    Ok(ResumeSession {
        path: path.to_path_buf(),
        date,
        question,
    })
}

fn content_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(content_value_to_string)
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(object) => object
            .get("text")
            .and_then(content_value_to_string)
            .or_else(|| object.get("content").and_then(content_value_to_string)),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::temporary_test_path;
    use serde_json::json;

    #[test]
    fn resume_orders_same_second_sessions_numerically_and_keeps_legacy_names() {
        let directory = temporary_test_path("resume-order");
        fs::create_dir_all(&directory).unwrap();
        for (suffix, question) in [("", "legacy"), ("-000002", "second"), ("-000010", "tenth")] {
            fs::write(
                directory.join(format!("2026-09-13-01-02-03{suffix}_trajectory.json")),
                serde_json::to_vec(&json!({"messages":[{"role":"user","content":question}]}))
                    .unwrap(),
            )
            .unwrap();
        }
        let sessions = resume_sessions_in(&directory, 3).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.question.as_str())
                .collect::<Vec<_>>(),
            ["tenth", "second", "legacy"]
        );
        assert!(sessions.iter().all(|s| s.date == "2026-09-13 01:02:03"));
        assert_eq!(
            resume_sessions_in(&directory, 1).unwrap()[0].question,
            "tenth"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

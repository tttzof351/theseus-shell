use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::{Hide, MoveDown, MoveToColumn, MoveUp, Show},
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, Clear, ClearType},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    common::{terminal_output, text::truncate_chars_end},
    input::{
        RawModeGuard, ViewportState, colorize_tag, is_control_key, is_key_press, is_plain_text_key,
    },
    logging::default_logs_dir,
};

#[derive(Debug, Clone)]
pub(super) struct ResumeSession {
    pub(super) path: PathBuf,
    date: String,
    question: String,
}

pub(super) fn select_resume_session(limit: usize) -> io::Result<Option<ResumeSession>> {
    let sessions = resume_sessions(limit)?;
    if sessions.is_empty() {
        return Ok(None);
    }

    select_session_with_search(sessions).map(Some)
}

fn resume_sessions(limit: usize) -> io::Result<Vec<ResumeSession>> {
    let logs_dir = default_logs_dir()?;
    let mut paths = fs::read_dir(logs_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_trajectory.json"))
        })
        .collect::<Vec<_>>();

    paths.sort_by(|left, right| right.file_name().cmp(&left.file_name()));

    Ok(paths
        .into_iter()
        .take(limit)
        .filter_map(|path| resume_session_from_path(&path).ok())
        .collect())
}

fn resume_session_from_path(path: &Path) -> io::Result<ResumeSession> {
    #[derive(Deserialize)]
    struct Snapshot {
        messages: Vec<Message>,
    }

    #[derive(Deserialize)]
    struct Message {
        role: Option<String>,
        content: Option<Value>,
    }

    let text = fs::read_to_string(path)?;
    let snapshot = serde_json::from_str::<Snapshot>(&text).map_err(io::Error::other)?;
    let question = snapshot
        .messages
        .iter()
        .filter(|message| message.role.as_deref() == Some("user"))
        .filter_map(|message| message.content.as_ref())
        .filter_map(content_value_to_string)
        .map(|question| question.trim().to_string())
        .rfind(|question| is_resume_title_question(question))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no user question"))?;

    Ok(ResumeSession {
        path: path.to_path_buf(),
        date: session_date(path),
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

            if text.is_empty() { None } else { Some(text) }
        }
        Value::Object(object) => object
            .get("text")
            .and_then(content_value_to_string)
            .or_else(|| object.get("content").and_then(content_value_to_string)),
        other => Some(other.to_string()),
    }
}

fn session_date(path: &Path) -> String {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return "unknown-date".to_string();
    };
    let timestamp = file_name
        .strip_suffix("_trajectory.json")
        .unwrap_or(file_name);
    let mut parts = timestamp.split('-').collect::<Vec<_>>();

    if parts.len() == 6 {
        return format!(
            "{}-{}-{} {}:{}:{}",
            parts.remove(0),
            parts.remove(0),
            parts.remove(0),
            parts.remove(0),
            parts.remove(0),
            parts.remove(0)
        );
    }

    timestamp.to_string()
}

fn select_session_with_search(sessions: Vec<ResumeSession>) -> io::Result<ResumeSession> {
    let mut state = ResumeSelectState::new(sessions);
    let _raw_mode = RawModeGuard::enable()?;
    let _cursor = HiddenCursorGuard::hide()?;
    terminal_output::with_stdout(|stdout| writeln!(stdout))?;
    render_resume_select(&mut state)?;

    loop {
        if let Event::Key(key) = event::read()? {
            if !is_key_press(key) {
                continue;
            }
            match key.code {
                KeyCode::Up => {
                    state.move_selected(-1);
                    render_resume_select(&mut state)?;
                }
                KeyCode::Down => {
                    state.move_selected(1);
                    render_resume_select(&mut state)?;
                }
                KeyCode::PageUp => {
                    state.move_selected(-(state.viewport.rows() as isize));
                    render_resume_select(&mut state)?;
                }
                KeyCode::PageDown => {
                    state.move_selected(state.viewport.rows() as isize);
                    render_resume_select(&mut state)?;
                }
                KeyCode::Home => {
                    state.viewport.set_selected(0);
                    render_resume_select(&mut state)?;
                }
                KeyCode::End => {
                    state
                        .viewport
                        .set_selected(state.filtered.len().saturating_sub(1));
                    render_resume_select(&mut state)?;
                }
                KeyCode::Enter => {
                    if let Some(session) = state.selected_session() {
                        let session = session.clone();
                        finish_resume_select(state.rendered_lines)?;
                        return Ok(session);
                    }
                }
                KeyCode::Backspace => {
                    state.query.pop();
                    state.refresh_filter();
                    render_resume_select(&mut state)?;
                }
                KeyCode::Char(c) if is_plain_text_key(key) => {
                    state.query.push(c);
                    state.refresh_filter();
                    render_resume_select(&mut state)?;
                }
                KeyCode::Esc => {
                    finish_resume_select(state.rendered_lines)?;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "resume cancelled",
                    ));
                }
                KeyCode::Char('c') if is_control_key(key) => {
                    finish_resume_select(state.rendered_lines)?;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "resume cancelled",
                    ));
                }
                _ => {}
            }
        }
    }
}

struct ResumeSelectState {
    sessions: Vec<ResumeSession>,
    query: String,
    filtered: Vec<usize>,
    viewport: ViewportState,
    rendered_lines: u16,
}

impl ResumeSelectState {
    fn new(sessions: Vec<ResumeSession>) -> Self {
        let filtered = filter_sessions(&sessions, "");
        let viewport = ViewportState::new(filtered.len(), resume_viewport_rows());
        Self {
            sessions,
            query: String::new(),
            filtered,
            viewport,
            rendered_lines: 0,
        }
    }

    fn refresh_filter(&mut self) {
        self.filtered = filter_sessions(&self.sessions, &self.query);
        self.viewport = ViewportState::new(self.filtered.len(), self.viewport.rows());
    }

    fn move_selected(&mut self, delta: isize) {
        self.viewport.move_selected(delta);
    }

    fn selected_session(&self) -> Option<&ResumeSession> {
        self.filtered
            .get(self.viewport.selected())
            .and_then(|index| self.sessions.get(*index))
    }
}

fn render_resume_select(state: &mut ResumeSelectState) -> io::Result<()> {
    let lines = resume_select_lines(state);
    state.rendered_lines = lines.len() as u16;

    terminal_output::with_stdout(|stdout| {
        for line in lines {
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            writeln!(stdout, "{line}")?;
        }

        execute!(stdout, MoveUp(state.rendered_lines))?;
        stdout.flush()
    })
}

fn resume_select_lines(state: &ResumeSelectState) -> Vec<String> {
    let mut lines = Vec::new();
    let columns = terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(80)
        .max(20)
        .saturating_sub(1);
    lines.push(colorize_tag("bold", "Resume session:"));
    lines.push(truncate_chars_end(
        &format!("Search: {}", state.query),
        columns,
    ));

    let total = state.filtered.len();
    let from = total.min(state.viewport.offset() + 1);
    let to = total.min(state.viewport.offset() + state.viewport.rows());
    lines.push(format!("Matches: {total}  Showing: {from}-{to}"));

    if state.filtered.is_empty() {
        lines.push(colorize_tag(
            "orange",
            &truncate_chars_end("  No matching sessions", columns),
        ));
        for _ in 1..state.viewport.rows() {
            lines.push(String::new());
        }
    } else {
        for row in 0..state.viewport.rows() {
            let filtered_index = state.viewport.offset() + row;
            let line = state
                .filtered
                .get(filtered_index)
                .and_then(|session_index| state.sessions.get(*session_index))
                .map(|session| {
                    format_session_row(
                        session,
                        filtered_index == state.viewport.selected(),
                        columns,
                    )
                })
                .unwrap_or_default();
            lines.push(line);
        }
    }

    lines
}

fn format_session_row(session: &ResumeSession, selected: bool, max_width: usize) -> String {
    let marker = if selected { ">" } else { " " };
    let question = normalize_question(&session.question);
    let line = truncate_chars_end(
        &format!("{marker} {} - {question}", session.date),
        max_width,
    );

    if selected {
        return colorize_tag("cyan", &colorize_tag("bold", &line));
    }

    line
}

fn normalize_question(question: &str) -> String {
    let one_line = question.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars_end(&one_line, 96)
}

fn is_resume_title_question(question: &str) -> bool {
    !question.is_empty() && !question.starts_with("Last shell command:")
}

fn filter_sessions(sessions: &[ResumeSession], query: &str) -> Vec<usize> {
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();

    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| {
            if terms.is_empty() {
                return true;
            }

            let haystack = format!(
                "{} {}",
                session.date.to_lowercase(),
                session.question.to_lowercase()
            );
            terms.iter().all(|term| haystack.contains(term))
        })
        .map(|(index, _)| index)
        .collect()
}

fn resume_viewport_rows() -> usize {
    let rows = terminal::size().map(|(_, rows)| rows).unwrap_or(24);
    usize::from(rows.saturating_sub(7)).clamp(5, 12)
}

fn finish_resume_select(rendered_lines: u16) -> io::Result<()> {
    terminal_output::with_stdout(|stdout| {
        execute!(
            stdout,
            MoveDown(rendered_lines),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        stdout.flush()
    })
}

struct HiddenCursorGuard;

impl HiddenCursorGuard {
    fn hide() -> io::Result<Self> {
        terminal_output::with_stdout(|stdout| execute!(stdout, Hide))?;
        Ok(Self)
    }
}

impl Drop for HiddenCursorGuard {
    fn drop(&mut self) {
        let _ = terminal_output::with_stdout(|stdout| execute!(stdout, Show));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_sessions_by_question_terms() {
        let sessions = vec![
            ResumeSession {
                path: PathBuf::from("one"),
                date: "2026-06-02 10:00:00".to_string(),
                question: "find pdf files".to_string(),
            },
            ResumeSession {
                path: PathBuf::from("two"),
                date: "2026-06-02 11:00:00".to_string(),
                question: "read hacker news".to_string(),
            },
        ];

        assert_eq!(filter_sessions(&sessions, "pdf"), vec![0]);
        assert_eq!(filter_sessions(&sessions, "hacker news"), vec![1]);
    }

    #[test]
    fn formats_session_date_from_trajectory_file_name() {
        assert_eq!(
            session_date(Path::new("2026-06-02-20-13-07_trajectory.json")),
            "2026-06-02 20:13:07"
        );
    }

    #[test]
    fn uses_last_real_user_question_as_title() {
        let path = std::env::temp_dir().join(format!(
            "2026-06-02-20-13-07_trajectory-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"
            {
              "messages": [
                { "role": "system", "content": "prompt" },
                { "role": "user", "content": "first question" },
                { "role": "assistant", "content": "answer" },
                { "role": "user", "content": "Last shell command: ls\nCommand output:\nfile" },
                { "role": "user", "content": "last question" }
              ]
            }
            "#,
        )
        .unwrap();

        let session = resume_session_from_path(&path).unwrap();

        assert_eq!(session.question, "last question");

        let _ = fs::remove_file(path);
    }
}

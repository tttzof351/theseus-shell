//! Completion candidates and cycling for the unified editor.

use super::{EditorMode, UnifiedEditor};
use crate::{
    application::{
        ansi::{byte_offset, char_len},
        home_dir,
    },
    commands,
};
use std::{collections::BTreeSet, env, fs, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application) struct CompletionCandidate {
    pub(in crate::application) replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application) struct CompletionCycle {
    pub(in crate::application) start: usize,
    pub(in crate::application) candidates: Vec<CompletionCandidate>,
    pub(in crate::application) selected: usize,
}

impl UnifiedEditor {
    pub(super) fn advance_completion(&mut self) {
        let restart_directory = self.completion.as_ref().is_some_and(|cycle| {
            cycle.candidates.len() == 1
                && cycle.candidates[0].replacement.ends_with(['/', '\\'])
                && self
                    .buffer
                    .current_line()
                    .chars()
                    .skip(cycle.start)
                    .take(self.buffer.cursor.char_offset.saturating_sub(cycle.start))
                    .collect::<String>()
                    == cycle.candidates[0].replacement
        });
        if restart_directory {
            self.completion = None;
        }
        if let Some(cycle) = &mut self.completion {
            cycle.selected = (cycle.selected + 1) % cycle.candidates.len();
            let replacement = cycle.candidates[cycle.selected].replacement.clone();
            self.buffer.replace_before_cursor(cycle.start, &replacement);
            return;
        }

        let line = self.buffer.current_line().to_string();
        let cursor = self.buffer.cursor.char_offset;
        let Some((start, candidates)) = completion_candidates(
            &line,
            cursor,
            matches!(self.mode, EditorMode::Command | EditorMode::Shell)
                && self.buffer.cursor.line == 0,
        ) else {
            return;
        };
        let replacement = candidates[0].replacement.clone();
        self.buffer.replace_before_cursor(start, &replacement);
        self.completion = Some(CompletionCycle {
            start,
            candidates,
            selected: 0,
        });
    }
}

fn completion_candidates(
    line: &str,
    cursor: usize,
    command_completion: bool,
) -> Option<(usize, Vec<CompletionCandidate>)> {
    let before_cursor = line.chars().take(cursor).collect::<String>();
    let start = completion_token_start(&before_cursor);
    let token = before_cursor.chars().skip(start).collect::<String>();
    let first_token = before_cursor[..byte_offset(&before_cursor, start)]
        .trim()
        .is_empty();

    if command_completion && first_token {
        let mut builtins = commands::slash_command_names()
            .chain(["cd", "exit"])
            .filter(|name| name.starts_with(&token))
            .map(str::to_string)
            .collect::<Vec<_>>();
        builtins.sort();
        builtins.dedup();
        if token.is_empty() || !builtins.is_empty() {
            let candidates = builtins
                .into_iter()
                .map(|replacement| CompletionCandidate { replacement })
                .collect::<Vec<_>>();
            return (!candidates.is_empty()).then_some((start, candidates));
        }
        let replacements = if looks_like_path_token(&token) {
            path_completions_with_common_prefix(&token)
        } else {
            path_executables(&token)
        };
        let candidates = replacements
            .into_iter()
            .map(|replacement| CompletionCandidate { replacement })
            .collect::<Vec<_>>();
        return (!candidates.is_empty()).then_some((start, candidates));
    }

    if command_completion && !first_token {
        let preceding_words = before_cursor[..byte_offset(&before_cursor, start)]
            .split_whitespace()
            .count();
        if preceding_words == 1 {
            let command = before_cursor.split_whitespace().next().unwrap_or_default();
            if matches!(command, "git" | "cargo") {
                let candidates = special_subcommand_completions(command, &token)
                    .into_iter()
                    .map(|replacement| CompletionCandidate { replacement })
                    .collect::<Vec<_>>();
                return (!candidates.is_empty()).then_some((start, candidates));
            }
        }
    }

    let candidates = path_completions_with_common_prefix(&token)
        .into_iter()
        .map(|replacement| CompletionCandidate { replacement })
        .collect::<Vec<_>>();
    (!candidates.is_empty()).then_some((start, candidates))
}

fn completion_token_start(text: &str) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    for index in (0..chars.len()).rev() {
        if !chars[index].is_whitespace() {
            continue;
        }
        let backslashes = chars[..index]
            .iter()
            .rev()
            .take_while(|ch| **ch == '\\')
            .count();
        if backslashes % 2 == 0 {
            return index + 1;
        }
    }
    0
}

fn looks_like_path_token(token: &str) -> bool {
    token.starts_with(['.', '/', '~']) || token.contains(['/', '\\'])
}

fn path_executables(prefix: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

fn special_subcommand_completions(command: &str, prefix: &str) -> Vec<String> {
    let values: &[&str] = match command {
        "git" => &[
            "add",
            "bisect",
            "branch",
            "checkout",
            "cherry-pick",
            "clean",
            "clone",
            "commit",
            "diff",
            "fetch",
            "grep",
            "init",
            "log",
            "merge",
            "mv",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "restore",
            "revert",
            "rm",
            "show",
            "stash",
            "status",
            "switch",
            "tag",
            "worktree",
        ],
        "cargo" => &[
            "add", "bench", "build", "check", "clean", "doc", "fetch", "fix", "fmt", "install",
            "login", "metadata", "new", "package", "publish", "remove", "run", "search", "test",
            "tree", "update",
        ],
        _ => return Vec::new(),
    };
    values
        .iter()
        .filter(|value| value.starts_with(prefix))
        .map(|value| (*value).to_string())
        .collect()
}

fn common_char_prefix(values: &[String]) -> Option<String> {
    let mut prefix = values.first()?.clone();
    for value in &values[1..] {
        let length = prefix
            .chars()
            .zip(value.chars())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(byte_offset(&prefix, length));
    }
    Some(prefix)
}

fn path_completions_with_common_prefix(token: &str) -> Vec<String> {
    let mut candidates = path_completions(token);
    if candidates.len() > 1
        && let Some(common) = common_char_prefix(&candidates)
        && char_len(&common) > char_len(token)
    {
        candidates.insert(0, common);
    }
    candidates
}

fn path_completions(token: &str) -> Vec<String> {
    let unescaped = unescape_shell_token(token);
    let (directory_text, file_prefix) = unescaped
        .rsplit_once('/')
        .map_or(("", unescaped.as_str()), |(directory, file)| {
            (directory, file)
        });
    let lookup_directory = if directory_text.is_empty() {
        PathBuf::from(".")
    } else if directory_text == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(rest) = directory_text.strip_prefix("~/") {
        home_dir().map_or_else(|| PathBuf::from(directory_text), |home| home.join(rest))
    } else {
        PathBuf::from(directory_text)
    };
    let Ok(entries) = fs::read_dir(lookup_directory) else {
        return Vec::new();
    };
    let rendered_directory = if directory_text.is_empty() {
        String::new()
    } else {
        format!("{directory_text}/")
    };
    let mut matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with(file_prefix)
                || (name.starts_with('.') && !file_prefix.starts_with('.'))
            {
                return None;
            }
            Some((
                escape_shell_token(&format!("{rendered_directory}{name}")),
                entry.path().is_dir(),
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    let single_directory = matches.len() == 1 && matches[0].1;
    let mut results = matches
        .into_iter()
        .map(|(mut replacement, _)| {
            if single_directory {
                replacement.push('/');
            }
            replacement
        })
        .collect::<Vec<_>>();
    results.sort();
    results
}

fn unescape_shell_token(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                output.push(next);
            } else {
                output.push(ch);
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn escape_shell_token(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '\\' | '\''
                    | '"'
                    | '$'
                    | '&'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '#'
            )
        {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_includes_git_subcommands() {
        let (_, candidates) = completion_candidates("git che", 7, true).unwrap();

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.replacement == "checkout")
        );
    }
}

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use crate::commands::slash_command_names;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletionToken {
    pub value: String,
    pub start: usize,
    pub is_command: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Completion {
    pub replacement: String,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletionState {
    pub token: CompletionToken,
    pub completions: Vec<Completion>,
    pub selected: Option<usize>,
}

pub(super) fn completion_state(line: &str, cursor_chars: usize) -> Option<CompletionState> {
    let token = token_before_cursor(line, cursor_chars)?;
    let completions = if token.is_command {
        command_completions(&token.value)
    } else if let Some(command) = second_argument_command(line, &token) {
        special_command_completions(&command, &token.value)
            .unwrap_or_else(|| path_completions(&token.value))
    } else {
        path_completions(&token.value)
    };

    (!completions.is_empty()).then_some(CompletionState {
        token,
        completions,
        selected: None,
    })
}

pub(super) fn token_before_cursor(line: &str, cursor_chars: usize) -> Option<CompletionToken> {
    let chars = line.chars().collect::<Vec<_>>();
    if cursor_chars > chars.len() {
        return None;
    }

    let mut start = cursor_chars;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }

    let value = chars[start..cursor_chars].iter().collect::<String>();
    let before_token = chars[..start].iter().collect::<String>();
    let is_command = before_token.trim().is_empty();

    Some(CompletionToken {
        value,
        start,
        is_command,
    })
}

fn second_argument_command(line: &str, token: &CompletionToken) -> Option<String> {
    if token.is_command {
        return None;
    }

    let before_token = line.chars().take(token.start).collect::<String>();
    let mut words = before_token.split_whitespace();
    let command = words.next()?;
    words.next().is_none().then(|| command.to_string())
}

fn command_completions(prefix: &str) -> Vec<Completion> {
    let mut candidates = BTreeSet::new();

    for command in slash_command_names().chain(["cd", "exit"]) {
        if command.starts_with(prefix) {
            candidates.insert(command.to_string());
        }
    }

    if prefix.is_empty() {
        return candidates
            .into_iter()
            .map(|name| Completion {
                replacement: name.clone(),
                display: name,
            })
            .collect();
    }

    if !candidates.is_empty() {
        return candidates
            .into_iter()
            .map(|name| Completion {
                replacement: name.clone(),
                display: name,
            })
            .collect();
    }

    if looks_like_path(prefix) {
        return path_completions(prefix);
    }

    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(prefix) {
                        candidates.insert(name);
                    }
                }
            }
        }
    }

    candidates
        .into_iter()
        .map(|name| Completion {
            replacement: name.clone(),
            display: name,
        })
        .collect()
}

fn special_command_completions(command: &str, prefix: &str) -> Option<Vec<Completion>> {
    let subcommands = match command {
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
        ][..],
        "cargo" => &[
            "add", "bench", "build", "check", "clean", "doc", "fetch", "fix", "fmt", "install",
            "login", "metadata", "new", "package", "publish", "remove", "run", "search", "test",
            "tree", "update",
        ][..],
        _ => return None,
    };

    Some(
        subcommands
            .iter()
            .filter(|subcommand| subcommand.starts_with(prefix))
            .map(|subcommand| Completion {
                replacement: (*subcommand).to_string(),
                display: (*subcommand).to_string(),
            })
            .collect(),
    )
}

fn path_completions(prefix: &str) -> Vec<Completion> {
    let (dir, name_prefix) = split_path_prefix(prefix);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut completions = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&name_prefix)
                || (name.starts_with('.') && !name_prefix.starts_with('.'))
            {
                return None;
            }

            let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
            let replacement = join_completion_prefix(prefix, &name);
            let display = if is_dir { format!("{name}/") } else { name };

            Some(Completion {
                replacement,
                display,
            })
        })
        .collect::<Vec<_>>();

    completions.sort_by(|left, right| left.display.cmp(&right.display));
    if completions.len() == 1 && completions[0].display.ends_with('/') {
        completions[0].replacement.push('/');
    }
    completions
}

fn split_path_prefix(prefix: &str) -> (PathBuf, String) {
    let expanded = expand_home_prefix(prefix);
    if prefix.ends_with(['/', '\\']) {
        return (PathBuf::from(expanded), String::new());
    }

    let path = Path::new(&expanded);

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        Some(_) | None => PathBuf::from("."),
    };
    let name_prefix = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    (dir, name_prefix)
}

fn expand_home_prefix(prefix: &str) -> String {
    #[cfg(unix)]
    if let Some(rest) = prefix.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }

    prefix.to_string()
}

fn join_completion_prefix(prefix: &str, name: &str) -> String {
    let separator_index = prefix.rfind(['/', '\\']).map(|index| index + 1);
    match separator_index {
        Some(index) => format!("{}{}", &prefix[..index], name),
        None => name.to_string(),
    }
}

fn looks_like_path(text: &str) -> bool {
    text.contains('/') || text.starts_with('~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_completion_token_before_cursor() {
        assert_eq!(
            token_before_cursor("cat src/mai", 11),
            Some(CompletionToken {
                value: "src/mai".to_string(),
                start: 4,
                is_command: false,
            })
        );
    }

    #[test]
    fn detects_command_completion_token() {
        assert_eq!(
            token_before_cursor("/con", 4),
            Some(CompletionToken {
                value: "/con".to_string(),
                start: 0,
                is_command: true,
            })
        );
    }

    #[test]
    fn completes_builtin_slash_commands_before_path_completion() {
        assert_eq!(
            command_completions("/conf"),
            vec![Completion {
                replacement: "/config".to_string(),
                display: "/config".to_string(),
            }]
        );
    }

    #[test]
    fn completes_status_slash_command() {
        assert_eq!(
            command_completions("/stat"),
            vec![Completion {
                replacement: "/status".to_string(),
                display: "/status".to_string(),
            }]
        );
    }

    #[test]
    fn detects_second_argument_command() {
        let token = token_before_cursor("git co", 6).unwrap();

        assert_eq!(
            second_argument_command("git co", &token),
            Some("git".to_string())
        );
    }

    #[test]
    fn does_not_use_special_completion_after_second_argument() {
        let token = token_before_cursor("git checkout sr", 15).unwrap();

        assert_eq!(second_argument_command("git checkout sr", &token), None);
    }

    #[test]
    fn completes_git_subcommands_for_second_argument() {
        let completions = special_command_completions("git", "c").unwrap();

        assert!(completions.iter().any(|item| item.replacement == "commit"));
        assert!(
            completions
                .iter()
                .any(|item| item.replacement == "checkout")
        );
        assert!(!completions.iter().any(|item| item.replacement == "status"));
    }

    #[test]
    fn unknown_special_command_falls_back_to_path_completion() {
        assert!(special_command_completions("vim", "sr").is_none());
    }

    #[test]
    fn joins_completion_inside_path_prefix() {
        assert_eq!(join_completion_prefix("src/mai", "main.rs"), "src/main.rs");
        assert_eq!(join_completion_prefix("mai", "main.rs"), "main.rs");
    }

    #[test]
    fn split_path_prefix_keeps_trailing_directory() {
        assert_eq!(
            split_path_prefix("src/"),
            (PathBuf::from("src/"), String::new())
        );
    }

    #[test]
    fn multiple_directory_candidates_do_not_include_trailing_slash() {
        let temp_root =
            env::temp_dir().join(format!("theseus-read-line-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(temp_root.join("src")).unwrap();
        fs::create_dir_all(temp_root.join("scripts")).unwrap();

        let prefix = temp_root.join("s").to_string_lossy().into_owned();
        let completions = path_completions(&prefix);
        fs::remove_dir_all(&temp_root).unwrap();

        assert!(
            completions
                .iter()
                .any(|item| item.replacement.ends_with("/src"))
        );
        assert!(
            completions
                .iter()
                .any(|item| item.replacement.ends_with("/scripts"))
        );
        assert!(
            !completions
                .iter()
                .any(|item| item.replacement.ends_with("/src/")
                    || item.replacement.ends_with("/scripts/"))
        );
    }

    #[test]
    fn single_directory_candidate_includes_trailing_slash() {
        let temp_root = env::temp_dir().join(format!(
            "theseus-read-line-single-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(temp_root.join("src")).unwrap();

        let prefix = temp_root.join("s").to_string_lossy().into_owned();
        let completions = path_completions(&prefix);
        fs::remove_dir_all(&temp_root).unwrap();

        assert_eq!(completions.len(), 1);
        assert!(completions[0].replacement.ends_with("/src/"));
        assert_eq!(completions[0].display, "src/");
    }
}

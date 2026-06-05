use std::{env, path::PathBuf};

use crate::input::colorize_nested;

pub(super) fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }

    #[cfg(unix)]
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }

    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

pub(super) fn default_prompt(working_dir: Option<&PathBuf>) -> String {
    let user_name = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "user".to_string());
    let name = working_dir
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("theseus");

    colorize_nested(&format!(
        "<bold><cyan>{user_name}</cyan></bold> <bold><magenta>{name}</magenta></bold>> "
    ))
}

#[cfg(unix)]
pub fn default_shell() -> PathBuf {
    env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(windows)]
pub fn default_shell() -> PathBuf {
    env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

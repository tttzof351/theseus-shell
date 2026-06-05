use std::{
    env, io,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::args::string_arg;

pub(super) fn expanded_path_arg(arguments: &Value, key: &str) -> io::Result<PathBuf> {
    expanded_path_arg_with_home(arguments, key, home_dir().as_deref())
}

pub(super) fn expanded_path_arg_with_home(
    arguments: &Value,
    key: &str,
    home_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    string_arg(arguments, key).map(|path| expand_home_path(path, home_dir))
}

pub(super) fn expand_home_path(path: &str, home_dir: Option<&Path>) -> PathBuf {
    if path == "~" {
        return home_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home_dir) = home_dir
    {
        return home_dir.join(rest);
    }

    PathBuf::from(path)
}

pub(super) fn expand_home_path_default(path: &str) -> PathBuf {
    expand_home_path(path, home_dir().as_deref())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

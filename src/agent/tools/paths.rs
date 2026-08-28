use std::{
    env, io,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::args::string_arg;

pub(super) fn expanded_path_arg_with_home(
    arguments: &Value,
    key: &str,
    home_override: Option<&Path>,
    working_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    let default_home = home_dir();
    let home_dir = home_override.or(default_home.as_deref());
    string_arg(arguments, key).map(|path| {
        let expanded = expand_home_path(path, home_dir);
        resolve_relative_path(expanded, working_dir)
    })
}

/// Resolves a tool path argument the same way the bash tool resolves its
/// working directory: absolute paths are used as-is, while relative paths are
/// joined to the shell session's working directory. File tools must not depend
/// on the process-wide current dir, which is only kept in sync as a side
/// effect of `Application::run_shell`.
fn resolve_relative_path(path: PathBuf, working_dir: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match working_dir {
        Some(working_dir) => working_dir.join(path),
        None => path,
    }
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

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn default_home_expansion_precedes_working_dir_resolution() {
        let Some(home) = home_dir() else {
            return;
        };
        let working_dir = env::temp_dir().join("theseus-path-resolution-cwd");
        let arguments = json!({ "path": "~/.theseus/config.jsonc" });

        let resolved =
            expanded_path_arg_with_home(&arguments, "path", None, Some(&working_dir)).unwrap();

        assert_eq!(resolved, home.join(".theseus/config.jsonc"));
    }
}

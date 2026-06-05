use std::{env, path::Path};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchToolAvailability {
    pub rg: bool,
    pub jq: bool,
}

pub fn search_tool_availability() -> SearchToolAvailability {
    SearchToolAvailability {
        rg: command_exists("rg"),
        jq: command_exists("jq"),
    }
}

fn command_exists(command: &str) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return is_executable_path(Path::new(command));
    }

    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path).any(|dir| is_executable_path(&dir.join(command)))
}

#[cfg(unix)]
fn is_executable_path(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };

    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable_path(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }

    let Some(pathext) = env::var_os("PATHEXT") else {
        return false;
    };

    env::split_paths(&pathext)
        .filter_map(|extension| extension.to_str().map(str::to_string))
        .any(|extension| {
            path.with_extension(extension.trim_start_matches('.'))
                .is_file()
        })
}

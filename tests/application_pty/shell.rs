use super::*;

fn run_git(repo: &Path, arguments: &[&str]) -> io::Result<()> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn git_branch_fixture() -> io::Result<(PathBuf, PathBuf)> {
    let home = temp_home()?;
    let repo = home.join("theseus-shell");
    let pager = home.join("git-branch-pager.sh");
    fs::create_dir_all(&repo)?;
    fs::write(
        &pager,
        "#!/bin/sh\nprintf '\\r\\033[K'\ncat\nprintf '\\r\\033[K'\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&pager, fs::Permissions::from_mode(0o755))?;
    }
    run_git(&repo, &["init", "-q"])?;
    run_git(&repo, &["config", "user.email", "application@example.test"])?;
    run_git(&repo, &["config", "user.name", "Application Test"])?;
    run_git(&repo, &["config", "color.ui", "always"])?;
    fs::write(repo.join("tracked.txt"), "fixture\n")?;
    run_git(&repo, &["add", "tracked.txt"])?;
    run_git(&repo, &["commit", "-qm", "fixture"])?;
    run_git(&repo, &["branch", "-m", "master"])?;
    run_git(&repo, &["branch", "exps"])?;
    run_git(&repo, &["checkout", "-q", "exps"])?;
    Ok((home, repo))
}

#[test]
fn shell_handoff_preserves_input_sent_in_same_write_as_command_enter() -> io::Result<()> {
    for shell in ["/bin/sh", "/bin/dash", "/bin/bash", "/bin/zsh"]
        .map(Path::new)
        .into_iter()
        .filter(|shell| shell.exists())
    {
        let mut app = ApplicationPty::start_with_shell(
            temp_home()?,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            Some(shell),
        )?;
        for (request, expected) in [
            (
                "read value; printf 'RECEIVED=%s\\n' \"$value\"\rimmediate-stdin\r",
                "RECEIVED=immediate-stdin",
            ),
            (
                "sh -c 'read first; read second; printf \"CHILD=%s/%s\\n\" \"$first\" \"$second\"'\rПривет\rsecond-line\r",
                "CHILD=Привет/second-line",
            ),
        ] {
            app.write(request)?;
            let bytes = app
                .wait_until(|bytes| {
                    screen_rows(bytes).iter().any(|row| row == expected)
                        && settled_prompt_is_visible(bytes)
                })
                .map_err(|error| {
                    io::Error::new(error.kind(), format!("shell {}: {error}", shell.display()))
                })?;
            assert!(!String::from_utf8_lossy(&bytes).contains("__THESEUS_READY_"));
        }
        app.exit()?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn shell_is_ready_before_first_prompt_and_reused_for_commands() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home()?;
    let startup_log = home.join("shell-starts");
    let wrapper = home.join("test-shell");
    fs::write(
        &wrapper,
        "#!/bin/sh\nsleep 0.2\nprintf 'started\\n' >> \"$HOME/shell-starts\"\nexec /bin/sh -i\n",
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
    let mut shell = ApplicationPty::start_with_shell(
        home,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(&wrapper),
    )?;

    // start_with_shell returns as soon as the initial prompt is visible.
    // Check readiness directly instead of asserting a machine-dependent latency.
    assert_eq!(fs::read_to_string(&startup_log)?, "started\n");
    for marker in ["FIRST_READY", "SECOND_READY"] {
        let offset = shell.transcript_len();
        shell.write(&format!(
            "printf '%s%s\\n' '{}' '{}'\r",
            &marker[..5],
            &marker[5..]
        ))?;
        shell.wait_until(|bytes| {
            find_bytes(bytes.get(offset..).unwrap_or_default(), marker.as_bytes()).is_some()
                && settled_prompt_is_visible(bytes)
        })?;
    }
    assert_eq!(fs::read_to_string(&startup_log)?, "started\n");
    shell.exit()
}

#[test]
fn streamed_shell_output_does_not_move_when_diff_renderer_resumes() -> io::Result<()> {
    let mut shell = ApplicationPty::start()?;
    let offset = shell.transcript_len();

    // The external terminal expands this tab using its native eight-column tab
    // stops. Once the command finishes, the application rebuilds the same visible
    // transcript from VirtualScreen. That hand-off must not move existing text.
    shell.write("printf 'X\\tTAB_RIGHT\\n'\r")?;
    let transcript = shell.wait_until(|bytes| {
        let tail = bytes.get(offset..).unwrap_or_default();
        find_bytes(tail, b"X\tTAB_RIGHT").is_some() && settled_prompt_is_visible(bytes)
    })?;

    let tail = &transcript[offset..];
    let streamed_marker = find_bytes(tail, b"X\tTAB_RIGHT").expect("streamed marker");
    let streamed_marker_end = offset + streamed_marker + b"X\tTAB_RIGHT".len();
    let before_renderer_resumes = &transcript[..streamed_marker_end];
    let (streamed_column, streamed_row) =
        output_marker_column(before_renderer_resumes).expect("marker in streamed terminal state");
    let (settled_column, settled_row) =
        output_marker_column(&transcript).expect("marker in settled diff-rendered state");

    assert_eq!(
        streamed_column, 8,
        "the control state should reproduce the terminal's native tab stop: {streamed_row:?}"
    );
    assert_eq!(
        settled_column, streamed_column,
        "shell output moved while control returned to the diff renderer; before={streamed_row:?}, after={settled_row:?}"
    );

    shell.exit()
}

#[test]
fn git_branch_output_has_no_extra_rows_around_streamed_output() -> io::Result<()> {
    const COMMAND: &str = "git branch";

    let (home, repo) = git_branch_fixture()?;
    let mut shell = ApplicationPty::start_with_home_and_cwd(home, &repo)?;
    let fill_offset = shell.transcript_len();
    shell.write(
        "printf 'FILL01\\nFILL02\\nFILL03\\nFILL04\\nFILL05\\nFILL06\\nFILL07\\nFILL08\\n'\r",
    )?;
    shell.wait_until(|bytes| {
        bytes
            .get(fill_offset..)
            .is_some_and(|tail| find_bytes(tail, b"FILL08").is_some())
            && settled_prompt_is_visible(bytes)
    })?;

    let offset = shell.transcript_len();
    shell.write(&format!("{COMMAND}\r"))?;
    let transcript = shell.wait_until(|bytes| {
        let rows = screen_rows(bytes);
        bytes.get(offset..).is_some_and(|tail| {
            find_bytes(tail, b"exps").is_some() && find_bytes(tail, b"master").is_some()
        }) && rows.iter().any(|row| row.trim() == "* exps")
            && rows.iter().any(|row| row.trim() == "master")
            && settled_prompt_is_visible(bytes)
    })?;
    let tail = &transcript[offset..];
    let streamed_output_end = find_bytes(tail, b"master").expect("streamed branch output");
    let renderer_resume = streamed_output_end
        + find_bytes(&tail[streamed_output_end..], b"\x1b[1;1H\x1b[J")
            .expect("renderer recovery frame after streamed branch output");
    let streamed_rows = terminal_history_rows(&transcript[..offset + renderer_resume]);
    let streamed_command_row = streamed_rows
        .iter()
        .position(|row| row.contains(COMMAND))
        .expect("submitted git branch command in streamed frame");
    let streamed_active_branch_row = streamed_rows
        .iter()
        .position(|row| row.trim() == "* exps")
        .expect("active branch in streamed frame");
    let streamed_master_branch_row = streamed_rows
        .iter()
        .position(|row| row.trim() == "master")
        .expect("master branch in streamed frame");

    assert_eq!(
        [streamed_active_branch_row, streamed_master_branch_row],
        [streamed_command_row + 1, streamed_command_row + 2],
        "streamed git output introduced empty physical rows before the renderer resumed:\n{}",
        streamed_rows.join("\n")
    );

    let settled_rows = screen_rows(&transcript);
    let command_row = settled_rows
        .iter()
        .position(|row| row.contains(COMMAND))
        .expect("submitted git branch command");
    let active_branch_row = settled_rows
        .iter()
        .position(|row| row.trim() == "* exps")
        .expect("active branch output");
    let master_branch_row = settled_rows
        .iter()
        .position(|row| row.trim() == "master")
        .expect("master branch output");
    let next_prompt_row = settled_rows
        .iter()
        .position(|row| row == "tester theseus-shell>")
        .expect("next command prompt");

    assert_eq!(
        [active_branch_row, master_branch_row, next_prompt_row,],
        [command_row + 1, command_row + 2, command_row + 3],
        "settled git output contains empty physical rows:\n{}",
        settled_rows.join("\n")
    );

    shell.exit()
}

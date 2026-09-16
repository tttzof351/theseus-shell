use super::*;

#[test]
fn headless_and_piped_commands_emit_answer_once_without_ansi() -> io::Result<()> {
    for args in [vec!["-p", "held question"], vec!["/ask", "held question"]] {
        let (home, held) = held_json_fixture()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(args)
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        held.ready
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?;
        held.release.send(()).map_err(io::Error::other)?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.contains(&0x1b));
        assert!(!output.stderr.contains(&0x1b));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("HELD_ANSWER")
                .count(),
            1
        );
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn headless_sigint_cancels_waiting_http_and_exits_130() -> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "held question"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = wait_cli_exit(child, Duration::from_secs(2))?;
    assert_eq!(output.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(home)?;
    Ok(())
}

#[test]
fn closing_headless_stdout_cancels_tool_producer_without_hanging() -> io::Result<()> {
    let (home, server) = interrupted_agent_fixture()?;
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "run tool"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    let output = wait_cli_exit(child, Duration::from_secs(3))?;
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    fs::remove_dir_all(home)?;
    Ok(())
}

use super::*;

#[test]
fn plain_sse_cli_commits_text_once_and_routes_truncation_to_stderr() -> io::Result<()> {
    for args in [vec!["-p", "plain stream"], vec!["/ask", "plain stream"]] {
        let (home, server) = SseServer::start()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(args)
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        server.request()?;
        server.headers()?;
        server.content("**PLAIN_")?;
        server.delta(json!({"content":"ANSWER**", "reasoning":"PLAIN_REASONING"}))?;
        server.finish("length")?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.ends_with("PLAIN_REASONING\n**PLAIN_ANSWER**\n"),
            "{stdout}"
        );
        assert_eq!(stdout.matches("PLAIN_REASONING").count(), 1);
        assert_eq!(stdout.matches("PLAIN_ANSWER").count(), 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Response truncated"));
        assert!(!output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b));
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_sse_sigint_preserves_prefix_closes_http_and_exits_130() -> io::Result<()> {
    for prefix in [false, true] {
        let (home, server) = SseServer::start()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(["-p", "interrupt stream"])
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        server.request()?;
        if prefix {
            server.headers()?;
            server.content("PLAIN_PREFIX")?;
            let started = Instant::now();
            while !log_events(&home)?
                .iter()
                .any(|event| event["event"] == "llm_first_semantic_delta")
            {
                assert!(started.elapsed() < WAIT_TIMEOUT);
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        server.action(Action::ExpectClosed)?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            if prefix { "PLAIN_PREFIX\n" } else { "" }
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
        assert!(!output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b));
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[test]
fn plain_sse_broken_pipe_cancels_live_tool_output() -> io::Result<()> {
    let (home, server) = SseServer::start()?;
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "tool stream"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    server.request()?;
    server.headers()?;
    server.delta(tool_delta("printf 'PIPE_%s\\n' READY; exec sleep 30"))?;
    server.finish("tool_calls")?;
    let output = wait_cli_exit(child, Duration::from_secs(2))?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    fs::remove_dir_all(home)?;
    Ok(())
}

//! Linux process budgets complement deterministic scheduler/renderer tests.
use super::*;

#[derive(Debug)]
struct Sample {
    ticks: u64,
    switches: u64,
    rss_kib: u64,
}

fn sample(pid: u32) -> io::Result<Sample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect::<Vec<_>>();
    assert_ne!(
        fields[0], "Z",
        "must measure a living application, not a zombie"
    );
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = |key: &str| -> u64 {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    Ok(Sample {
        ticks: fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap(),
        switches: value("voluntary_ctxt_switches:") + value("nonvoluntary_ctxt_switches:"),
        rss_kib: value("VmRSS:"),
    })
}

fn check_idle_budget(app: &mut ApplicationPty, active: bool) -> io::Result<()> {
    assert!(app.child.try_wait()?.is_none());
    let pid = app.child.process_id().expect("application PID");
    // Exclude startup, command completion, and the final publication frame.
    thread::sleep(Duration::from_millis(300));
    let before = sample(pid)?;
    let start = Instant::now();
    thread::sleep(Duration::from_secs(1));
    let after = sample(pid)?;
    let seconds = start.elapsed().as_secs_f64();
    assert!(app.child.try_wait()?.is_none());
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu = (after.ticks - before.ticks) as f64 / hz / seconds;
    let switches = (after.switches - before.switches) as f64 / seconds;
    eprintln!(
        "active={active}: CPU={:.2}%, main-thread switches/s={switches:.1}, RSS={} KiB",
        cpu * 100.,
        after.rss_kib
    );
    // Two percent plus one clock tick tolerates coarse /proc accounting and shared CI hosts.
    assert!(
        cpu <= 0.02 + 1.0 / hz / seconds,
        "idle CPU exceeded budget: {:.2}%",
        cpu * 100.
    );
    let limit = if active { 60. } else { 20. };
    assert!(
        switches <= limit,
        "idle main thread woke {switches:.1} times/s (budget {limit})"
    );
    Ok(())
}

#[test]
fn performance_idle_after_large_shell_output() -> io::Result<()> {
    let mut app = ApplicationPty::start()?;
    app.write("seq 1 8000\r")?;
    app.wait_until(|bytes| {
        screen_rows(bytes).iter().any(|row| row == "8000") && settled_prompt_is_visible(bytes)
    })?;
    check_idle_budget(&mut app, false)?;
    app.exit()
}

#[test]
fn performance_waiting_for_provider_does_not_busy_poll() -> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let mut app =
        ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
    app.write("/ask held question\r")?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    app.wait_until(|bytes| waiting_spinner_row(bytes).is_some())?;
    check_idle_budget(&mut app, true)?;
    app.write("NEXT_DRAFT")?;
    app.wait_until(|bytes| screen_text(bytes).contains("NEXT_DRAFT"))?;
    app.write("\x03")?;
    app.wait_until(|bytes| {
        waiting_spinner_row(bytes).is_none() && screen_text(bytes).contains("NEXT_DRAFT")
    })?;
    app.write(&"\x7f".repeat("NEXT_DRAFT".len()))?;
    drop(held);
    app.exit()
}

#[test]
fn performance_two_mb_shell_output_stays_within_rss_budget() -> io::Result<()> {
    let mut app = ApplicationPty::start()?;
    let pid = app.child.process_id().expect("application PID");
    let baseline = sample(pid)?.rss_kib;
    app.write("python3 -c \"[print('z'*200000) for _ in range(10)]; print('MEMORY_DONE')\"\r")?;
    app.wait_until(|bytes| {
        screen_rows(bytes).iter().any(|row| row == "MEMORY_DONE")
            && settled_prompt_is_visible(bytes)
    })?;
    assert!(app.child.try_wait()?.is_none());
    let rss = sample(pid)?.rss_kib;
    let retained = rss.saturating_sub(baseline);
    eprintln!("2 MB shell output: RSS={rss} KiB, baseline={baseline} KiB, growth={retained} KiB");
    assert!(
        retained < 50 * 1024,
        "2 MB output grew RSS by {retained} KiB (budget 50 MiB)"
    );
    check_idle_budget(&mut app, false)?;
    app.exit()
}

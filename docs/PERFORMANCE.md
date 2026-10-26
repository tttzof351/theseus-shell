# Performance regression checks

These checks cover the idle CPU and retained-output memory problems investigated
in [CPU_USAGE.md](../drafts/CPU_USAGE.md). They run in the ordinary test suite;
no external model, API key, or performance profiler is needed.

## Test plan and budgets

1. **Scheduling (unit):** simulate one second without wall-clock sleeps. An idle
   application requests no frames, while a waiting operation requests the eight
   120 ms spinner updates. Input, resize (represented as dirty state), unfinished
   publication, and background layout must still schedule frames. An expired
   frame deadline must not turn idle waiting into a zero-timeout loop.
2. **Screen cache (unit):** retain 8,000 lines, change only the editor ten times,
   and count transcript comparisons. The budget is zero. A separate functional
   check covers output replacement, style changes, append, and clear.
3. **Input polling (unit, Unix):** use a quiet nonblocking socket as the input
   descriptor and count actual `poll` calls. A 100 ms wait takes at most two
   calls. Incomplete CSI, paste, and UTF-8 sequences also remain cheap; a lone
   Escape retains its short ambiguity deadline.
4. **Renderer memory (integration):** a dedicated test binary meters live heap
   allocations with a forwarding system allocator. Ten 200,000-character lines,
   the virtual screen, indexed layout, and viewport must retain less than
   **32 MiB**, while preserving all 16,000+ wrapped output rows. This binary has
   exactly one test to avoid concurrent allocations contaminating its result.
5. **Actual application (PTY, Linux):** sample a living child through `/proc`,
   after startup/publication has settled. Cover 8,000 output lines, 2 MB of long
   lines, and an HTTP fixture holding the model response. Idle CPU must stay
   below **2% plus one accounting tick** over the one-second window. The main
   thread's context-switch budgets are **20/s idle** and **60/s waiting for a
   provider**. RSS growth from the 2 MB output must stay below **50 MiB** relative
   to that child's startup baseline. The parent continuously drains the PTY.
6. **Functional regression:** compare lazy row materialization against full
   layout for widths 1–17, all cursor offsets, prefixes, tabs, combining marks,
   wide glyphs, and styles. Run the existing responsiveness, cancellation,
   streaming, resize, shell/TUI, scrollback, and xterm.js checks.

CPU and RSS checks intentionally have slack for clock granularity and allocator
variation. They are not microbenchmarks: deterministic work/allocation tests are
what distinguish unnecessary work from a slow or busy host. Context switches are
an observable proxy, not an exact count of syscalls or timer wakeups. RSS includes
allocator arenas and shared mappings, unlike the renderer's live-heap measurement.
The tests bound these workloads, not arbitrarily large histories or every possible
style distribution.

## Running

```sh
# Deterministic budgets and Unix input wait checks
cargo test --locked --lib performance_tests -- --nocapture

# Allocation budget (Linux and macOS)
cargo test --locked --test renderer_memory -- --nocapture

# Process CPU, main-thread context switches, and RSS (Linux only)
cargo test --locked --test application_pty performance_ -- --nocapture --test-threads=1

# Complete checks; includes the new tests
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
npm --prefix tests/xterm test
```

See [GUIDELINES.md](GUIDELINES.md) for xterm.js setup and the separate Vim smoke
check. The existing Linux/macOS CI workflow runs these tests automatically;
`/proc` tests are compiled only on Linux.

## Initial red tests and corrections (2026-10-27)

Tests were added and run before the fixes. Extracting scheduling/cache helpers
and adding test-only counters preserved the original behavior. Six unit budgets,
the renderer allocation test, and both initial process CPU tests failed; the
functional scheduling/cache checks already passed.

Observed locally with the debug test build:

| Measurement | Before | After |
| --- | ---: | ---: |
| Frames during simulated one-second model wait | 30 | 8 |
| Transcript comparisons for ten editor updates | 80,000 | 0 |
| `poll` calls during one quiet 100 ms input wait | 976 | at most 2 |
| Live renderer heap for 2 MB output | 171,084,516 bytes | 12,896,930 bytes |
| Idle CPU after 8,000 shell output lines | 8% | 0% at `/proc` resolution |
| CPU while provider response is held | 20% | 0–1% |
| Main-thread context switches/s: idle / held provider | 115 / 226 | 10 / 32 |

The additional 2 MB application test observed about 45 MiB RSS, approximately
25 MiB above startup, after the fix. These are measurements from this environment,
not guarantees about another machine. The original report measured a different
process/workload and should not be compared as an identical benchmark.

Corrections:

- Render only dirty state, changed animation, geometry, or pending layout/publication.
  Poll backend work at a frame budget and idle geometry at 100 ms; terminal input
  wakes the blocking wait immediately.
- Mark transcript replacement explicitly and reuse the unchanged screen prefix.
  Avoid recalculating document size on editor-only frames.
- Round nonzero `poll` timeouts **up** to milliseconds and stop slicing quiet waits
  into 10 ms intervals. The 25 ms ambiguity timer applies only to a lone Escape.
- Store character styles as coalesced runs and retain wrapped source ranges instead
  of every physical cell in history. Materialize rows for display/publication.
  History is preserved without truncation. Public renderer representation types
  now expose `CellStyles` and `CachedLogicalLayout::row` / `row_count` rather than
  dense style vectors and cached physical rows.

The original report's estimate of 32 empty polls on every loop iteration was not
supported by the code: that input batch stops at the first absent event. Actual
poll instrumentation found a different amplification: rounding the fractional
remainder down to zero repeatedly polled until the deadline expired.

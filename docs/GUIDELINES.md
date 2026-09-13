# Development guidelines

Run commands from the repository root. This guide covers verification, reporting
results, version changes, lock-file maintenance and releases.

## Prerequisites

- Linux or macOS with working native PTYs. These are the platforms covered by CI.
- The Rust toolchain specified by `package.rust-version` in
  [Cargo.toml](../Cargo.toml), with `rustfmt` and `clippy` installed.
- Vim for the real TUI integration scenarios. Check availability with
  `vim --version`; those scenarios are part of the normal PTY suite.
- Node.js 22+ and npm for the additional xterm.js checks.

Install the formatter/linter on the selected Rust toolchain and install the
pinned JavaScript test dependencies:

```sh
rustup component add rustfmt clippy
npm --prefix tests/xterm ci --ignore-scripts --no-audit --no-fund
```

Repeat `npm ci` after changing the JavaScript lock file or starting from a fresh
checkout. Ordinary Rust tests do not need Node. HTTP/SSE integration tests use
local fixture servers and temporary application homes; they need no real LLM
credentials or paid requests.

## Required checks for code changes

Before considering a Rust code change ready, run formatting, Clippy and the Rust
test suite on the final version of the change:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

`cargo fmt --check` only checks formatting. If it fails, run `cargo fmt`, review
the diff, then rerun the check. Clippy warnings are failures: fix their cause;
do not remove `-D warnings` or add a broad `allow` just to make the check pass.

`--locked` prevents verification from silently changing Cargo.lock. If dependency
or package metadata changes require a lock-file update, make that update explicitly
and review it before running the checks again.

For changes affecting rendering, input, history, PTY ownership, streaming output,
Markdown layout or Unicode, also run the xterm.js suite:

```sh
npm --prefix tests/xterm test
```

Before a release, run the full sequence above and the additional Vim smoke test:

```sh
vim --version
cargo test --locked --lib shell::pty::session::tests::ignored_vim_smoke_starts_and_exits -- --ignored --exact
```

Run this smoke test for PTY/TUI changes too. Check that Vim was actually available;
the older smoke test can report a skip when it is missing.

For documentation-only changes, check links, paths, commands and consistency with
the scripts/CI they describe. Rebuilding and rerunning every suite is unnecessary
unless the change also affects executable code or test configuration. If adding
or changing executable Rustdoc examples, run `cargo test --locked --doc` separately:
the `--all-targets` command above does not run doctests.

## Test layers and focused commands

Use focused runs while developing, then perform the required checks above.

| Layer | What it checks | Command |
| --- | --- | --- |
| Rust library tests | Agent/config/transport logic, events, Markdown, layout and renderer behavior; also the managed UI subprocess fixtures | `cargo test --locked --lib` |
| Managed UI PTY fixtures | Actual application loop with a controlled backend; editing, lifecycle, cancellation and rendering | `cargo test --locked --lib application::managed_ui_tests` |
| Application integration tests | Real binary, HTTP/SSE fixtures, shell PTYs, Vim and headless CLI | `cargo test --locked --test application_pty` |
| Streaming integration tests | HTTP/SSE through the application and terminal, including failure/cancellation and tool execution | `cargo test --locked --test application_pty streaming::` |
| Stdout serialization | Concurrent output transactions do not interleave | `cargo test --locked --test stdout_lock` |
| xterm.js replay | Real PTY recordings interpreted by an independent terminal engine | `npm --prefix tests/xterm test` |

To run one specific integration test and see its diagnostic output:

```sh
cargo test --locked --test application_pty streaming::long_bash_output_then_streamed_table_publish_once_without_preview_in_history -- --exact --nocapture
```

Discover current names rather than relying on a fixed test count:

```sh
cargo test --locked --all-targets --all-features -- --list
```

## Ignored tests

The two current ignored entries have different purposes:

- `application::managed_ui_tests::fixture_child` is a subprocess entry point.
  Parent PTY tests launch it with `--ignored --exact` and supply a fixture directory
  and other required state. Its appearance as ignored in the top-level report
  does not mean the managed UI scenarios were skipped. Do not launch it alone.
- `shell::pty::session::tests::ignored_vim_smoke_starts_and_exits` is an additional
  Vim smoke test. Run it explicitly using the command above. Normal integration
  tests also exercise real Vim, including a return to streaming output.

Do not use a blanket `cargo test -- --ignored` as a substitute for the full suite:
it invokes the fixture entry point without the parent that prepares its state.

## xterm.js: execution, artifacts and limits

The npm command first runs the streaming Rust integration scenarios with trace
export enabled. It then replays six selected recordings through the pinned
`@xterm/headless` engine. Each recording runs with both erase behaviors and both
whole/fragmented byte writes, for 24 replay combinations. Resize events and
intermediate checkpoints are part of the recordings.

Assertions cover scenario output appearing once and in order, native scrollback,
cursor position, text attributes, spinner/draft isolation and alternate-screen
behavior. Two negative checks restore the original ED2 and CSI S bugs in a trace
and require the same assertions to reject them.

Artifacts are retained under `target/xterm/run-*`. A failed replay adds
`.failure.json` and `.history.txt` files. The runner prints the absolute directory;
use it to investigate the failure without starting the application again:

```sh
npm --prefix tests/xterm test -- --replay /absolute/path/to/target/xterm/run-XXXXXX
```

Replay is useful for debugging the checker or studying terminal behavior. To
verify a renderer/application fix, run `npm --prefix tests/xterm test` again so it
captures output from the changed binary. Replaying an old recording cannot
validate a new application implementation.

Setting `THESEUS_XTERM_TRACE_DIR` only exports data; it does not run the xterm.js
assertions. See [the xterm.js guide](../tests/xterm/README.md) for the trace format's
use, scenario coverage and known limits, including the startup-banner resize case.

Headless replay checks terminal buffers and cells. It does not check browser font
or GPU rendering, VS Code extensions, terminal replies sent back to a live
application, or other terminal engines. For visual terminal changes, also exercise
the affected flow in the terminal where the issue was reported when available:
long output, editing a draft, resize, interruption, and TUI entry/exit as applicable.
Record which terminal was actually used; do not describe an xterm.js replay as a
manual pass in Terminal.app, iTerm2 or Ghostty.

## Adding regressions and investigating failures

- Put a deterministic logic regression at the smallest useful Rust layer. Add a
  PTY scenario when behavior depends on input, terminal ownership or the output
  lifecycle, and add an xterm.js checkpoint/assertion for terminal-engine behavior.
- Assert observable behavior: missing/duplicate output, ordering, cursor position,
  styles, preserved input and buffer isolation. A raw transcript may contain many
  legitimate redraws; counting its text occurrences alone does not test scrollback.
- Hold fixture HTTP responses or backend events at explicit boundaries and wait
  for observable states. Keep deadlines bounded; avoid long sleeps as synchronization.
- Reproduce a bug with a failing assertion before the fix when possible. For a
  checker change, show that a broken trace or implementation is still rejected.
- Treat latency budgets as requirements. Investigate slow paths and timing
  failures rather than raising a timeout or weakening an assertion to obtain green
  output. A pass after a rerun does not explain the original failure.
- Inspect failures with `--nocapture` and, where useful, `RUST_BACKTRACE=1`; use the
  xterm.js artifacts to distinguish application output from emulator behavior.
- After changing code to address a failure, rerun the affected checks. Once the
  required checks pass on the final code, repeat them only for a new change or an
  unresolved concern.

## CI and reporting results

[The test workflow](../.github/workflows/tests.yml) runs on pull requests, pushes
to `master` and manual dispatch. Linux and macOS jobs run formatting, strict
Clippy, the Rust suite, the extra Vim smoke test and xterm.js. Failed jobs upload
available terminal traces for diagnosis. Workflow configuration is not evidence
that a remote run has succeeded; inspect the actual job results.

When reporting a change, name the checks run and their outcomes. Explain ignored,
skipped, failed or unavailable checks, distinguish automated replay from manual
terminal testing, and mention relevant coverage limits or unresolved defects.
Keep counts tied to a specific run rather than hard-coding an expected total into
this guide.

## Releases

### Where to change the version

The application version is `[package].version` in [Cargo.toml](../Cargo.toml).
For example, a patch release could change `0.2.7` to `0.2.8`; choose the actual
next version for the changes being released. Synchronize [Cargo.lock](../Cargo.lock)
after editing the manifest, and commit both files together.

| Location | Role in a release |
| --- | --- |
| `Cargo.toml`: `[package].version` | Source of the application version; edit this field |
| `Cargo.lock`: `[[package]]` with `name = "theseus"` | Cargo-managed copy of the package version; update through Cargo |
| `src/commands/mod.rs`: `VERSION` | Reads `CARGO_PKG_VERSION` at compile time; no manual version edit |
| `Cargo.toml`: `[package].rust-version` | Rust toolchain requirement, also used by CI; change only when the required toolchain changes |
| `tests/xterm/package.json` and `package-lock.json` | Separate JavaScript test dependencies; an application version bump does not require changing them |

### Synchronize Cargo.lock

After editing `[package].version`, run:

```sh
cargo update --workspace
git diff -- Cargo.toml Cargo.lock
```

For a version-only bump with the required registry data already cached, the
offline equivalent is:

```sh
cargo update --workspace --offline
```

`--workspace` updates workspace packages while preserving existing dependency
versions unless dependency requirements also require changes. For a version-only
bump here, expect the `theseus` entry in Cargo.lock to acquire the new version,
with dependency versions and checksums unchanged. Review unexpected changes.

Do not delete Cargo.lock, run an unrestricted `cargo update`, or regenerate the
entire lock file just to bump the application version: those operations can
resolve newer dependency versions as well. For an intentional dependency update,
scope it to that dependency, for example `cargo update -p crossterm`, and review
and test the resulting dependency changes separately.

Do not add `--locked` to the synchronization command: this step must write the
lock file. Use `--locked` for verification and builds afterwards.

### Verify and publish

Run all [required checks](#required-checks-for-code-changes) on the final release
change, including xterm.js and the additional Vim smoke test. Also verify the
local release build:

```sh
cargo build --locked --release
git diff --check
```

Include Cargo.toml and Cargo.lock in the reviewed release change. Merging or
pushing that version change to `master` starts the
[Release workflow](../.github/workflows/release.yml). It compares the manifest
version before and after the push. Changes to source or the lock file without a
version change do not normally create another release.

The workflow builds Linux/macOS binaries for amd64 and arm64 using the Rust
version from Cargo.toml and `cargo build --locked --release`. After all builds
succeed, it publishes a GitHub Release named `theseus X.Y.Z`, with tag `vX.Y.Z`,
four `.tar.gz` archives and their `.sha256` files. This process distributes
binaries through GitHub Releases; it does not run `cargo publish`.

The workflow creates a missing tag, so a separate tag push is not required for
the normal release path. Its current `gh release create` invocation does not
specify `--target`: a new tag uses the default branch's head at publication time.
Keep that branch stable during publication and verify that the resulting tag
points to the commit used for the release builds.

`Tests` and `Release` are separate workflows. Release currently does not wait for
Tests to pass. Complete the checks before merging/pushing the release change and
inspect the actual workflow results. Configuring both workflows does not make
test success a publication prerequisite.

For a manual run, select **Release → Run workflow** in GitHub Actions on the
intended release ref. Manual dispatch can publish the current version without
another bump when that GitHub Release does not exist. An existing Release for
`vX.Y.Z` makes the workflow skip publication; use a new version for a new release.

After publication, verify the tag/commit, all four archives and checksums, and
smoke-test the extracted binary on an available target platform. The installer
uses the latest GitHub Release by default; `THESEUS_VERSION` can select a specific
released version.

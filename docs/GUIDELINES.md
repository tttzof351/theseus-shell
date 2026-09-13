# Development guidelines

Run commands from the repository root. Requires Linux/macOS, Rust from
[Cargo.toml](../Cargo.toml) (`rust-version`), Vim, and Node.js 22+ for xterm.js.

## Required checks

Run after Rust code changes:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Use `cargo fmt` to fix formatting. Fix warnings and failing tests rather than
weakening checks or timeouts. For documentation-only changes, check links and commands.

## Terminal checks

Run for input/output, rendering, streaming or PTY changes, and before releases:

```sh
# Install once, and again after package-lock.json changes
npm --prefix tests/xterm ci --ignore-scripts --no-audit --no-fund
npm --prefix tests/xterm test

# Additional Vim smoke test
vim --version
cargo test --locked --lib shell::pty::session::tests::ignored_vim_smoke_starts_and_exits -- --ignored --exact
```

The ignored `fixture_child` is launched by parent PTY tests; do not run it alone
or use a blanket `cargo test -- --ignored`.

Shell tests also exercise `bash` from `PATH`, so Bash 5 can be checked alongside
the system Bash 3.2 on macOS.

xterm.js records new PTY output and checks it with the real emulator. Artifacts:
`target/xterm/run-*`. See [the xterm.js guide](../tests/xterm/README.md) for replay,
coverage and known limits. An old recording does not verify a new application fix.

## Focused tests

```sh
cargo test --locked --lib
cargo test --locked --lib application::managed_ui_tests
cargo test --locked --test application_pty
cargo test --locked --test application_pty streaming::
cargo test --locked --all-targets --all-features -- --list
```

Filter by test name and add `-- --nocapture` for diagnostics. Run
`cargo test --locked --doc` when changing executable Rustdoc examples.
Add regression coverage at the relevant layer; report checks run and any failures/skips.

## Releases

1. Bump `[package].version` in [Cargo.toml](../Cargo.toml).
   `rust-version` is the compiler requirement, not the application version.
2. Synchronize the lock file, run all checks above, and build:

```sh
cargo update --workspace  # Add --offline if registry data is cached
git diff -- Cargo.toml Cargo.lock
# Run the required and terminal checks above
cargo build --locked --release
git diff --check
```

For a version-only bump, dependency versions should stay unchanged. Avoid a broad
`cargo update` or deleting Cargo.lock. Commit Cargo.toml and Cargo.lock together.

3. Merge/push the version change to `master` to start the
   [Release workflow](../.github/workflows/release.yml). It publishes `vX.Y.Z` with
   Linux/macOS archives for amd64/arm64 and checksums. Manual launch:
   **GitHub Actions → Release → Run workflow**. Existing releases are skipped.
4. Verify the release tag, build commit, archives and checksums; smoke-test a binary.

Release does not wait for the [Tests workflow](../.github/workflows/tests.yml), so
complete verification before merging/pushing. A missing tag is created from the
default branch's head at publication time; keep it stable and verify the tag's commit.

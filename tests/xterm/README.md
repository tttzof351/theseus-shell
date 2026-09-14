# xterm.js terminal checks

For the overall verification workflow, fmt/Clippy requirements and Rust/PTY test
commands, see [the development guidelines](../../docs/GUIDELINES.md).

On Linux or macOS, from the repository root, with the project's Rust toolchain,
Node.js 22+ and Vim:

```sh
npm --prefix tests/xterm ci --ignore-scripts --no-audit --no-fund
npm --prefix tests/xterm test
```

The command runs the real HTTP/SSE → application → PTY integration scenarios in
`tests/application_pty/streaming.rs`. Selected scenarios export their actual output
bytes, resize boundaries and named checkpoints into a fresh `target/xterm/run-*`
directory. Checkpoints use the exact bytes inspected by the Rust wait condition;
completion checks must also wait for native history publication, since a hidden
spinner alone does not mean the background layout has finished. The runner then
feeds those bytes into **@xterm/headless 6.0.0**. Both
dependencies are pinned in `package-lock.json`; no browser or native Node PTY addon
is required. The HTTP provider is a local fixture and uses no API keys or external
LLM service.

Six traces cover:

- 180 bash output rows followed by a 36-row streaming table;
- a late Markdown reference, a code fence, Cyrillic, CJK and emoji across resize;
- cancellation, provider failure and truncated SSE, followed by another request;
- shell stdin, wrapped output, real Vim in the alternate screen, resize and return
  to another streaming response.

Every trace is replayed four ways: `scrollOnEraseInDisplay` disabled/enabled, each
with whole and fragmented byte writes. The enabled setting matches VS Code's
erase behavior. Unicode 11 is loaded for both modes, matching VS Code's default
width calculation. The fragmented mode splits ANSI sequences and UTF-8 across
writes; resize events are applied at their recorded byte offsets.

Assertions inspect the terminal's actual buffers and cells: output uniqueness and
order, native scrollback, intermediate previews, spinner/draft leakage, editor
cursor position, bold attributes and alternate-screen isolation. They use semantic
expectations, not screenshots or snapshots computed by `vt100`.

Two additional negative checks restore each original bug in the captured long
output (`ED2` repaint and `CSI S` publication). Both mutated replays must fail the
same assertions. Missing traces/checkpoints, Cargo failures and replay failures
make the command exit unsuccessfully.

Traces are retained for diagnosis. A failed replay also writes a `.failure.json`
with screen/cursor/buffer state and a readable `.history.txt`. Replay an existing
recording without rerunning Cargo:

```sh
npm --prefix tests/xterm test -- --replay /absolute/path/to/target/xterm/run-XXXXXX
```

Ordinary `cargo test` keeps its Rust/vt100 checks and does not require Node. Setting
`THESEUS_XTERM_TRACE_DIR` enables trace export, but exporting alone does not run
xterm.js assertions. CI runs both suites on Linux and macOS and uploads recorded
traces on failure.

This tests xterm.js parsing, cells, screen buffers and reflow. It does not test
browser font/GPU rendering, VS Code shell-integration extensions, terminal replies
fed back into a running application, or other terminal emulators. During capture,
the existing Rust fixture still uses vt100 to wait for application states; xterm.js
independently verifies those recorded states afterwards.

The uniqueness assertions cover scenario output, not every line of the startup
help banner. Inspection of the resize recordings also shows repeated startup
banner rows (for example, `— resume agent session` occurs twice in the Markdown
and Vim traces). Native resize scrolling and managed publication need a separate
investigation for that case; these passing checks do not certify banner uniqueness.

Relevant upstream interfaces: [headless terminal API](https://github.com/xtermjs/xterm.js/blob/master/typings/xterm-headless.d.ts)
and [VS Code terminal setup](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/terminal/browser/xterm/xtermTerminal.ts).

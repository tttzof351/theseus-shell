# Streaming implementation and acceptance

Accepted in the working tree on 2026-09-14. Base commit: `c641cf6` (`Up version`),
Theseus 0.2.7, Rust 1.96.0, macOS arm64. Scope: `drafts/PLAN_STREAMING.md`, S1–S5.
Streaming remains opt-in; the release version and Cargo dependencies are unchanged.

## Baseline and final gates

Before S1, the unrestricted baseline passed fmt, Clippy, the complete suite
(404 library + 16 PTY + 1 stdout = **421 passed, 2 ignored**) and the explicit
ignored Vim smoke. The initial sandboxed baseline could not bind fixture ports;
that environment failure was resolved by the unrestricted run.

The final working tree passes:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --locked --offline --all-targets --all-features -- -D warnings` | Passed, no warnings |
| `cargo test --locked --offline --all-features --no-fail-fast` | **450 library + 33 PTY/CLI + 1 stdout = 484 passed, 0 failed, 2 ignored** |
| `cargo test --locked --offline --lib shell::pty::session::tests::ignored_vim_smoke_starts_and_exits -- --exact --ignored --nocapture` | 1 passed |
| `git diff --check` | Passed |

`fixture_child` is an ignored subprocess entry point, invoked by the normal managed
PTY tests with `--ignored`, a PTY and `THESEUS_UI_FIXTURE`; running it alone is not a
valid test. The other ignored test is the Vim smoke run explicitly above. Vim was
available at `/opt/homebrew/bin/vim`. Both the existing interactive Vim test and the
new SSE → Vim → SSE test ran successfully, including editing and saving a file.
No blanket `--include-ignored` run was used.

Final logs on the verification machine:
`/tmp/theseus-streaming-final-tests.log`,
`/tmp/theseus-streaming-final-clippy.log`,
`/tmp/theseus-streaming-final-vim.log`,
`/tmp/theseus-streaming-final-perf.log`,
`/tmp/theseus-streaming-final-layout-shutdown.log`.
The tables here retain results independently of temporary log retention.

## S1–S3 contract audit

| Plan requirement | Implementation and verification |
| --- | --- |
| One bounded sync/async ingress; ordered sequence; cancellation independent of queue | `common/events.rs`: shared `try_lock`/`try_send`, sequence increments only on acceptance, 64 events × 16 KiB, UTF-8 splitting. Four async tests exercise mixed producers, full queue, contention, cancellation, disconnect and deadline; terminal events retain accepted data. |
| One JSON/SSE presentation owner; public String API; local results remain visible | `agent/completion_output.rs`, `loops.rs`, `worker.rs`: `Pending`/`Emitted`, same block ids through finish, no final replay. Presentation tests plus `managed_json_turns_preserve_reasoning_content_and_real_tool_output`, JSON fallback and public API HTTP tests cover both paths. |
| Reasoning before content before tools, including late reasoning and empty placeholders | CompletionOutput reserves an invisible Reasoning block before content; empty reservation finishes without another diagnostic. Unit and real PTY tests verify late reasoning, tool-only output and one final content copy. |
| Activity and usage do not invalidate source/layout | `activity_updates_status_without_invalidating_source_or_layout` asserts unchanged layout key and cache; the real SSE cache test below covers usage and terminal outcomes. Spinner uses the existing UI timer, without heartbeat events or a managed spinner thread. |
| SSE framing across arbitrary byte boundaries | `agent/streaming/decoder.rs` and `tests.rs`: all split positions and byte-by-byte input for UTF-8/BOM/CR/LF/CRLF, comments, multiple data lines/events, empty data, ignored id/retry and incomplete EOF. Valid events are yielded before inspecting a malformed tail in the same read; a live HTTP test checks retained prefix. |
| Choice zero, role/schema validation, DONE and supported terminal reasons | The accumulator selects index zero, rejects duplicate selected choices, conflicting role/reason, missing role/content/reason/DONE and semantic data after finish. Provider errors, native network errors and unsupported terminal reasons fail before commit. Length keeps text with an explicit diagnostic and cannot validate tools. |
| Tool assembly and execution barrier | Indexed interleaved fragments retain late id/type/name/arguments. Validation requires unique ids, complete function fields and JSON object arguments. Unit tests cover incomplete/conflicting/duplicate/truncated calls; PTY side-effect tests prove no execution before DONE and exactly one execution after validation. |
| Reasoning representation, opaque metadata and continuation | First nonempty display representation wins; string wins a same-frame tie. Encrypted entries never become visible. Fixtures preserve late identities/types, text/summary, signature, format and opaque extensions. `sse_and_json_reasoning_survive_snapshot_resume_and_next_request` covers both encodings, old snapshots, real file resume and next-request serialization. |
| Usage independent of choices and finish reason | Repeated snapshots replace rather than sum; empty choices and repeated terminal reason are accepted. Current-request unknown usage remains separate from the last known context guard. HTTP public API, reset/resume and clone-isolation tests check unknown/partial status values. |
| Actual memory limits and cooperative assembly | 19 protocol tests include a real 8 MiB event boundary and overflow before buffer growth, plus a 24 MiB tool argument interleaved with 7 MiB opaque reasoning. Another MiB fails the shared 32 MiB budget without extending retained data. A manually polled future proves large tool/detail assembly yields while incomplete, with semantic retry guard set before cancellation can win. |
| Opt-in configuration and JSON fallback | `stream` absent/false retains JSON; true requires n absent/1. Idle timeout is optional, defaults to 60 s and rejects zero. Unexpected Content-Type fails; JSON fallback uses the same response without resubmission. Config round-trip and CST model/API-key edit tests preserve stream, idle, stream_options and comments. |
| Total deadline, network idle and HTTP closure | `llm/transport.rs` reads bounded chunks/error bodies. Total timeout covers parsing/enqueue; idle covers network waits only. Twelve live HTTP tests include stalled headers/body, continuous heartbeats, full ingress and cancel/disconnect. Socket EOF is observed before terminal events are allowed to drain; runtime shutdown precedes synchronous block cleanup. |
| Typed retry and DONE/cancel race | Attempt errors retain phase, attempt, semantic and provider state. Pre-semantic retry uses identical messages/tools/config; content, opaque reasoning and tools forbid retry after a prefix, including compact. A handshake holds block cleanup after validated DONE, cancels, then proves no assistant trajectory commit or tool side effect. |
| Compact remains private and atomic, with valid trim retries | PTY success/cancel/provider-error tests retain the old context until commit and never display the private summary. The live HTTP trim test removes one oldest history item only on a pre-semantic provider context-window error; semantic and protocol failures cannot trigger trimming/retry. |
| Aggregated telemetry without token/secret payloads | Request id is stable across retries; attempt id is distinct. Transport logs purpose, actual format, phase, elapsed/TTFT, last network/semantic activity, bytes/chunks, finish reason and usage presence. PTY tests check start/first-semantic/finish/failure fields and absence of encrypted data/signatures/authorization. Header logging uses an explicit allowlist, tested with secret custom headers. |

## S4 real HTTP → AgentWorker → terminal coverage

The 17 tests in [application_pty/streaming.rs](../tests/application_pty/streaming.rs)
use temporary HOME directories and a local HTTP fixture with request/write/close
handshakes, deadlines and cleanup. VT assertions inspect formatted cells and
logical screen/history, not raw ANSI occurrence counts. The existing component
and fake-PTY tests remain in the full suite.

| S4 item | Evidence |
| --- | --- |
| 1. Formatted prefix before DONE; animation before headers/between chunks | `first_formatted_delta_precedes_done_and_keeps_short_footer_next_to_draft` withholds DONE until bold marker cells and editable draft are visible. |
| 2. Late links, open fence, growing table, Unicode and once-only publication | `late_reference_code_table_and_unicode_publish_once_across_resize` checks formatting and one logical copy after resize and the next command. |
| 3. Late reasoning, content, tools and next content | `late_reasoning_and_tools_keep_order_and_execute_once_only_after_done`; tool-only phases also covered by the retry test. |
| 4. Tool side effect only after full validation | The preceding test checks the file is absent before DONE and contains one execution afterward. `tool_only_stream_never_executes_cancelled_truncated_or_invalid_calls` checks negative cases. |
| 5. Cancel/error/EOF retain prefix and close HTTP; next operation works | `cancel_provider_error_and_eof_preserve_prefix_draft_and_next_operation`; live HTTP tests additionally check full-queue closure and the DONE/cancel race. |
| 6. Draft/paste/Enter/Up/Down/end/clear | `streaming_paste_clear_and_multiline_history_keep_draft_until_explicit_enter`. Ctrl+L hides the earlier prefix while retaining source in trajectory, as required by the existing clear contract; later deltas remain visible. |
| 7. Multiple shell leases, stdin, wrapping, resize and alternate screen | `sse_shell_leases_stdin_and_real_vim_return_to_streaming_without_replay` performs real Vim editing/saving and verifies no alternate-screen content leaks into primary history. |
| 8. Large source responsiveness and exit | `large_active_sse_markdown_keeps_input_resize_cancel_and_exit_budgets`, measured below. An additional single-long-code-line regression now passes. |
| 9. Preview/finish cache, origins, shared layout and immutable Prepared | `application::output_document::streaming_tests::real_sse_usage_and_outcomes_reuse_source_cache_origins_and_prepared_lines` feeds real HTTP SSE through Agent/EventSink into the document and actual layout worker. It compares cache/origin/layout Arc identity on success/failure/cancel and rejects a ready frame after clear. Existing `resize_discards_ready_layout_and_returns_formatted_current_width` and `clear_and_replace_discard_completed_old_generations` independently verify stale width/replacement keys; managed background-layout tests verify visible publication. |
| 10. Spinner-only across startup, headers, heartbeat, retry and tool-only phases | `retry_headers_heartbeat_and_tool_only_response_show_only_one_spinner`, the first-delta test and existing `operation_without_activity_shows_only_spinner_and_preserves_draft`. Completion/cancel removes the spinner from screen/history; explicit cancellation/error diagnostics remain. |
| 11. Short inline/multiline footer, pending resize, growing/shrinking preview | The first-delta test covers both submission forms. `late_reference_shrinks_footer_after_clear_and_background_resize` keeps the background worker active, resizes and checks the footer moves upward after a late reference shrinks the preview. Existing pending-resize component tests directly check the temporary cached frame. |
| 12. Styled agent bash prompt before/after cancel | `sse_bash_preview_keeps_editor_prompt_style_before_and_after_tool_cancel` compares text, color and bold cell-by-cell with the editor, including default style for `>`. |

The cache test uses Agent directly for access to document/cache internals; the
17 integration tests exercise the real AgentWorker and Application boundary.
Ordinary streaming uses append events; replacement remains the separately tested
renderer contract rather than a new token-by-token replacement path.

## S5 plain/API and regression coverage

- `plain_sse_cli_commits_text_once_and_routes_truncation_to_stderr`: no ANSI or
  duplicates, unchanged reasoning routing, explicit truncation diagnostic.
- `plain_sse_sigint_preserves_prefix_closes_http_and_exits_130`: SIGINT before
  headers and after a prefix, retained text, closed socket and exit code 130.
- `plain_sse_broken_pipe_cancels_live_tool_output`: closed stdout cancels the
  producer. Existing piped command and JSON headless tests also pass.
- Public String APIs, compact, status, resume and configuration evidence is in
  the S1–S3 table. No reqwest feature change was necessary for `Response::chunk()`;
  Cargo.toml and Cargo.lock remain synchronized and unchanged.
- [README](../README.md) documents opt-in, configuration, deadlines and plain
  block buffering. [INPUT_OUTPUT](INPUT_OUTPUT.md) documents producer ownership,
  lifecycle, retry, cache and telemetry rules. All pre-existing shell, renderer,
  cache, cancellation and I/O performance checks remain enabled and pass.

## Measured performance and the code-line fix

Debug build, real SSE, 18 terminal rows, resize 60→90 columns. Measurements run
from the keyboard/resize action to visible VT state, not just enqueue completion.

| Fixture | Input | Resize | Cancel | Exit after cancel |
| --- | ---: | ---: | ---: | ---: |
| 890,009-byte Markdown paragraph | 37.81 ms | 48.85 ms | 37.96 ms | 1.204 s |
| 614,020-byte open Rust fence, 16,000 Unicode lines | 37.73 ms | 37.91 ms | 37.90 ms | 0.853 s |

The original limits remain **250 ms / 2 s**. Layout-worker shutdown measured
**1.95 ms**, with the original **1 s** limit, using
`shutdown_interrupts_active_markdown_and_does_not_start_queued_layout`. It pauses
inside formatting and proves that cancelled/queued work cannot become visible.

Measurements were captured with `--exact --nocapture` for that library test and
`streaming::large_active_sse_markdown_keeps_input_resize_cancel_and_exit_budgets`
in the `application_pty` integration target. These are local debug fixtures,
not a latency promise for arbitrary response sizes or machines.

An exploratory 614 KB single code line initially missed the 5-second preview
bound. Profiling showed termimad justified code before wrapping, retaining the
original enormous padding width on every wrapped row. `constrain_wrapped_code_spacing`
in `application/ansi.rs` caps synthetic padding to the available row width in both
Markdown rendering paths, preserving borrowed source slices/origins. The real SSE
regression `a_single_long_code_line_does_not_stall_streaming_layout` now displays
the tail before DONE and cancels successfully. No performance limits were raised.

The local fixture gates and all twelve readiness criteria in the plan are met.
A smoke request to an external provider is an optional additional compatibility
check; this acceptance uses deterministic local HTTP endpoints and no external
provider request was made. Streaming by default and streaming mutable text directly
to pipes remain outside the first-release scope defined by the plan.

# Input and output architecture

The application owns terminal presentation. Agent, HTTP, MCP and tool workers send
semantic events through `common/events.rs`; they do not receive a terminal writer
in managed mode. Public `Agent::run` callers retain the legacy output adapter.

`AgentWorker` owns one Agent and its trajectory. The application consumes an
ordered event queue, edits an `OutputDocument`, and renders its current revision.
The queue holds at most 64 events, each with at most 16 KiB of payload. Text and
byte appends split at that limit; an oversized replacement is an explicit error.
Cancellation uses an independent handle and cannot wait behind a full queue.
Synchronous and asynchronous producers share the same ingress, sequence and block
allocator. The async path yields while the queue or short ingress lock is busy;
no blocking mutex guard or thread sleep spans an await. Sequence advances only
after successful enqueue. Terminal events follow accepted payloads, even on cancel.

`agent/completion_output.rs` owns presentation for both JSON and SSE responses.
It emits Reasoning and Markdown blocks once per completion, retaining their ids
through validation, failure or cancellation. Content arriving before reasoning
reserves an invisible Reasoning block so later deltas keep reasoning/content/tools
in document order. Empty placeholders finish without an extra outcome diagnostic.
The internal result carries full text and `Pending`/`Emitted` presentation state;
AgentWorker emits only pending results, including local command confirmations.
Public String APIs wrap that result without replaying SSE deltas.
Legacy direct Agent callers keep their previous tool-message presentation.

Reset and saved configuration changes are acknowledged operations too. Their
confirmation is shown only after the worker has applied the configuration and
updated its status snapshot. Input typed during that operation remains a draft
and needs an explicit submission afterwards. Applying an already persisted
configuration is a local commit: Ctrl+C cannot leave the saved file and running
Agent on different settings; the worker finishes applying it before acknowledgement.

Each operation and output block has an identity. A block contains Markdown,
reasoning, tool preview/output or diagnostics. Appending and replacing change the
same block; completion seals it. The document rejects stale, duplicate and late
events. Markdown is rendered from the current full source at an explicit width.
Cached RenderLines are derived data, separate from input history and trajectory.
Their cache key uses the source revision and width, independently of the block's
outcome. Markdown source origins are retained during preview too. Finishing a text
block reuses its formatted lines and adds any failure/cancellation marker as a
separate publication group; finishing ANSI still flushes and invalidates its cache.
Rejected events are logged as `backend_event_rejected` with operation/block ids,
sequence and event kind; their payload is excluded. A `Finished` event only ends
the UI operation after the frontend accepts it.

The managed renderer distinguishes a mutable viewport from a stable prefix. Only
stable output is eligible for native scrollback publication; interim versions are
repainted in place. PageUp/PageDown browse the application's retained output and
End resumes following. Up/Down keep their input-history role. Publication uses
block identities and source-line groups, not the number of rows produced by
termimad at a particular width. A stable group may cross the native/live boundary
in parts. Markdown formatting retains the source position of displayed characters,
including repeated words and reordered table cells; publication remembers which
characters have already reached native history. Resizing does not republish them.
While browsing, a source-group anchor keeps the viewport in place as the backend
appends, and the status/editor stay visible in a separate footer.
The footer follows the visible output directly when it fits on screen. Reserving
space for status/editor does not insert blank rows above them; they reach the bottom
only when output fills the available height. Shrinking a preview moves the footer
back up. Pending-resize frames follow the same rule using the cached output height.
The active status includes the original braille spinner, advancing every 120 ms
from elapsed operation time. Managed UI frames draw it without a spinner thread or
direct terminal writes; animation frames never become document/history entries.
While waiting for an LLM response, only the spinner is visible: the waiting label,
elapsed time and attempt counter are omitted from that status row.
This includes startup before the first activity, headers, heartbeats, tool-only
streams and retries. Activity updates do not change the source/layout version.

The loop drains ready input before a bounded backend batch (normally at most
32 KiB), and coalesces rendering at a 33 ms frame interval. Frames use a buffered
terminal writer. Native publication is also batched: at most 128 physical rows
per frame, including a single large source group. Publication still pending when
the application exits or hands the terminal to shell is completed first.
This bounds terminal publication; Markdown computation has its own scheduling.
At 128 KiB of document source, the managed application starts a layout worker and
continues using it for that session. Smaller documents use the synchronous path.
The worker prepares Markdown, VirtualScreen and IndexedPhysicalLayout from a
snapshot without a terminal writer. It retains one pending request and one ready
result, replacing superseded entries. The UI applies the document representation,
publication metadata and physical layout together.
The worker also retains the previous physical layout. Immutable logical-line
layouts are shared through Arc with prepared UI frames; changed lines are replaced
and resized documents reflow. Status changes and new trailing output reuse the
unchanged prefix without copying its physical cells or mutating older frames.

Results carry document version, display generation and width. Clear/replace and
width changes reject old results. An earlier append-only snapshot at the same
width/generation may supply a temporary preview while the current one is pending;
it does not gain the current document's completion/publication status. The status
and editor keep updating independently. Cancellation/failure gets immediate footer
feedback until the document layout includes that outcome. Shell hand-off and exit
finish pending layout and native publication before transferring/dropping ownership.
Those final frames omit the editor/footer: a submitted command such as `/exit`
already belongs to the document and must not remain as a second editable copy.
Dropping the layout worker requests cancellation before joining. Checkpoints run
between preprocessing/parsing phases, Markdown rows, document publication groups
and physical logical-line layouts. A cancelled job produces no ready frame and
the queued next job is not started. A single preprocessing or termimad parsing
call is still indivisible; the checkpoints do not promise a hard bound inside it.
Snapshot copying, old-layout disposal, resize fallback and worker joining still
need to be distinguished from queue size when assessing performance. While a
different-width layout is pending, ManagedRenderer reuses cached document rows
and crops/pads only the visible cells. It separately lays out the current status
and editor at the actual width. Wide glyphs are either kept whole or omitted at
the edge. Temporary frames never publish native history or advance the partial
publication cursor; the final layout resumes normal publication. PageUp/PageDown
can browse the cached document during that wait, preserving the source anchor.
The temporary clipping changes presentation only, not retained document text.

## Terminal ownership

`application/terminal.rs` owns raw mode, bracketed paste and the managed/shell
lease. On Unix, `common/terminal_input.rs` owns a single `/dev/tty` descriptor and
its byte buffer. The managed keyboard decoder consumes only one event. A shell
lease passes all remaining bytes verbatim to the PTY forwarder, which joins before
managed input resumes. Raw mode stays enabled across that boundary.
The input forwarder stops and joins as soon as the command sentinel is recognized,
before the final output and post-sentinel drain. Its RAII guard also joins on
early return or unwind. A buffer read that was waiting for the shell writer
rechecks stop after acquiring it and returns unforwarded bytes to managed input.

Managed rendering and shell passthrough receive separate writer handles checked
against the lease state. `terminal_motion.rs` tracks the cursor, wrapping,
scrolling regions and primary/alternate screen state for the bytes actually
written, including the hand-off and prompt separators. Its incremental `vte`
parser retains no transcript. During a shell lease, each physical row also carries
the last source-byte offset written there. Scrolling a main-screen row advances
that source watermark. The application maps it through primary-screen filtering
and ANSI decoding to a block/group and character offset; publication therefore
does not assume one wrapping width for the whole command capture.

If raw scrolling cuts a shell source group, only its unscrolled character suffix
is published at the current width. If only rows from the preceding managed frame
have scrolled, the renderer uses that frame's cached publication/layout to commit
the remaining group rows before reflowing the document. `PtyOutput` reports resize
events through the terminal owner even between output chunks. The returning UI
queries the final geometry rather than reusing the dimensions from lease entry.

The primary-screen bytes before and after a TUI remain part of the output document;
alternate-screen content is excluded. Returning the lease restores the main screen
if necessary, default scroll margins, cursor origin and wrapping, even when the
external program left those modes changed. The complete original shell capture
remains available for command context/logging.

This replaces crossterm's Unix input reader in the managed loop: its private
read-ahead buffer could swallow stdin sent in the same write as a command's Enter.
Crossterm still supplies key types, terminal commands, raw-mode support and size
queries. The non-Unix event path retains crossterm; byte-transparent shell hand-off
has been tested on Unix.
The decoder preserves CSI-u shifted alternatives, press/repeat/release, extended
modifiers, keypad and lock state. Unsupported protocol functional keys are ignored
instead of inserting private-use characters. An interrupted CSI/SS3 or invalid
UTF-8 prefix cannot consume the following Ctrl+C. Bracketed paste content remains
opaque: embedded control bytes belong to the paste, not keyboard commands.
An input-buffer test verifies that returning an unforwarded partial paste opener
from a shell lease restores its position before the queued editor input.

While an agent operation runs, the editor holds a draft of the next input. Enter
and pasted trailing newlines do not submit it. Completion does not execute it;
the user explicitly submits after the current operation ends. Ctrl+C cancels the
active operation and keeps that draft. Ctrl+L clears visible output without
cancelling work or clearing the draft. A character-level edit mapping rebases the
clear boundary across replacements so inserting before hidden source cannot
resurrect that source.

## Cancellation and tool output

The model picker uses an `Operation::ModelCatalog` task in the agent worker. Its catalog HTTP
request is a scoped async future with cancellation; it retains fresh/stale-cache
and static-fallback behavior for ordinary failures. Cancellation returns an
interruption instead of opening a fallback picker. While loading, the editor
accepts a draft; the picker temporarily retains that editor and restores it on
selection or cancellation. Catalog data is delivered separately from transcript
events and only applied after a successful operation completion.

Session log names are reserved with `create_new`. Multiple reset/compact/model
changes in one second receive numeric suffixes instead of sharing the same
trajectory path. An existing trajectory is protected even if its log was removed.
Resume orders same-second suffixes numerically and continues to read legacy names.
Resume parses JSON through a reader with 8 KiB refills and checks cancellation
between them, including within a large message string. It validates the complete
snapshot and checks cancellation again before replacing the agent context. An
invalid or cancelled load does not write trajectory state. This keeps parsing in
the worker and avoids a second full source-string allocation. It does not provide
a deadline for an individual filesystem read on a stalled filesystem.

If the event channel closes without an accepted `Finished`, the UI seals open
blocks with an error, cancels the producer and logs `backend_output_disconnected`.
Editing remains available while it waits for executor cleanup. A subsequent
completion cannot change this protocol failure into success. Plain mode flushes
the retained text and unfinished tool lines once on this path, including incomplete
UTF-8, and rejects subsequent events for the closed operation.

JSON and SSE HTTP requests run as scoped async futures inside the worker.
Cancellation drops the request/response and shuts down its Tokio runtime before
terminal events can wait for output queue capacity. This also closes hyper's
connection tasks when a consumer is stalled. There is no detached HTTP thread.
Retry waits and compaction share the operation's cancellation handle.

`agent/streaming/` decodes SSE framing and accumulates choice zero independently
of the UI. It preserves UTF-8 across reads, BOM, CR/LF/CRLF, comments and multiline
data; incomplete EOF events are discarded. A valid finish reason and `[DONE]`
are required before trajectory commit or tool execution. Usage can arrive after
finish_reason, and repeated usage snapshots replace previous values. Tool deltas
assemble by index, with late identities and validated JSON object arguments.
Reasoning text and opaque `reasoning_details` remain in trajectory and subsequent
requests; encrypted details never become display text. Only the first nonempty
reasoning representation is displayed, preferring string reasoning within a frame.

Streaming is enabled by `llm_request_settings.body.stream: true`. Content-Type
selects SSE or a JSON fallback for that response; unexpected formats fail without
resubmitting. Newly generated configurations explicitly set `stream: true` and
`stream_idle_timeout_seconds: 60`; existing configurations with `stream` absent
or `false` retain JSON. Optional
`stream_idle_timeout_seconds` (60 seconds by default) applies to network waits,
including headers. Streaming requests have no overall deadline: incoming chunks
or heartbeat comments can keep them alive beyond `request_timeout_seconds`.
This also applies to a JSON fallback for a request made with `stream: true`.
Output queue backpressure does not count toward network idle; cancel/disconnect
still releases the producer and closes HTTP before terminal-event cleanup.
JSON-mode requests retain the total `request_timeout_seconds` deadline across
reading, parsing and enqueueing. The shared reqwest client has a connect timeout
only, so it cannot impose a hidden total deadline on streaming requests.
Request telemetry records the applied total timeout as null for streaming, with
`stream_idle_timeout_seconds` recorded separately.
Heartbeats create no mandatory UI events. Limits are 8 MiB per unfinished SSE event,
32 MiB of aggregated fields, and a bounded HTTP error body. Decoding, accumulator
assembly and enqueueing yield between bounded portions so cancellation/deadlines
can make progress; semantic state is recorded before the first assembly yield.

Typed attempt errors carry phase, attempt, retryability and semantic-start state.
Only retryable failures before any content, reasoning, opaque detail or tool delta
can retry. All attempts reuse the same request snapshot and request id, with distinct
attempt ids. Stream milestones log aggregated timings, counters and usage presence,
without token payloads or encrypted reasoning. Response headers use an explicit
non-secret allowlist. `/compact` uses the same transport with progress-only
presentation; its summary commits internally after validation. Context trimming
can retry only a provider context-window error before semantic data arrives.
Current-request usage resets before request preparation and is updated only after
a validated completion. It is separate from the latest known prompt-token estimate
in trajectory, which still protects the context limit. Status displays absent
values as `n/a` and incomplete cumulative values as `partial`. Resume/reset and
Agent clones maintain independent request-usage state.

MCP startup, discovery and calls select cancellation alongside the network future.
Cancelled sessions join before returning; close has a deadline. Stdio child
ownership is explicit, so runtime shutdown cannot discard a scheduled child
cleanup task before reaping the process.

Agent `bash` starts a separate Unix process group. Cancellation kills that group,
including descendants holding stdout/stderr open after the shell exits, and joins
both readers. A command scope performs the same cleanup on an early error or
unwinding path. A caught worker/reader panic becomes an operation error; its panic
hook does not independently write to the managed terminal. Other threads retain
the application's original panic hook.
The two readers serialize spool writes and event delivery in the
observed arrival order. They cannot reconstruct an order the OS does not provide
across independent pipes.

The complete output goes to its existing tool log. The UI preview is limited to
256 KiB per bash command, with an explicit notice containing the log path. The
model's configured head/tail preview is read from that log with bounded memory.
ANSI parsing keeps incomplete UTF-8, escape sequences and style separately for
stdout and stderr; decoded characters and line controls update their merged
display. Unsupported terminal controls cannot manipulate the physical screen.
Backend activity and failure labels are decoded into plain single-line text;
escape sequences and line controls cannot switch screens or overwrite the editor.

## Non-TTY presentation

`PlainFrontend` consumes the same events in document order. Completed tool lines
are written as they arrive once preceding blocks have finished; later blocks wait
behind an unfinished earlier block. An unfinished tool line is flushed when its
block ends. Replaceable text
commits once, without ANSI styling. Diagnostics use stderr. There is no final
reprint of text already emitted by block completion. SIGINT works in `-p` mode;
writer errors propagate and dropping the consumer cancels producers before join.
Completion diagnostics also pass through ANSI decoding before reaching stderr.

## Current verification and remaining acceptance

Automated coverage includes cancellation before HTTP headers and during a body,
MCP cancellation in three phases with child reaping, cancellation of a shell
descendant holding a pipe, bounded tool preview with a complete spool, and
independent ANSI state for interleaved stdout/stderr.

PTY checks cover immediate stdin at shell hand-off, preserving a busy draft across
completion/cancellation, suppressing busy paste/Enter submission, and Ctrl+L
during a held request. CLI checks cover output uniqueness, absence of ANSI,
headless SIGINT and a closed stdout. Existing shell, history, pager and clear
regressions remain part of the suite.

A test-only subprocess runs the actual application loop with an injected backend
executor. It waits for each test command before appending/replacing/finishing a
block. PTY tests verify formatted Markdown before completion, long-preview
shrink/finish without draft copies, source-group publication across resize,
replacement after Ctrl+L, anchored browsing during appends, failure/panic with
partial output, and cooked-terminal restoration after exit. No fixture entry
point or environment switch is compiled into the production application.

New PTY checks cover repeated shell leases with resize between commands, a native
boundary through a wrapped shell line, preservation of main-screen output around
a TUI, and recovery when a TUI leaves terminal modes changed. A component test
checks bounded publication of 600 stable rows across multiple frames. The busy
Markdown test measures cancellation against a 250 ms deadline (106 ms in a focused
local run after publication batching). PTY handshakes also verify output before
and after a resize during shell execution, shrinking both width and height after
completed rows entered native history, and a quiet shell over previously rendered
Markdown. Component checks cover source offsets through resize/alternate/clear
and the old frame's group boundary at the hand-off.

Large-group checks now cover a single wrapped line split across 128-row frames,
a 3,000-word Markdown paragraph, a 1,000-word table row, and shell hand-off after
partial publication and reflow. PTY tests enforce a 250 ms response budget for
resize/editing those completed blocks, and resize/editing/cancellation of an open
890,009-byte paragraph and a 614,020-byte code fence at 60→90 columns, 18 rows.
After removing synchronous resize fallback, these scenarios measured 35–36 ms
for resize/editing and 37–42 ms for cancellation in a focused local debug run. These fixture
sizes do not establish a time bound for arbitrarily large source documents.
Worker tests hold ready results across resize, clear and replace, and check that
an older open snapshot stays ineligible for publication until the finished one
arrives. A PTY scenario clears a large preview during resize, replaces its source,
finishes new Markdown, and runs two shell commands: hidden output stays absent and
the final answer and command outputs each remain present once.
A renderer component test holds the prepared document cache while resizing
30→12→80→20 columns during partial publication. It checks cache equality, unchanged
native history and publication cursor, an independently wrapped editor, and
browsing. After installing the new layout, all 1,200 markers occur exactly once.
A separate cell test covers wide-glyph clipping and style preservation. A growing
nested-list test checks indentation, bold formatting and unchanged source at
40/20/60 columns before and after completion.
Additional tests cover control sequences in errors while preserving the primary
screen/draft and ordered plain output with overlapping block lifetimes.
PTY checks now also cover shifted CSI-u input and release suppression while busy,
cancellation after an incomplete CSI, payload-free logging of a late block event,
and output-channel loss with delayed or missing cleanup completion. The channel
fixture holds acknowledgement until error rendering, cancellation and draft
editing are observed, then verifies a subsequent shell command.
Additional real-PTY transport tests check the post-sentinel input boundary on sh,
bash and zsh: a paste injected at the final prompt separator remains available
to the managed reader. An unwind test verifies forwarder shutdown and joining.
The model-catalog tests hold both the UI operation and a local HTTP body: resize,
draft editing/cancellation meet the 250 ms budget, the selected model is saved,
the draft returns after selection, and network cancellation closes the response.
Command-lifecycle PTY checks resume a real snapshot, hold reset acknowledgement,
send `/reset` and `/status` in one input packet, and verify that status changes only
after acknowledgement and explicit draft submission. A local HTTP fixture covers
compact cancellation, invalid JSON and success: the editor stays usable, old
trajectory files are unchanged, success creates one new summary trajectory, and
the summary is not printed as a user-facing answer. Concurrent same-timestamp
logger allocation and legacy/suffixed resume ordering have separate tests.
Resume reader tests cancel inside a 2.1 MB JSON string and at the final read,
verify unchanged context, and reject empty snapshots and trailing invalid JSON.
Mixed-output PTY tests cover reasoning → Markdown → tool stdout/stderr → Markdown
on success, failure and cancellation, including split ANSI, CR progress overwrite,
resize, a preserved draft and a subsequent shell command. A separate local JSON
server test executes real bash between two model responses and verifies block
types/order, final-response reasoning and tool context in the next request.

A checkpoint handshake stops real formatting of an 890 KB paragraph after parsing:
worker join takes 2.4 ms in a focused debug run (test budget 1 s), and no queued
layout starts. Exiting immediately after cancellation of that paragraph completes
publication in 1.36–1.38 s after cache reuse fixes (test budget unchanged at 2 s),
retains its tail and outcome once,
prints `/exit` once and restores ICANON/ECHO/ISIG.
Reference links are resolved from the full Markdown source before termimad
formatting. A late definition updates the existing open block; rendering does not
change its source. Clear retains definition context for new visible text without
restoring hidden output. Unit and PTY checks cover late definitions, resize and
finish/error/cancel, with exactly one published link and no draft copies.

Additional tests exercise real MCP initialization/discovery with responsive input,
resize and cancellation, retry-backoff cancellation, plain-event validation and
an interactive Vim round trip with a saved file and terminal resize.

The final suite passes: 403 unit tests, 16 integration PTY/CLI tests and one stdout
test (420 passed, two explicitly ignored tests). The ignored cases are a managed
fixture subprocess entry point and an older Vim version smoke test; the new
interactive Vim test ran successfully on the acceptance machine.
Regression tests check preview cache reuse across success/error/cancel, separate
outcome publication, ANSI finalization, shared physical layouts and unchanged older
frames. Clippy also passes for all targets/features with `-D warnings`.
Footer-position tests cover submitting inline/multiline requests through real PTY,
growing/shrinking previews and pending resize without a gap above status/editor.

These are historical I/O-refactoring results. Current streaming implementation,
HTTP/PTY evidence, performance measurements and completed acceptance audit are
recorded in [STREAMING_ACCEPTANCE.md](STREAMING_ACCEPTANCE.md).

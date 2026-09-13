# Theseus-shell

![Theseus Logo](assets/theseus-logo-l5.png)

**Theseus-shell** is a rust shell wrapper with an embedded LLM agent.

It runs regular shell commands through a PTY, keeps command input/output history,
and can switch from shell mode into agent-assisted workflows.

If you are wondering why another agent should exist, the short motivation
is described in [docs/MOTIVATION.md](docs/MOTIVATION.md).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/tttzof351/theseus-shell/master/install.sh | bash
```

## Build from source

The project ships a single application binary:

- `theseus` — the diff-rendered shell wrapper (entry point: `src/main.rs`).

The virtual-screen layout and physical terminal diff engine are part of the
application as the `src/terminal_renderer` module; they are not a separate
binary target. `Cargo.toml` declares only `theseus` under `[[bin]]`.

Useful cargo invocations:

```sh
# Build the complete workspace
cargo build --workspace

# Build only the production shell
cargo build --bin theseus

# Run the production shell in the foreground
cargo run --bin theseus

# Build a release version (optimized, stripped of debug info)
cargo build --workspace --release
# → target/release/theseus
```

## Shell usage

Regular input is executed as a shell command. Natural-language input is routed to
the agent when it does not look like a shell command. Use `/ask ...` to
explicitly talk to the agent.

During an agent request, Theseus keeps the editor responsive. You can prepare the
next input; Enter and pasted newlines do not submit it until the current operation
finishes. Ctrl+C cancels the operation while keeping that draft. Ctrl+L clears the
visible output without cancelling the request. PageUp/PageDown browse output and
End returns to the latest output; Up/Down still navigate input history.

Loading the model list in `/config` also keeps the editor responsive. Ctrl+C
cancels loading; a draft typed while waiting is preserved after cancellation or
after leaving the model picker.

Large Markdown documents are formatted in a background worker while the editor
and operation status continue updating. Cancelling `/resume` while it loads a
trajectory preserves the previous agent context.

Completed output enters terminal scrollback once; mutable previews stay in the
application's viewport. Shell commands retain ordinary PTY interaction, and their
main-screen output before and after a pager/TUI remains in the application history.

Agent bash output is saved in full to its tool log. The on-screen preview is
limited to 256 KiB per command and shows the log path when that limit is reached.
The input/output architecture is described in
[docs/INPUT_OUTPUT.md](docs/INPUT_OUTPUT.md), with verification evidence in the
[streaming acceptance report](docs/STREAMING_ACCEPTANCE.md).

### Streaming responses

New configurations enable streaming for Chat Completions endpoints by default
with `llm_request_settings.body.stream: true` and explicitly set
`llm_request_settings.stream_idle_timeout_seconds: 60`. To enable it in an existing
`~/.theseus/config.jsonc`, merge these settings while preserving the model, headers
and other fields, then restart Theseus:

```jsonc
"llm_request_settings": {
  "stream_idle_timeout_seconds": 60,
  "body": {
    "stream": true
  }
}
```

With `stream` absent or `false`, requests retain the JSON response path. Streaming
supports one choice (`n` absent or `1`); explicit `stream_options` are passed through.
An endpoint returning JSON to a streaming request is handled without a second request.

In the terminal, reasoning and formatted Markdown appear as they arrive, above the
existing spinner and editor. Tools execute only after `[DONE]` and validation of
the complete assistant message. Cancelled or interrupted streams retain their
visible prefix; they are not retried after semantic data has arrived.

For `stream: true`, there is no total request-duration limit.
`stream_idle_timeout_seconds` limits each wait for headers or more network data
and defaults to 60 seconds; it must be positive. Incoming chunks, including
keep-alive comments, reset that wait. The same policy applies if the endpoint
returns JSON to a streaming request. Waiting for output queue capacity does not
count as provider inactivity; cancellation remains available.
For JSON mode (`stream` absent or `false`), `request_timeout_seconds` still limits
the whole HTTP attempt, including output queue waits. Responses remain limited
to 8 MiB per unfinished SSE event and 32 MiB of accumulated message fields.

For pipes and headless mode (`theseus -p ...`), replaceable text is buffered until
its block finishes, then printed once without ANSI controls. Tool output lines
remain live. `/compact` uses streaming internally without displaying its summary.
Public `Agent::run` and `run_with_context` continue returning the complete String.
`/status` shows unavailable usage as `n/a` and marks incomplete totals as `partial`.
The last known context-token estimate is listed separately and still guards the
context limit when the current request has no usage.

To start Theseus automatically from `~/.zshrc`, guard it with
`THESEUS_ACTIVE` so commands executed by Theseus can still load your aliases
from `~/.zshrc` without recursively starting another wrapper:

```sh
if [[ -z "${THESEUS_ACTIVE:-}" ]]; then
  theseus
fi
```

Natural-language shell workflow:

![Natural language shell workflow](assets/largest_files.gif)

Multiline editor for shell commands:

![Multiline shell mode](assets/syntax_shell_mode.gif)

Agent-assisted repository inspection:

![Agent repo inspection](assets/agent_repo_inspection.gif)

## MCP servers

Theseus reads MCP server configuration from `~/.theseus/config.jsonc`.
Add servers under the top-level `mcp_servers` object. Each server id becomes
part of the public tool name exposed to the agent.

For example, [Tavily](https://www.tavily.com/) (around 1,000 free requests per month) can be added as a remote MCP server for web search, together with a local `pdf-mcp` server:

```jsonc
{
  ...
  "mcp_servers": {
    "tavily-remote-mcp": {
      "type": "http",
      "url": "https://mcp.tavily.com/mcp/?tavilyApiKey=<TAVILY_API_KEY>"
    },
    "pdf-mcp": {
      "command": "uvx",
      "args": ["pdf-mcp@1.14.0"],
      "env": {
        "PDF_MCP_CACHE_DIR": "~/.cache/pdf-mcp",
        "PDF_MCP_CACHE_TTL": "24"
      }
    }
  }
}
```

After updating the config, restart Theseus and run `/mcp` to check server status
and see the public tool names available to the agent.

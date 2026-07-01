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

The project ships two binaries that share the same `theseus` library crate
(exposed by `src/lib.rs`):

- `theseus` — the production shell wrapper (entry point: `src/main.rs`).
- `playground` — a scratchpad binary for experimenting with the internal
  modules without touching the production code (entry point: `src/bin/playground.rs`).

Both targets are declared explicitly in `Cargo.toml` under `[[bin]]` and
end up in `target/<profile>/` after a build.

Useful cargo invocations:

```sh
# Build both binaries
cargo build --bins

# Build only the production shell
cargo build --bin theseus

# Build only the playground
cargo build --bin playground

# Run the playground (smoke-tests that the library is linked correctly)
cargo run --bin playground

# Run the production shell in the foreground
cargo run --bin theseus

# Build a release version of both binaries (optimized, stripped of debug info)
cargo build --bins --release
# → target/release/theseus
# → target/release/playground
```

The `playground` binary can `use theseus::...` for any public module
(`agent`, `shell`, `input`, `common`, `commands`, `logging`) and is meant
for one-off experiments, debug probes, and quick local checks during
development.

## Shell usage

Regular input is executed as a shell command. Natural-language input is routed to
the agent when it does not look like a shell command. Use `/ask ...` to
explicitly talk to the agent.

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

## Motivation

### Shell as a natural interface for an agent

Most popular coding agents, such as Claude Code, Codex, and OpenCode, use a TUI interface. I think there is already a well-suited environment for text-based interaction with agents: the shell.

The shell removes the split between the terminal where the agent is running and the terminal where the user runs build, test, and debugging commands. The whole workflow stays in one context: user commands, their output, and the agent's replies are next to each other.

This also gives the agent a simple and natural way to take previous commands and their output into account.

### A simple agent loop

Modern agents often use a more complex execution loop: subagents, planning mode, automatic summarization, multi-step decision making, and other mechanisms. This can be useful in some scenarios, but in others it can make the agent's behavior less predictable.

I prefer a simpler agent loop because it improves determinism and makes the agent's behavior easier to understand and control.

A good explanation of the agent loop structure is available in this article:
https://www.mihaileric.com/The-Emperor-Has-No-Clothes/

Need a planning mode? You can simply ask the agent to write a `PLAN.md` for the current task, then discuss and edit that plan together. This approach is simpler, more transparent, and does not require a separate mode inside the system.

### No built-in permissions system

The project does not include a separate permissions system (at least until I find a convenient way to do it). Responsibility for executing commands belongs to the user and to the environment where the agent is running.

If the agent is running with access to production infrastructure, secrets, or a filesystem without backups, that is a problem with the execution environment, not with the agent itself.

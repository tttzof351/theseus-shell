#!/usr/bin/env python3
import argparse
import html
import json
import re
import shlex
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_READ_FILE_START_LINE = 1
DEFAULT_READ_FILE_END_LINE = 200


@dataclass
class ToolStats:
    calls: int = 0
    outputs: int = 0
    errors: int = 0
    output_chars: int = 0
    output_bytes: int = 0
    min_output_chars: int | None = None
    max_output_chars: int = 0

    def add_output(self, chars: int, bytes_len: int, is_error: bool) -> None:
        self.outputs += 1
        self.output_chars += chars
        self.output_bytes += bytes_len
        self.max_output_chars = max(self.max_output_chars, chars)
        self.min_output_chars = chars if self.min_output_chars is None else min(self.min_output_chars, chars)
        if is_error:
            self.errors += 1


@dataclass
class ReadFileArgStats:
    calls: int = 0
    default_start_line: int = 0
    default_end_line: int = 0
    default_line_numbers: int = 0


@dataclass
class BashKeyCommandStats:
    calls: int = 0
    outputs: int = 0
    errors: int = 0
    output_chars: int = 0
    output_bytes: int = 0

    def add_output(self, chars: int, bytes_len: int, is_error: bool) -> None:
        self.outputs += 1
        self.output_chars += chars
        self.output_bytes += bytes_len
        if is_error:
            self.errors += 1


@dataclass
class SedRangeStats:
    calls: int = 0
    outputs: int = 0
    errors: int = 0
    output_chars: int = 0
    output_bytes: int = 0

    def add_output(self, chars: int, bytes_len: int, is_error: bool) -> None:
        self.outputs += 1
        self.output_chars += chars
        self.output_bytes += bytes_len
        if is_error:
            self.errors += 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate an HTML report with Theseus trajectory statistics.",
    )
    parser.add_argument("trajectory", type=Path, help="Path to *_trajectory.json")
    return parser.parse_args()


def load_messages(path: Path) -> list[dict[str, Any]]:
    with path.open("r", encoding="utf-8") as file:
        data = json.load(file)

    messages = data.get("messages") if isinstance(data, dict) else data
    if not isinstance(messages, list):
        raise SystemExit("trajectory must be a JSON object with `messages` array or a JSON array")

    normalized = []
    for index, message in enumerate(messages):
        if not isinstance(message, dict):
            raise SystemExit(f"message #{index + 1} is not a JSON object")
        normalized.append(message)
    return normalized


def output_path_for(trajectory_path: Path) -> Path:
    name = trajectory_path.name
    if name.endswith("_trajectory.json"):
        stem = name[: -len("_trajectory.json")]
    else:
        stem = trajectory_path.stem
    return Path.cwd() / "report" / f"{stem}_report.html"


def content_text(content: Any) -> str:
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict):
                if part.get("type") == "text":
                    parts.append(str(part.get("text") or ""))
                elif part.get("type") == "image_url":
                    url = ((part.get("image_url") or {}).get("url") or "")
                    parts.append(f"[image_url {len(url)} chars]")
                else:
                    parts.append(json.dumps(part, ensure_ascii=False))
            else:
                parts.append(str(part))
        return "".join(parts)
    return json.dumps(content, ensure_ascii=False)


def content_stats(content: Any) -> tuple[int, int, int, int, int]:
    text_chars = 0
    text_bytes = 0
    image_parts = 0
    image_url_chars = 0
    image_url_bytes = 0
    total_parts = 0

    if isinstance(content, list):
        total_parts = len(content)
        for part in content:
            if not isinstance(part, dict):
                text = str(part)
                text_chars += len(text)
                text_bytes += len(text.encode("utf-8"))
                continue
            if part.get("type") == "text":
                text = str(part.get("text") or "")
                text_chars += len(text)
                text_bytes += len(text.encode("utf-8"))
            elif part.get("type") == "image_url":
                url = str(((part.get("image_url") or {}).get("url") or ""))
                image_parts += 1
                image_url_chars += len(url)
                image_url_bytes += len(url.encode("utf-8"))
            else:
                text = json.dumps(part, ensure_ascii=False)
                text_chars += len(text)
                text_bytes += len(text.encode("utf-8"))
    else:
        text = content_text(content)
        text_chars = len(text)
        text_bytes = len(text.encode("utf-8"))

    return text_chars + image_url_chars, text_bytes + image_url_bytes, image_parts, image_url_chars, total_parts


def is_tool_error(output: str) -> bool:
    lowered = output.lower()
    if re.search(r"^status:\s*[1-9]\d*", output):
        return True
    markers = [
        "tool `",
        " failed:",
        "mcp tool returned an error",
        "error:",
        "traceback (most recent call last)",
        "no such file or directory",
        "permission denied",
    ]
    return any(marker in lowered for marker in markers)


def parse_tool_arguments(tool_call: dict[str, Any]) -> dict[str, Any]:
    function = tool_call.get("function") or {}
    raw_arguments = function.get("arguments")
    if isinstance(raw_arguments, dict):
        return raw_arguments
    if not isinstance(raw_arguments, str) or not raw_arguments:
        return {}
    try:
        parsed = json.loads(raw_arguments)
    except json.JSONDecodeError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def read_file_arg_key(arguments: dict[str, Any]) -> tuple[int, int, bool]:
    start_line = int(arguments.get("start_line") or DEFAULT_READ_FILE_START_LINE)
    end_line = int(arguments.get("end_line") or DEFAULT_READ_FILE_END_LINE)
    line_numbers = bool(arguments.get("line_numbers", True))
    return start_line, end_line, line_numbers


def bash_key_command(command: str) -> str:
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return "unknown"

    expect_command = True
    skip_next = False
    control_tokens = {"|", "||", "&&", ";", "(", ")"}
    assignments = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")

    for token in tokens:
        if skip_next:
            skip_next = False
            continue
        if token in control_tokens:
            expect_command = True
            continue
        if token in {">", ">>", "<", "2>", "2>>", "&>"}:
            skip_next = True
            continue
        if not expect_command:
            continue
        if assignments.match(token):
            continue
        return token

    return "unknown"


def sed_line_window(command: str) -> tuple[str, int | None, int | None, int | None]:
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError:
        return "unknown", None, None, None

    if not tokens or tokens[0] != "sed":
        return "unknown", None, None, None

    scripts = []
    index = 1
    while index < len(tokens):
        token = tokens[index]
        if token == "-n":
            index += 1
            continue
        if token == "-e" and index + 1 < len(tokens):
            scripts.append(tokens[index + 1])
            index += 2
            continue
        if token.startswith("-"):
            index += 1
            continue
        scripts.append(token)
        break

    for script in scripts:
        match = re.fullmatch(r"\s*(\d+)\s*,\s*(\d+)\s*p\s*", script)
        if match:
            start = int(match.group(1))
            end = int(match.group(2))
            if end < start:
                return "invalid", start, end, None
            return str(end - start + 1), start, end, end - start + 1

    return "unknown", None, None, None


def percent(value: int | float, total: int | float) -> str:
    if not total:
        return "0.0%"
    return f"{(value / total) * 100:.1f}%"


def fmt_int(value: int | None) -> str:
    if value is None:
        return "-"
    return f"{value:,}".replace(",", " ")


def fmt_float(value: float | None, digits: int = 6) -> str:
    if value is None:
        return "-"
    return f"{value:.{digits}f}"


def collect_stats(messages: list[dict[str, Any]]) -> dict[str, Any]:
    role_counts: Counter[str] = Counter()
    content_chars_by_role: Counter[str] = Counter()
    content_bytes_by_role: Counter[str] = Counter()
    reasoning_chars = 0
    messages_with_reasoning = 0
    messages_with_usage = 0
    messages_with_tool_calls = 0
    multipart_messages = 0
    image_parts = 0
    image_url_chars = 0
    total_content_parts = 0

    tool_call_names: dict[str, str] = {}
    bash_tool_call_keys: dict[str, str] = {}
    sed_tool_call_windows: dict[str, str] = {}
    tool_stats: defaultdict[str, ToolStats] = defaultdict(ToolStats)
    read_file_args: defaultdict[tuple[int, int, bool], ReadFileArgStats] = defaultdict(
        ReadFileArgStats
    )
    bash_key_stats: defaultdict[str, BashKeyCommandStats] = defaultdict(BashKeyCommandStats)
    sed_range_stats: defaultdict[str, SedRangeStats] = defaultdict(SedRangeStats)
    orphan_tool_outputs = 0

    usage_totals = Counter()
    cost_total = 0.0
    cost_seen = False

    for message in messages:
        role = str(message.get("role") or "unknown")
        role_counts[role] += 1

        text = content_text(message.get("content"))
        content_chars, content_bytes, message_image_parts, message_image_url_chars, message_parts = content_stats(
            message.get("content")
        )
        content_chars_by_role[role] += content_chars
        content_bytes_by_role[role] += content_bytes
        if message_parts:
            multipart_messages += 1
            total_content_parts += message_parts
        image_parts += message_image_parts
        image_url_chars += message_image_url_chars

        reasoning = message.get("reasoning")
        if isinstance(reasoning, str) and reasoning:
            messages_with_reasoning += 1
            reasoning_chars += len(reasoning)

        tool_calls = message.get("tool_calls") or []
        if isinstance(tool_calls, list) and tool_calls:
            messages_with_tool_calls += 1
            for tool_call in tool_calls:
                if not isinstance(tool_call, dict):
                    continue
                tool_call_id = str(tool_call.get("id") or "")
                function = tool_call.get("function") or {}
                name = str(function.get("name") or "unknown")
                if tool_call_id:
                    tool_call_names[tool_call_id] = name
                tool_stats[name].calls += 1

                arguments = parse_tool_arguments(tool_call)
                if name == "read_file":
                    key = read_file_arg_key(arguments)
                    stats = read_file_args[key]
                    stats.calls += 1
                    if "start_line" not in arguments:
                        stats.default_start_line += 1
                    if "end_line" not in arguments:
                        stats.default_end_line += 1
                    if "line_numbers" not in arguments:
                        stats.default_line_numbers += 1
                elif name == "bash":
                    command = str(arguments.get("command") or "")
                    if command:
                        key_command = bash_key_command(command)
                        if tool_call_id:
                            bash_tool_call_keys[tool_call_id] = key_command
                        bash_key_stats[key_command].calls += 1
                        if key_command == "sed":
                            window_key, _, _, _ = sed_line_window(command)
                            if tool_call_id:
                                sed_tool_call_windows[tool_call_id] = window_key
                            sed_range_stats[window_key].calls += 1

        if role == "tool":
            tool_call_id = str(message.get("tool_call_id") or "")
            tool_name = tool_call_names.get(tool_call_id, "unknown")
            if tool_name == "unknown":
                orphan_tool_outputs += 1
            tool_error = is_tool_error(text)
            tool_stats[tool_name].add_output(content_chars, content_bytes, tool_error)
            if tool_name == "bash":
                key_command = bash_tool_call_keys.get(tool_call_id, "unknown")
                bash_key_stats[key_command].add_output(content_chars, content_bytes, tool_error)
                if key_command == "sed":
                    window_key = sed_tool_call_windows.get(tool_call_id, "unknown")
                    sed_range_stats[window_key].add_output(content_chars, content_bytes, tool_error)

        usage = message.get("usage")
        if isinstance(usage, dict):
            messages_with_usage += 1
            for field in ("prompt_tokens", "completion_tokens", "total_tokens"):
                value = usage.get(field)
                if isinstance(value, int):
                    usage_totals[field] += value
            cost = usage.get("cost")
            if isinstance(cost, (int, float)):
                cost_total += float(cost)
                cost_seen = True

    total_content_chars = sum(content_chars_by_role.values())
    total_content_bytes = sum(content_bytes_by_role.values())
    total_tool_output_chars = sum(stats.output_chars for stats in tool_stats.values())
    total_tool_output_bytes = sum(stats.output_bytes for stats in tool_stats.values())
    total_bash_output_chars = sum(stats.output_chars for stats in bash_key_stats.values())
    total_bash_output_bytes = sum(stats.output_bytes for stats in bash_key_stats.values())
    total_sed_output_chars = sum(stats.output_chars for stats in sed_range_stats.values())
    total_sed_output_bytes = sum(stats.output_bytes for stats in sed_range_stats.values())

    return {
        "role_counts": role_counts,
        "content_chars_by_role": content_chars_by_role,
        "content_bytes_by_role": content_bytes_by_role,
        "total_messages": len(messages),
        "total_content_chars": total_content_chars,
        "total_content_bytes": total_content_bytes,
        "messages_with_reasoning": messages_with_reasoning,
        "reasoning_chars": reasoning_chars,
        "messages_with_usage": messages_with_usage,
        "messages_with_tool_calls": messages_with_tool_calls,
        "multipart_messages": multipart_messages,
        "image_parts": image_parts,
        "image_url_chars": image_url_chars,
        "total_content_parts": total_content_parts,
        "tool_stats": dict(tool_stats),
        "read_file_args": dict(read_file_args),
        "bash_key_stats": dict(bash_key_stats),
        "sed_range_stats": dict(sed_range_stats),
        "total_bash_output_chars": total_bash_output_chars,
        "total_bash_output_bytes": total_bash_output_bytes,
        "total_sed_output_chars": total_sed_output_chars,
        "total_sed_output_bytes": total_sed_output_bytes,
        "total_tool_output_chars": total_tool_output_chars,
        "total_tool_output_bytes": total_tool_output_bytes,
        "orphan_tool_outputs": orphan_tool_outputs,
        "usage_totals": usage_totals,
        "cost_total": cost_total if cost_seen else None,
    }


def html_page(trajectory_path: Path, stats: dict[str, Any]) -> str:
    role_rows = []
    for role, count in sorted(stats["role_counts"].items()):
        chars = stats["content_chars_by_role"][role]
        bytes_len = stats["content_bytes_by_role"][role]
        role_rows.append(
            row(
                role,
                fmt_int(count),
                percent(count, stats["total_messages"]),
                fmt_int(chars),
                percent(chars, stats["total_content_chars"]),
                fmt_int(bytes_len),
            )
        )

    tool_rows = []
    tool_stats: dict[str, ToolStats] = stats["tool_stats"]
    for name, item in sorted(
        tool_stats.items(), key=lambda pair: (-pair[1].calls, -pair[1].output_chars, pair[0])
    ):
        avg = round(item.output_chars / item.outputs) if item.outputs else 0
        tool_rows.append(
            row(
                name,
                fmt_int(item.calls),
                fmt_int(item.outputs),
                fmt_int(item.errors),
                percent(item.errors, item.outputs),
                fmt_int(item.output_chars),
                percent(item.output_chars, stats["total_tool_output_chars"]),
                fmt_int(item.output_bytes),
                fmt_int(item.min_output_chars),
                fmt_int(avg),
                fmt_int(item.max_output_chars),
            )
        )

    usage = stats["usage_totals"]
    summary_items = [
        ("Messages", fmt_int(stats["total_messages"])),
        ("Content chars", fmt_int(stats["total_content_chars"])),
        ("Content bytes", fmt_int(stats["total_content_bytes"])),
        ("Tool output chars", fmt_int(stats["total_tool_output_chars"])),
        ("Tool output bytes", fmt_int(stats["total_tool_output_bytes"])),
        ("Tool calls", fmt_int(sum(item.calls for item in tool_stats.values()))),
        ("Tool errors", fmt_int(sum(item.errors for item in tool_stats.values()))),
        ("Reasoning chars", fmt_int(stats["reasoning_chars"])),
        ("Prompt tokens", fmt_int(usage.get("prompt_tokens", 0))),
        ("Completion tokens", fmt_int(usage.get("completion_tokens", 0))),
        ("Total tokens", fmt_int(usage.get("total_tokens", 0))),
        ("Cost", fmt_float(stats["cost_total"])),
    ]

    summary_cards = "\n".join(
        f"<div class=\"card\"><div class=\"label\">{h(label)}</div><div class=\"value\">{h(value)}</div></div>"
        for label, value in summary_items
    )

    tool_table = table(
        [
            "Tool",
            "Calls",
            "Outputs",
            "Errors",
            "Error %",
            "Output chars",
            "Output %",
            "Output bytes",
            "Min chars",
            "Avg chars",
            "Max chars",
        ],
        tool_rows,
        empty="No tool calls.",
    )
    read_file_table = read_file_arguments_table(stats)
    bash_key_commands_table = bash_key_commands_report_table(stats)
    sed_line_windows_table = sed_line_windows_report_table(stats)
    role_table = table(
        ["Role", "Messages", "Message %", "Content chars", "Content %", "Content bytes"],
        role_rows,
    )

    extra_rows = [
        row("Messages with reasoning", fmt_int(stats["messages_with_reasoning"])),
        row("Messages with usage", fmt_int(stats["messages_with_usage"])),
        row("Assistant messages with tool calls", fmt_int(stats["messages_with_tool_calls"])),
        row("Multipart messages", fmt_int(stats["multipart_messages"])),
        row("Content parts", fmt_int(stats["total_content_parts"])),
        row("Image parts", fmt_int(stats["image_parts"])),
        row("Image URL chars", fmt_int(stats["image_url_chars"])),
        row("Tool outputs without matching tool_call_id", fmt_int(stats["orphan_tool_outputs"])),
    ]

    title = f"Trajectory report: {trajectory_path.name}"
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{h(title)}</title>
  <style>
    :root {{
      color-scheme: light;
      --bg: #f7f8fa;
      --panel: #ffffff;
      --text: #17202a;
      --muted: #5d6b7a;
      --line: #d9e0e7;
      --accent: #2f6f73;
    }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font: 14px/1.45 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    main {{
      max-width: 1180px;
      margin: 0 auto;
      padding: 28px;
    }}
    h1 {{
      margin: 0 0 4px;
      font-size: 26px;
      line-height: 1.2;
      letter-spacing: 0;
    }}
    h2 {{
      margin: 28px 0 10px;
      font-size: 18px;
      letter-spacing: 0;
    }}
    .path {{
      color: var(--muted);
      word-break: break-all;
    }}
    .cards {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
      gap: 10px;
      margin-top: 20px;
    }}
    .card {{
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      padding: 12px;
    }}
    .label {{
      color: var(--muted);
      font-size: 12px;
    }}
    .value {{
      margin-top: 3px;
      font-size: 20px;
      font-weight: 650;
    }}
    table {{
      width: 100%;
      border-collapse: collapse;
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      overflow: hidden;
    }}
    th, td {{
      padding: 8px 10px;
      border-bottom: 1px solid var(--line);
      text-align: right;
      white-space: nowrap;
    }}
    th:first-child, td:first-child {{
      text-align: left;
      white-space: normal;
      overflow-wrap: anywhere;
    }}
    tr:last-child td {{
      border-bottom: 0;
    }}
    th {{
      color: var(--muted);
      font-size: 12px;
      font-weight: 650;
      background: #eef3f5;
    }}
    .empty {{
      padding: 12px;
      color: var(--muted);
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
    }}
  </style>
</head>
<body>
  <main>
    <h1>{h(title)}</h1>
    <div class="path">{h(str(trajectory_path))}</div>
    <section class="cards">
      {summary_cards}
    </section>
    <h2>Tool Usage</h2>
    {tool_table}
    <h2>Read File Arguments</h2>
    {read_file_table}
    <h2>Bash Key Commands</h2>
    {bash_key_commands_table}
    <h2>Sed Line Windows</h2>
    {sed_line_windows_table}
    <h2>Messages By Role</h2>
    {role_table}
    <h2>Additional Stats</h2>
    {table(["Metric", "Value"], extra_rows)}
  </main>
</body>
</html>
"""


def read_file_arguments_table(stats: dict[str, Any]) -> str:
    read_file_args: dict[tuple[int, int, bool], ReadFileArgStats] = stats["read_file_args"]
    rows = []
    total_calls = sum(item.calls for item in read_file_args.values())
    for (start_line, end_line, line_numbers), item in sorted(
        read_file_args.items(), key=lambda pair: (-pair[1].calls, pair[0])
    ):
        rows.append(
            row(
                fmt_int(item.calls),
                percent(item.calls, total_calls),
                fmt_int(start_line),
                fmt_int(end_line),
                fmt_int(end_line - start_line + 1 if end_line >= start_line else None),
                "yes" if line_numbers else "no",
                fmt_int(item.default_start_line),
                fmt_int(item.default_end_line),
                fmt_int(item.default_line_numbers),
            )
        )

    return table(
        [
            "Calls",
            "Call %",
            "Start line",
            "End line",
            "Lines requested",
            "Line numbers",
            "Default start",
            "Default end",
            "Default line numbers",
        ],
        rows,
        empty="No read_file calls.",
    )


def bash_key_commands_report_table(stats: dict[str, Any]) -> str:
    bash_key_stats: dict[str, BashKeyCommandStats] = stats["bash_key_stats"]
    rows = []
    total_calls = sum(item.calls for item in bash_key_stats.values())
    total_output_chars = stats["total_bash_output_chars"]
    for key_command, item in sorted(
        bash_key_stats.items(), key=lambda pair: (-pair[1].calls, -pair[1].output_chars, pair[0])
    ):
        rows.append(
            row(
                key_command,
                fmt_int(item.calls),
                percent(item.calls, total_calls),
                fmt_int(item.outputs),
                fmt_int(item.errors),
                percent(item.errors, item.outputs),
                fmt_int(item.output_chars),
                percent(item.output_chars, total_output_chars),
                fmt_int(item.output_bytes),
            )
        )

    return table(
        [
            "Key command",
            "Calls",
            "Call %",
            "Outputs",
            "Errors",
            "Error %",
            "Output chars",
            "Output %",
            "Output bytes",
        ],
        rows,
        empty="No bash calls.",
    )


def sed_line_windows_report_table(stats: dict[str, Any]) -> str:
    sed_range_stats: dict[str, SedRangeStats] = stats["sed_range_stats"]
    rows = []
    total_calls = sum(item.calls for item in sed_range_stats.values())
    total_output_chars = stats["total_sed_output_chars"]
    for window, item in sorted(
        sed_range_stats.items(), key=lambda pair: (sed_window_sort_key(pair[0]), -pair[1].calls)
    ):
        rows.append(
            row(
                window,
                fmt_int(item.calls),
                percent(item.calls, total_calls),
                fmt_int(item.outputs),
                fmt_int(item.errors),
                percent(item.errors, item.outputs),
                fmt_int(item.output_chars),
                percent(item.output_chars, total_output_chars),
                fmt_int(item.output_bytes),
            )
        )

    return table(
        [
            "Lines requested",
            "Calls",
            "Call %",
            "Outputs",
            "Errors",
            "Error %",
            "Output chars",
            "Output %",
            "Output bytes",
        ],
        rows,
        empty="No sed calls.",
    )


def sed_window_sort_key(window: str) -> tuple[int, int | str]:
    if window.isdigit():
        return 0, int(window)
    return 1, window


def h(value: Any) -> str:
    return html.escape(str(value), quote=True)


def row(*cells: Any) -> str:
    return "<tr>" + "".join(f"<td>{h(cell)}</td>" for cell in cells) + "</tr>"


def table(headers: list[str], rows: list[str], empty: str | None = None) -> str:
    if not rows and empty is not None:
        return f"<div class=\"empty\">{h(empty)}</div>"
    head = "<tr>" + "".join(f"<th>{h(header)}</th>" for header in headers) + "</tr>"
    return f"<table><thead>{head}</thead><tbody>{''.join(rows)}</tbody></table>"


def main() -> None:
    args = parse_args()
    trajectory_path = args.trajectory.expanduser().resolve()
    messages = load_messages(trajectory_path)
    stats = collect_stats(messages)
    output_path = output_path_for(trajectory_path)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(html_page(trajectory_path, stats), encoding="utf-8")
    print(output_path)


if __name__ == "__main__":
    main()

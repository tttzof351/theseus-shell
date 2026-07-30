#![allow(dead_code, unused_imports)]

// Run with: cargo run --bin render_v2
//
// This binary is the standalone diff-rendered implementation of the Theseus
// shell. It intentionally does not reuse the production editors: every UI
// mode below feeds one virtual-screen -> indexed-layout -> physical-diff
// pipeline.

// The prototype is a separate crate root. Including the production modules
// here keeps crate-private low-level agent and PTY APIs reusable without
// widening the public library API. None of the existing editor entry points
// are called below.
#[path = "../agent/mod.rs"]
mod agent;
#[path = "../shell/command_routing.rs"]
mod command_routing_v2;
#[path = "../commands/mod.rs"]
mod commands;
#[path = "../common/mod.rs"]
mod common;
#[path = "../feature_flags.rs"]
mod feature_flags;
#[path = "../input/mod.rs"]
mod input;
#[path = "../logging/mod.rs"]
mod logging;
#[path = "../shell/markdown_preprocessor.rs"]
mod markdown_preprocessor_v2;
#[path = "../agent/config/model_catalog.rs"]
mod model_catalog;
#[path = "../agent/config/models.rs"]
mod models;
#[path = "../shell/mod.rs"]
mod shell;
#[path = "../shell/terminal.rs"]
mod shell_terminal_v2;

use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, read,
    },
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, ScrollUp, disable_raw_mode, enable_raw_mode, size},
};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthChar;

use agent::{Agent, AgentConfig, AgentRunContext, CompactOutcome, ShellCommandContext};
use command_routing_v2::{CommandRoute, classify_command};
use commands::{SlashCommand, parse_slash_command};
use common::tmp_files::cleanup_expired_tmp_files_async;
use logging::{AppLogger, default_logs_dir};
use shell::pty::{PersistentShellConfig, PersistentShellSession};

const SHELL_PROMPT: &str = "user> ";
const TERMINAL_TAB_STOP: usize = 8;
const MAX_PERSISTED_HISTORY: usize = 100;
const MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES: usize = 32 * 1024;

enum ConfigPatch {
    SetModel(String),
    SetAuthorization(String),
}

fn patch_config_jsonc_file(path: &Path, patch: ConfigPatch) -> io::Result<AgentConfig> {
    let text = fs::read_to_string(path)?;
    let patched = patch_config_jsonc_text(&text, patch)?;

    // Validate the complete typed configuration before replacing the real
    // file. The CST parser above proves syntax and shape around the edited
    // value; loading a private sibling proves all AgentConfig invariants too.
    let validation = create_validation_config_file(path, &patched)?;
    let config = AgentConfig::load_or_create_at(validation.path.clone())?.config;
    drop(validation);

    fs::write(path, patched)?;
    Ok(config)
}

fn patch_config_jsonc_text(text: &str, patch: ConfigPatch) -> io::Result<String> {
    let root = CstRootNode::parse(text, &config_jsonc_parse_options())
        .map_err(|error| invalid_config(format!("config must be valid JSONC: {error}")))?;
    let object = root
        .object_value()
        .ok_or_else(|| invalid_config("config root must be an object"))?;

    match patch {
        ConfigPatch::SetModel(model) => {
            let settings =
                config_object_field(&object, "llm_request_settings", "llm_request_settings")?;
            let body = config_object_field(&settings, "body", "llm_request_settings.body")?;
            set_or_insert_config_string(
                &body,
                "model",
                model,
                &[
                    "tool_choice",
                    "parallel_tool_calls",
                    "include_reasoning",
                    "max_tokens",
                ],
            );
        }
        ConfigPatch::SetAuthorization(authorization) => {
            let settings =
                config_object_field(&object, "llm_request_settings", "llm_request_settings")?;
            let header = config_object_field(&settings, "header", "llm_request_settings.header")?;
            set_or_insert_config_string(&header, "Authorization", authorization, &["Content-Type"]);
        }
    }

    Ok(root.to_string())
}

fn config_object_field(object: &CstObject, name: &str, path: &str) -> io::Result<CstObject> {
    object
        .object_value(name)
        .ok_or_else(|| invalid_config(format!("config field `{path}` must be an object")))
}

fn set_or_insert_config_string(object: &CstObject, name: &str, value: String, before: &[&str]) {
    let value = CstInputValue::String(value);
    match object.get(name) {
        Some(property) => property.set_value(value),
        None => {
            if let Some(index) = before.iter().find_map(|candidate| {
                object
                    .get(candidate)
                    .map(|property| property.property_index())
            }) {
                object.insert(index, name, value);
            } else {
                object.append(name, value);
            }
        }
    }
}

fn config_jsonc_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct ValidationConfigFile {
    path: PathBuf,
}

impl Drop for ValidationConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_validation_config_file(path: &Path, text: &str) -> io::Result<ValidationConfigFile> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.jsonc");

    for suffix in 0..100 {
        let candidate = parent.join(format!(
            ".{name}.render-v2-{}-{suffix}.tmp",
            std::process::id()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(text.as_bytes()) {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(ValidationConfigFile { path: candidate });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary config validation file",
    ))
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let result = match parse_cli(&args) {
        Ok(Cli::Version) => {
            println!("theseus {}", commands::VERSION);
            Ok(0)
        }
        Ok(Cli::Help) => {
            print_cli_help();
            Ok(0)
        }
        Ok(Cli::Headless(prompt)) => run_headless(&prompt),
        Ok(Cli::Shell(args)) => run_application(args),
        Err(error) => {
            eprintln!("theseus: {error}\n");
            print_cli_help();
            Ok(2)
        }
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("theseus: {error}");
            std::process::exit(2);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
    Shell(Vec<String>),
    Headless(String),
    Version,
    Help,
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut iter = args.iter();
    let mut shell_args = Vec::new();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-v" | "--version" => return Ok(Cli::Version),
            "-h" | "--help" => return Ok(Cli::Help),
            "-p" | "--prompt" => {
                let prompt = iter
                    .next()
                    .ok_or_else(|| format!("`{arg}` requires a prompt argument"))?
                    .clone();
                if iter.next().is_some() {
                    return Err(format!(
                        "`{arg}` does not accept additional arguments after the prompt"
                    ));
                }
                return Ok(Cli::Headless(prompt));
            }
            _ => shell_args.push(arg.clone()),
        }
    }
    Ok(Cli::Shell(shell_args))
}

fn print_cli_help() {
    println!(
        "Theseus shell wrapper (v{})\n\n\
         -p --prompt 'Say Hello'   run the agent non-interactively\n\
         -v --version              print the version and exit\n\
         -h --help                 print this help and exit\n\n\
         ~/.theseus/config.jsonc\n\
         ~/.theseus/logs",
        commands::VERSION
    );
}

fn is_key_action(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste, Print("\x1b[0m"), Show);
        let _ = disable_raw_mode();
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VirtualCursor {
    line: usize,
    char_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VirtualScreen {
    lines: Vec<String>,
    prefixes: Vec<String>,
    line_styles: Vec<Vec<CellStyle>>,
    prefix_styles: Vec<Vec<CellStyle>>,
    cursor: VirtualCursor,
    cursor_visible: bool,
    dirty_from: usize,
}

impl VirtualScreen {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            prefixes: vec![SHELL_PROMPT.to_string()],
            line_styles: vec![Vec::new()],
            prefix_styles: vec![vec![CellStyle::default(); char_len(SHELL_PROMPT)]],
            cursor: VirtualCursor {
                line: 0,
                char_offset: 0,
            },
            cursor_visible: true,
            dirty_from: 0,
        }
    }

    #[cfg(test)]
    fn with_line(text: impl Into<String>, char_offset: usize) -> Self {
        let text = text.into();
        let line_len = char_len(&text);
        let char_offset = char_offset.min(line_len);
        Self {
            lines: vec![text],
            prefixes: vec![SHELL_PROMPT.to_string()],
            line_styles: vec![vec![CellStyle::default(); line_len]],
            prefix_styles: vec![vec![CellStyle::default(); char_len(SHELL_PROMPT)]],
            cursor: VirtualCursor {
                line: 0,
                char_offset,
            },
            cursor_visible: true,
            dirty_from: 0,
        }
    }

    fn from_render_lines(
        lines: Vec<RenderLine>,
        cursor: VirtualCursor,
        cursor_visible: bool,
    ) -> Self {
        let mut prefixes = Vec::with_capacity(lines.len());
        let mut logical_lines = Vec::with_capacity(lines.len());
        let mut prefix_styles = Vec::with_capacity(lines.len());
        let mut line_styles = Vec::with_capacity(lines.len());
        for line in lines {
            prefixes.push(line.prefix);
            logical_lines.push(line.text);
            prefix_styles.push(line.prefix_styles);
            line_styles.push(line.styles);
        }
        Self {
            lines: logical_lines,
            prefixes,
            line_styles,
            prefix_styles,
            cursor,
            cursor_visible,
            dirty_from: 0,
        }
    }

    fn truncate(&mut self, length: usize) {
        self.lines.truncate(length);
        self.prefixes.truncate(length);
        self.line_styles.truncate(length);
        self.prefix_styles.truncate(length);
        self.dirty_from = self.dirty_from.min(length);
    }

    fn push_render_line(&mut self, line: &RenderLine) {
        self.prefixes.push(line.prefix.clone());
        self.lines.push(line.text.clone());
        self.prefix_styles.push(line.prefix_styles.clone());
        self.line_styles.push(line.styles.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderLine {
    prefix: String,
    text: String,
    prefix_styles: Vec<CellStyle>,
    styles: Vec<CellStyle>,
}

impl RenderLine {
    fn new(prefix: impl Into<String>, text: impl Into<String>) -> Self {
        let prefix = prefix.into();
        let text = text.into();
        Self {
            prefix_styles: vec![CellStyle::default(); char_len(&prefix)],
            styles: vec![CellStyle::default(); char_len(&text)],
            prefix,
            text,
        }
    }

    fn plain(text: impl Into<String>) -> Self {
        Self::new("", text)
    }

    fn styled(text: impl Into<String>, styles: Vec<CellStyle>) -> Self {
        let text = text.into();
        debug_assert_eq!(char_len(&text), styles.len());
        Self {
            prefix: String::new(),
            prefix_styles: Vec::new(),
            text,
            styles,
        }
    }

    fn style_range(&mut self, start: usize, end: usize, style: CellStyle) {
        for cell_style in self.styles.iter_mut().take(end).skip(start) {
            *cell_style = style;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct CellStyle {
    foreground: Option<TerminalColor>,
    background: Option<TerminalColor>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    reverse: bool,
    strikethrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TerminalColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorBuffer {
    lines: Vec<String>,
    cursor: VirtualCursor,
    goal_column: Option<usize>,
}

impl EditorBuffer {
    fn new() -> Self {
        Self::from_text("")
    }

    fn from_text(text: &str) -> Self {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = normalized
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let line = lines.len().saturating_sub(1);
        let char_offset = lines.get(line).map_or(0, |line| char_len(line));
        Self {
            lines: if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            },
            cursor: VirtualCursor { line, char_offset },
            goal_column: None,
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor.line]
    }

    fn replace_with(&mut self, text: &str) {
        *self = Self::from_text(text);
    }

    fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        line.insert(offset, ch);
        self.cursor.char_offset += 1;
        self.goal_column = None;
    }

    fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            match ch {
                '\n' => self.split_line(),
                ch if !ch.is_control() => self.insert_char(ch),
                '\t' => self.insert_char(ch),
                _ => {}
            }
        }
    }

    fn split_line(&mut self) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        let tail = line.split_off(offset);
        self.cursor.line += 1;
        self.cursor.char_offset = 0;
        self.lines.insert(self.cursor.line, tail);
        self.goal_column = None;
    }

    fn remove_current_line(&mut self) {
        if self.lines.len() == 1 {
            self.lines[0].clear();
            self.cursor = VirtualCursor {
                line: 0,
                char_offset: 0,
            };
            return;
        }
        self.lines.remove(self.cursor.line);
        self.cursor.line = self.cursor.line.saturating_sub(1);
        self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
    }

    fn backspace(&mut self) {
        if self.cursor.char_offset > 0 {
            let line = &mut self.lines[self.cursor.line];
            let end = byte_offset(line, self.cursor.char_offset);
            let start = byte_offset(line, self.cursor.char_offset - 1);
            line.replace_range(start..end, "");
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            let current = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
            self.lines[self.cursor.line].push_str(&current);
        }
        self.goal_column = None;
    }

    fn delete(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_offset(line, self.cursor.char_offset);
            let end = byte_offset(line, self.cursor.char_offset + 1);
            line.replace_range(start..end, "");
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
        self.goal_column = None;
    }

    fn move_left(&mut self) {
        if self.cursor.char_offset > 0 {
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
        }
        self.goal_column = None;
    }

    fn move_right(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            self.cursor.char_offset += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.char_offset = 0;
        }
        self.goal_column = None;
    }

    fn move_up(&mut self) -> bool {
        if self.cursor.line == 0 {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line -= 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    fn move_down(&mut self) -> bool {
        if self.cursor.line + 1 >= self.lines.len() {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line += 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    fn move_home(&mut self) {
        self.cursor.char_offset = 0;
        self.goal_column = None;
    }

    fn move_end(&mut self) {
        self.cursor.char_offset = char_len(self.current_line());
        self.goal_column = None;
    }

    fn move_word_left(&mut self) {
        if self.cursor.char_offset == 0 {
            self.move_left();
            return;
        }
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    fn move_word_right(&mut self) {
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        let cursor = self.cursor.char_offset;
        if start > cursor {
            return;
        }
        let line = &mut self.lines[self.cursor.line];
        let byte_start = byte_offset(line, start);
        let byte_end = byte_offset(line, cursor);
        line.replace_range(byte_start..byte_end, replacement);
        self.cursor.char_offset = start + char_len(replacement);
        self.goal_column = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
enum HistoryKind {
    Shell,
    Agent,
    Special,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
enum HistoryMode {
    SingleLine,
    SingleLineAsk,
    MultiLineAsk,
    MultiLineShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
struct HistoryEntry {
    text: String,
    kind: HistoryKind,
    mode: HistoryMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    Command,
    Ask,
    Shell,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionKind {
    Command,
    Ask,
    Shell,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorSubmission {
    kind: SubmissionKind,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorOutcome {
    Changed,
    Redraw,
    Submit(EditorSubmission),
    OpenMultiline(EditorMode, String),
    Cancel,
    Exit,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionCandidate {
    replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionCycle {
    start: usize,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct UnifiedEditor {
    mode: EditorMode,
    buffer: EditorBuffer,
    prompt: String,
    continuation_prompt: String,
    history: Vec<HistoryEntry>,
    history_indices: Vec<usize>,
    history_position: Option<usize>,
    history_draft: String,
    recalled_mode: Option<HistoryMode>,
    completion: Option<CompletionCycle>,
}

impl UnifiedEditor {
    fn command(prompt: String, history: &[HistoryEntry]) -> Self {
        Self::new(EditorMode::Command, prompt, "· ", history)
    }

    fn ask(initial: Option<String>, history: &[HistoryEntry]) -> Self {
        let mut editor = Self::new(EditorMode::Ask, "· ".to_string(), "· ", history);
        if let Some(initial) = initial {
            editor.buffer.replace_with(&initial);
        }
        editor
    }

    fn shell(initial: Option<String>, history: &[HistoryEntry]) -> Self {
        let mut editor = Self::new(EditorMode::Shell, "· ".to_string(), "· ", history);
        if let Some(initial) = initial {
            editor.buffer.replace_with(&initial);
        }
        editor
    }

    fn api_key() -> Self {
        Self::new(
            EditorMode::ApiKey,
            "Openrouter API key: ".to_string(),
            "",
            &[],
        )
    }

    fn new(
        mode: EditorMode,
        prompt: String,
        continuation_prompt: &str,
        history: &[HistoryEntry],
    ) -> Self {
        let history_indices = history
            .iter()
            .enumerate()
            .filter(|(_, entry)| match mode {
                EditorMode::Command => true,
                EditorMode::Ask => entry.kind == HistoryKind::Agent,
                EditorMode::Shell => entry.kind == HistoryKind::Shell,
                EditorMode::ApiKey => false,
            })
            .map(|(index, _)| index)
            .collect();
        Self {
            mode,
            buffer: EditorBuffer::new(),
            prompt,
            continuation_prompt: continuation_prompt.to_string(),
            history: history.to_vec(),
            history_indices,
            history_position: None,
            history_draft: String::new(),
            recalled_mode: None,
            completion: None,
        }
    }

    fn render_lines(&self) -> (Vec<RenderLine>, VirtualCursor) {
        if self.mode == EditorMode::Command
            && self.history_is_browsing()
            && matches!(
                self.recalled_mode,
                Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
            )
        {
            let (command, hint) = match self.recalled_mode {
                Some(HistoryMode::MultiLineAsk) => (
                    "/ask",
                    format!(
                        "Enter multiline input. Type {} on a new line to finish.",
                        input::MULTILINE_SUBMIT_COMMAND
                    ),
                ),
                Some(HistoryMode::MultiLineShell) => (
                    "/shell",
                    format!(
                        "Enter multiline shell command. Type {} on a new line to run.",
                        input::MULTILINE_SUBMIT_COMMAND
                    ),
                ),
                _ => unreachable!(),
            };
            let mut lines = vec![
                RenderLine::new(self.prompt.clone(), command),
                RenderLine::plain(hint),
            ];
            lines.extend(
                self.buffer
                    .lines
                    .iter()
                    .map(|text| RenderLine::new(self.continuation_prompt.clone(), text.clone())),
            );
            return (
                lines,
                VirtualCursor {
                    line: self.buffer.cursor.line + 2,
                    char_offset: self.buffer.cursor.char_offset,
                },
            );
        }

        let lines = self
            .buffer
            .lines
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let prefix = if index == 0 {
                    self.prompt.clone()
                } else {
                    self.continuation_prompt.clone()
                };
                let text = if self.mode == EditorMode::ApiKey {
                    "*".repeat(char_len(text))
                } else {
                    text.clone()
                };
                RenderLine::new(prefix, text)
            })
            .collect();
        (lines, self.buffer.cursor)
    }

    fn handle_event(&mut self, event: Event) -> EditorOutcome {
        match event {
            Event::Paste(text) => self.handle_paste(&text),
            Event::Key(key) if is_key_action(key.kind) => self.handle_key(key),
            _ => EditorOutcome::Unchanged,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EditorOutcome {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return EditorOutcome::Cancel;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return match self.mode {
                EditorMode::Command if self.buffer.is_empty() => EditorOutcome::Exit,
                EditorMode::Ask => self.submit(SubmissionKind::Ask),
                EditorMode::Shell => self.submit(SubmissionKind::Shell),
                EditorMode::ApiKey if self.buffer.is_empty() => EditorOutcome::Cancel,
                _ => EditorOutcome::Unchanged,
            };
        }
        if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return EditorOutcome::Redraw;
        }
        if key.code == KeyCode::Esc {
            return EditorOutcome::Unchanged;
        }

        if self.history_is_browsing()
            && matches!(
                key.code,
                KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab | KeyCode::Char(_)
            )
        {
            // Recalled history is a preview until Enter or a navigation key
            // explicitly accepts it. Destructive/text keys cannot
            // accidentally modify a history entry while it is still a
            // browsing selection.
            return EditorOutcome::Changed;
        }

        if self.history_is_browsing() {
            let keeps_preview = matches!(key.code, KeyCode::Home | KeyCode::End)
                || matches!(key.code, KeyCode::Left | KeyCode::Right)
                    && key.modifiers.intersects(
                        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
                    );
            if keeps_preview {
                return EditorOutcome::Changed;
            }
            if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                if let Some(mode) = self.recalled_multiline_editor_mode() {
                    return EditorOutcome::OpenMultiline(mode, self.buffer.text());
                }
                self.stop_history();
            }
        }

        match key.code {
            KeyCode::Enter => return self.handle_enter(),
            KeyCode::Backspace => {
                self.prepare_edit();
                self.buffer.backspace();
            }
            KeyCode::Delete => {
                self.prepare_edit();
                self.buffer.delete();
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.buffer.move_home();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.buffer.move_end();
            }
            KeyCode::Left
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.buffer.move_word_left();
            }
            KeyCode::Right
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.buffer.move_word_right();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.buffer.move_word_left();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.buffer.move_word_right();
            }
            KeyCode::Left => {
                self.buffer.move_left();
            }
            KeyCode::Right => {
                self.buffer.move_right();
            }
            KeyCode::Up => {
                if self.history_position.is_some()
                    || (self.mode == EditorMode::Command && self.buffer.lines.len() == 1)
                    || self.buffer.is_empty()
                {
                    self.history_previous();
                } else {
                    self.buffer.move_up();
                }
            }
            KeyCode::Down => {
                if self.history_position.is_some()
                    || (self.mode == EditorMode::Command && self.buffer.lines.len() == 1)
                    || (self.buffer.cursor.line + 1 == self.buffer.lines.len()
                        && self.buffer.cursor.char_offset == char_len(self.buffer.current_line()))
                {
                    self.history_next();
                } else {
                    self.buffer.move_down();
                }
            }
            KeyCode::Home => {
                self.buffer.move_home();
            }
            KeyCode::End => {
                self.buffer.move_end();
            }
            KeyCode::Tab if self.mode != EditorMode::ApiKey => {
                self.advance_completion();
                return EditorOutcome::Changed;
            }
            KeyCode::Tab => return EditorOutcome::Unchanged,
            KeyCode::Char(ch) if is_plain_text_key_v2(key) && !ch.is_control() => {
                self.prepare_edit();
                self.buffer.insert_char(ch);
            }
            _ => return EditorOutcome::Unchanged,
        }
        self.completion = None;
        EditorOutcome::Changed
    }

    fn handle_enter(&mut self) -> EditorOutcome {
        self.completion = None;
        if self.history_is_browsing() {
            if let Some(mode) = self.recalled_multiline_editor_mode() {
                return EditorOutcome::OpenMultiline(mode, self.buffer.text());
            }
            self.stop_history();
            return EditorOutcome::Changed;
        }

        match self.mode {
            EditorMode::Command => {
                if shell::input_syntax::should_read_shell_continuation(&self.buffer.text()) {
                    self.buffer.split_line();
                    EditorOutcome::Changed
                } else {
                    self.submit(SubmissionKind::Command)
                }
            }
            EditorMode::Ask | EditorMode::Shell => {
                if self.buffer.current_line().trim() == input::MULTILINE_SUBMIT_COMMAND
                    && self.buffer.cursor.line + 1 == self.buffer.lines.len()
                {
                    self.buffer.remove_current_line();
                    let kind = if self.mode == EditorMode::Ask {
                        SubmissionKind::Ask
                    } else {
                        SubmissionKind::Shell
                    };
                    self.submit(kind)
                } else {
                    self.buffer.split_line();
                    EditorOutcome::Changed
                }
            }
            EditorMode::ApiKey => self.submit(SubmissionKind::ApiKey),
        }
    }

    fn handle_paste(&mut self, text: &str) -> EditorOutcome {
        if self.history_is_browsing() {
            return EditorOutcome::Changed;
        }
        self.prepare_edit();
        if self.mode == EditorMode::ApiKey {
            let filtered = text
                .chars()
                .filter(|ch| !matches!(ch, '\r' | '\n'))
                .collect::<String>();
            self.buffer.insert_text(&filtered);
            return EditorOutcome::Changed;
        }
        self.buffer.insert_text(text);
        self.completion = None;
        let ended_with_newline = text.ends_with(['\r', '\n']);
        if self.mode == EditorMode::Command
            && ended_with_newline
            && !shell::input_syntax::should_read_shell_continuation(&self.buffer.text())
        {
            while self.buffer.lines.last().is_some_and(String::is_empty)
                && self.buffer.lines.len() > 1
            {
                self.buffer.lines.pop();
            }
            self.buffer.cursor.line = self.buffer.lines.len() - 1;
            self.buffer.cursor.char_offset = char_len(self.buffer.current_line());
            return self.submit(SubmissionKind::Command);
        }
        EditorOutcome::Changed
    }

    fn submit(&self, kind: SubmissionKind) -> EditorOutcome {
        EditorOutcome::Submit(EditorSubmission {
            kind,
            text: self.buffer.text(),
        })
    }

    fn prepare_edit(&mut self) {
        self.completion = None;
    }

    fn history_is_browsing(&self) -> bool {
        self.history_position.is_some()
            && (matches!(self.mode, EditorMode::Ask | EditorMode::Shell)
                || self.buffer.lines.len() > 1
                || matches!(
                    self.recalled_mode,
                    Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
                ))
    }

    fn recalled_multiline_editor_mode(&self) -> Option<EditorMode> {
        if self.mode != EditorMode::Command {
            return None;
        }
        match self.recalled_mode {
            Some(HistoryMode::MultiLineAsk) => Some(EditorMode::Ask),
            Some(HistoryMode::MultiLineShell) => Some(EditorMode::Shell),
            _ => None,
        }
    }

    fn stop_history(&mut self) {
        self.history_position = None;
        self.recalled_mode = None;
    }

    fn history_previous(&mut self) {
        if self.history_indices.is_empty() {
            return;
        }
        let next_position = match self.history_position {
            None => {
                self.history_draft = self.buffer.text();
                self.history_indices.len() - 1
            }
            Some(position) => position.saturating_sub(1),
        };
        self.restore_history(next_position);
    }

    fn history_next(&mut self) {
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 >= self.history_indices.len() {
            self.buffer.replace_with(&self.history_draft);
            self.stop_history();
        } else {
            self.restore_history(position + 1);
        }
    }

    fn restore_history(&mut self, position: usize) {
        let Some(entry) = self
            .history_indices
            .get(position)
            .and_then(|index| self.history.get(*index))
        else {
            return;
        };
        let text = if self.mode == EditorMode::Command && entry.mode == HistoryMode::SingleLineAsk {
            format!("/ask {}", entry.text)
        } else {
            entry.text.clone()
        };
        self.buffer.replace_with(&text);
        self.history_position = Some(position);
        self.recalled_mode = Some(entry.mode);
        self.completion = None;
    }

    fn advance_completion(&mut self) {
        let restart_directory = self.completion.as_ref().is_some_and(|cycle| {
            cycle.candidates.len() == 1
                && cycle.candidates[0].replacement.ends_with(['/', '\\'])
                && self
                    .buffer
                    .current_line()
                    .chars()
                    .skip(cycle.start)
                    .take(self.buffer.cursor.char_offset.saturating_sub(cycle.start))
                    .collect::<String>()
                    == cycle.candidates[0].replacement
        });
        if restart_directory {
            self.completion = None;
        }
        if let Some(cycle) = &mut self.completion {
            cycle.selected = (cycle.selected + 1) % cycle.candidates.len();
            let replacement = cycle.candidates[cycle.selected].replacement.clone();
            self.buffer.replace_before_cursor(cycle.start, &replacement);
            return;
        }

        let line = self.buffer.current_line().to_string();
        let cursor = self.buffer.cursor.char_offset;
        let Some((start, candidates)) = completion_candidates(
            &line,
            cursor,
            matches!(self.mode, EditorMode::Command | EditorMode::Shell)
                && self.buffer.cursor.line == 0,
        ) else {
            return;
        };
        let replacement = candidates[0].replacement.clone();
        self.buffer.replace_before_cursor(start, &replacement);
        self.completion = Some(CompletionCycle {
            start,
            candidates,
            selected: 0,
        });
    }
}

fn completion_candidates(
    line: &str,
    cursor: usize,
    command_completion: bool,
) -> Option<(usize, Vec<CompletionCandidate>)> {
    let before_cursor = line.chars().take(cursor).collect::<String>();
    let start = completion_token_start(&before_cursor);
    let token = before_cursor.chars().skip(start).collect::<String>();
    let first_token = before_cursor[..byte_offset(&before_cursor, start)]
        .trim()
        .is_empty();

    if command_completion && first_token {
        let mut builtins = commands::slash_command_names()
            .chain(["cd", "exit"])
            .filter(|name| name.starts_with(&token))
            .map(str::to_string)
            .collect::<Vec<_>>();
        builtins.sort();
        builtins.dedup();
        if token.is_empty() || !builtins.is_empty() {
            let candidates = builtins
                .into_iter()
                .map(|replacement| CompletionCandidate { replacement })
                .collect::<Vec<_>>();
            return (!candidates.is_empty()).then_some((start, candidates));
        }
        let replacements = if looks_like_path_token(&token) {
            path_completions_with_common_prefix(&token)
        } else {
            path_executables(&token)
        };
        let candidates = replacements
            .into_iter()
            .map(|replacement| CompletionCandidate { replacement })
            .collect::<Vec<_>>();
        return (!candidates.is_empty()).then_some((start, candidates));
    }

    if command_completion && !first_token {
        let preceding_words = before_cursor[..byte_offset(&before_cursor, start)]
            .split_whitespace()
            .count();
        if preceding_words == 1 {
            let command = before_cursor.split_whitespace().next().unwrap_or_default();
            if matches!(command, "git" | "cargo") {
                let candidates = special_subcommand_completions(command, &token)
                    .into_iter()
                    .map(|replacement| CompletionCandidate { replacement })
                    .collect::<Vec<_>>();
                return (!candidates.is_empty()).then_some((start, candidates));
            }
        }
    }

    let candidates = path_completions_with_common_prefix(&token)
        .into_iter()
        .map(|replacement| CompletionCandidate { replacement })
        .collect::<Vec<_>>();
    (!candidates.is_empty()).then_some((start, candidates))
}

fn completion_token_start(text: &str) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    for index in (0..chars.len()).rev() {
        if !chars[index].is_whitespace() {
            continue;
        }
        let backslashes = chars[..index]
            .iter()
            .rev()
            .take_while(|ch| **ch == '\\')
            .count();
        if backslashes % 2 == 0 {
            return index + 1;
        }
    }
    0
}

fn looks_like_path_token(token: &str) -> bool {
    token.starts_with(['.', '/', '~']) || token.contains(['/', '\\'])
}

fn path_executables(prefix: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

fn special_subcommand_completions(command: &str, prefix: &str) -> Vec<String> {
    let values: &[&str] = match command {
        "git" => &[
            "add",
            "bisect",
            "branch",
            "checkout",
            "cherry-pick",
            "clean",
            "clone",
            "commit",
            "diff",
            "fetch",
            "grep",
            "init",
            "log",
            "merge",
            "mv",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "restore",
            "revert",
            "rm",
            "show",
            "stash",
            "status",
            "switch",
            "tag",
            "worktree",
        ],
        "cargo" => &[
            "add", "bench", "build", "check", "clean", "doc", "fetch", "fix", "fmt", "install",
            "login", "metadata", "new", "package", "publish", "remove", "run", "search", "test",
            "tree", "update",
        ],
        _ => return Vec::new(),
    };
    values
        .iter()
        .filter(|value| value.starts_with(prefix))
        .map(|value| (*value).to_string())
        .collect()
}

fn common_char_prefix(values: &[String]) -> Option<String> {
    let mut prefix = values.first()?.clone();
    for value in &values[1..] {
        let length = prefix
            .chars()
            .zip(value.chars())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(byte_offset(&prefix, length));
    }
    Some(prefix)
}

fn path_completions_with_common_prefix(token: &str) -> Vec<String> {
    let mut candidates = path_completions(token);
    if candidates.len() > 1
        && let Some(common) = common_char_prefix(&candidates)
        && char_len(&common) > char_len(token)
    {
        candidates.insert(0, common);
    }
    candidates
}

fn path_completions(token: &str) -> Vec<String> {
    let unescaped = unescape_shell_token(token);
    let (directory_text, file_prefix) = unescaped
        .rsplit_once('/')
        .map_or(("", unescaped.as_str()), |(directory, file)| {
            (directory, file)
        });
    let lookup_directory = if directory_text.is_empty() {
        PathBuf::from(".")
    } else if directory_text == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(rest) = directory_text.strip_prefix("~/") {
        home_dir().map_or_else(|| PathBuf::from(directory_text), |home| home.join(rest))
    } else {
        PathBuf::from(directory_text)
    };
    let Ok(entries) = fs::read_dir(lookup_directory) else {
        return Vec::new();
    };
    let rendered_directory = if directory_text.is_empty() {
        String::new()
    } else {
        format!("{directory_text}/")
    };
    let mut matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with(file_prefix)
                || (name.starts_with('.') && !file_prefix.starts_with('.'))
            {
                return None;
            }
            Some((
                escape_shell_token(&format!("{rendered_directory}{name}")),
                entry.path().is_dir(),
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    let single_directory = matches.len() == 1 && matches[0].1;
    let mut results = matches
        .into_iter()
        .map(|(mut replacement, _)| {
            if single_directory {
                replacement.push('/');
            }
            replacement
        })
        .collect::<Vec<_>>();
    results.sort();
    results
}

fn unescape_shell_token(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                output.push(next);
            } else {
                output.push(ch);
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn escape_shell_token(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '\\' | '\''
                    | '"'
                    | '$'
                    | '&'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '#'
            )
        {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[derive(Debug, Clone)]
struct PickerItem {
    id: String,
    label: String,
    detail: String,
}

#[derive(Debug, Clone)]
struct PickerState {
    title: String,
    query: String,
    items: Vec<PickerItem>,
    filtered: Vec<usize>,
    selected: usize,
    searchable: bool,
    viewport_rows: usize,
}

impl PickerState {
    fn new(title: impl Into<String>, items: Vec<PickerItem>, searchable: bool) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            query: String::new(),
            items,
            filtered,
            selected: 0,
            searchable,
            viewport_rows: 10,
        }
    }

    fn with_selected_id(mut self, id: Option<&str>) -> Self {
        if let Some(id) = id
            && let Some(position) = self
                .filtered
                .iter()
                .position(|index| self.items[*index].id == id)
        {
            self.selected = position;
        }
        self
    }

    fn selected_item(&self) -> Option<&PickerItem> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    fn move_by(&mut self, amount: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.filtered.len() - 1);
    }

    fn refresh(&mut self) {
        let terms = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack =
                    format!("{} {} {}", item.id, item.label, item.detail).to_ascii_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return PickerOutcome::Cancel;
        }
        match key.code {
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_by(self.viewport_rows as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.filtered.len().saturating_sub(1),
            KeyCode::Enter => {
                return self
                    .selected_item()
                    .map(|item| PickerOutcome::Submit(item.id.clone()))
                    .unwrap_or(PickerOutcome::Changed);
            }
            KeyCode::Backspace if self.searchable => {
                self.query.pop();
                self.refresh();
            }
            KeyCode::Char(ch) if self.searchable && is_plain_text_key_v2(key) => {
                self.query.push(ch);
                self.refresh();
            }
            _ => return PickerOutcome::Unchanged,
        }
        PickerOutcome::Changed
    }

    fn render_lines(
        &mut self,
        viewport_rows: usize,
        width: usize,
    ) -> (Vec<RenderLine>, VirtualCursor, bool) {
        let mut lines = vec![RenderLine::plain(truncate_for_width(&self.title, width))];
        if self.searchable {
            lines.push(RenderLine::new(
                "Search: ",
                truncate_for_width(&self.query, width.saturating_sub(char_len("Search: "))),
            ));
        }
        let row_capacity = viewport_rows.max(1).min(self.items.len().max(1));
        self.viewport_rows = row_capacity;
        let mut rendered_rows = 0;
        if self.filtered.is_empty() {
            lines.push(RenderLine::plain("  No matches"));
            rendered_rows = 1;
        } else {
            let rows = row_capacity.min(self.filtered.len());
            let top = self
                .selected
                .saturating_sub(rows / 2)
                .min(self.filtered.len().saturating_sub(rows));
            for position in top..top + rows {
                let item = &self.items[self.filtered[position]];
                let marker = if position == self.selected {
                    "> "
                } else {
                    "  "
                };
                let detail = if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", item.detail)
                };
                lines.push(RenderLine::plain(truncate_for_width(
                    &format!("{marker}{}{detail}", item.label),
                    width,
                )));
                rendered_rows += 1;
            }
        }
        lines.extend(
            std::iter::repeat_with(|| RenderLine::plain(""))
                .take(row_capacity.saturating_sub(rendered_rows)),
        );
        lines.push(RenderLine::plain("Enter: select · Esc/Ctrl+C: cancel"));
        // The virtual cursor doubles as the viewport anchor. Keep it on the
        // last picker row so a picker opened near the bottom of a long
        // transcript remains fully visible.
        let cursor_line = lines.len().saturating_sub(1);
        let cursor = VirtualCursor {
            line: cursor_line,
            char_offset: char_len(&lines[cursor_line].text),
        };
        (lines, cursor, false)
    }
}

fn truncate_for_width(text: &str, width: usize) -> String {
    if char_len(text) <= width {
        return text.to_string();
    }
    match width {
        0 => String::new(),
        1..=3 => ".".repeat(width),
        _ => {
            let mut truncated = text.chars().take(width - 3).collect::<String>();
            truncated.push_str("...");
            truncated
        }
    }
}

fn model_catalog_source_label(source: &model_catalog::ModelCatalogSource) -> &'static str {
    match source {
        model_catalog::ModelCatalogSource::Fresh => "(OpenRouter)",
        model_catalog::ModelCatalogSource::Cache => "(cache)",
        model_catalog::ModelCatalogSource::StaleCache => "(stale cache)",
        model_catalog::ModelCatalogSource::Fallback => "(fallback)",
    }
}

fn format_context_length(context_length: u64) -> String {
    if context_length >= 1_000_000 {
        format!("{}m", context_length / 1_000_000)
    } else if context_length >= 1_000 {
        format!("{}k", context_length / 1_000)
    } else {
        context_length.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerOutcome {
    Changed,
    Submit(String),
    Cancel,
    Unchanged,
}

#[derive(Debug, Clone)]
struct ResumeSessionV2 {
    path: PathBuf,
    date: String,
    question: String,
}

#[derive(Debug, Clone)]
enum Interaction {
    Editor(UnifiedEditor),
    Config(PickerState),
    Models(PickerState),
    Resume(PickerState, Vec<ResumeSessionV2>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRecordV2 {
    input: String,
    output: String,
    status_code: Option<i32>,
}

struct Application {
    transcript: Vec<RenderLine>,
    screen_cache: VirtualScreen,
    cached_transcript_lines: usize,
    interaction: Interaction,
    history: Vec<HistoryEntry>,
    history_path: Option<PathBuf>,
    command_records: Vec<CommandRecordV2>,
    config: AgentConfig,
    config_path: PathBuf,
    agent: Agent,
    logger: AppLogger,
    shell_path: PathBuf,
    shell_env: Vec<(String, String)>,
    working_dir: Option<PathBuf>,
    shell_session: Option<PersistentShellSession>,
    last_shell_command: Option<ShellCommandContext>,
    active_draft: Option<(usize, HistoryKind, HistoryMode)>,
    exit_requested: bool,
    physical_invalidated: bool,
    last_output_streamed: bool,
    last_command_status: i32,
    pending_command_log: Option<String>,
}

impl Application {
    fn new() -> io::Result<Self> {
        let init = AgentConfig::load_or_create_default()?;
        cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
        let logger = AppLogger::start_session()?;
        let agent = Agent::new(init.config.clone()).with_logger(logger.clone());
        let working_dir = env::current_dir().ok();
        let history_path = home_dir().map(|home| {
            home.join(".theseus")
                .join("persist")
                .join("history_command_v2.json")
        });
        let history = history_path
            .as_deref()
            .and_then(|path| load_history(path).ok())
            .unwrap_or_default();
        let prompt = shell_prompt(working_dir.as_deref());
        let interaction = Interaction::Editor(UnifiedEditor::command(prompt, &history));
        let shell_path = default_shell_path();
        let mut app = Self {
            transcript: Vec::new(),
            screen_cache: VirtualScreen::new(),
            cached_transcript_lines: 0,
            interaction,
            history,
            history_path,
            command_records: Vec::new(),
            config: init.config,
            config_path: init.path,
            agent,
            logger,
            shell_path,
            shell_env: vec![("THESEUS_ACTIVE".to_string(), "1".to_string())],
            working_dir,
            shell_session: None,
            last_shell_command: None,
            active_draft: None,
            exit_requested: false,
            physical_invalidated: false,
            last_output_streamed: false,
            last_command_status: 0,
            pending_command_log: None,
        };
        app.append_text(&common::info::render_info());
        Ok(app)
    }

    fn screen(&mut self, terminal_size: TerminalSize) -> &VirtualScreen {
        let mut dirty_from = self.cached_transcript_lines.min(self.transcript.len());
        if self.cached_transcript_lines > self.transcript.len() {
            self.screen_cache.truncate(0);
            self.cached_transcript_lines = 0;
            dirty_from = 0;
        } else {
            self.screen_cache.truncate(self.cached_transcript_lines);
        }
        for line in &self.transcript[self.cached_transcript_lines..] {
            self.screen_cache.push_render_line(line);
        }
        self.cached_transcript_lines = self.transcript.len();
        let base = self.cached_transcript_lines;
        let (mut active_lines, cursor, cursor_visible) = match &mut self.interaction {
            Interaction::Editor(editor) => {
                let (lines, cursor) = editor.render_lines();
                (lines, cursor, !editor.history_is_browsing())
            }
            Interaction::Config(picker)
            | Interaction::Models(picker)
            | Interaction::Resume(picker, _) => picker.render_lines(
                terminal_size.height.saturating_sub(5).min(16),
                terminal_size.width,
            ),
        };
        style_interaction_lines(
            &self.interaction,
            &mut active_lines,
            &self.config.shell_settings.shell_highlight,
        );
        for line in &active_lines {
            self.screen_cache.push_render_line(line);
        }
        if self.screen_cache.lines.is_empty() {
            self.screen_cache.push_render_line(&RenderLine::plain(""));
        }
        self.screen_cache.cursor = VirtualCursor {
            line: base + cursor.line,
            char_offset: cursor.char_offset,
        };
        self.screen_cache.cursor_visible = cursor_visible;
        self.screen_cache.dirty_from = dirty_from;
        &self.screen_cache
    }

    fn handle_event(&mut self, event: Event) -> io::Result<bool> {
        if matches!(event, Event::Resize(_, _)) {
            return Ok(true);
        }
        let Event::Key(key) = &event else {
            let outcome = match &mut self.interaction {
                Interaction::Editor(editor) => Some(editor.handle_event(event)),
                _ => None,
            };
            if let Some(outcome) = outcome {
                return self.finish_editor_outcome(outcome);
            }
            return Ok(false);
        };
        if !is_key_action(key.kind) {
            return Ok(false);
        }

        match &mut self.interaction {
            Interaction::Editor(editor) => {
                let outcome = editor.handle_event(event);
                self.finish_editor_outcome(outcome)
            }
            Interaction::Config(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_config_picker(outcome)
            }
            Interaction::Models(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_model_picker(outcome)
            }
            Interaction::Resume(picker, _) => {
                let outcome = picker.handle_key(*key);
                self.finish_resume_picker(outcome)
            }
        }
    }

    fn finish_editor_outcome(&mut self, outcome: EditorOutcome) -> io::Result<bool> {
        match outcome {
            EditorOutcome::Submit(submission) => {
                let submission_kind = submission.kind;
                self.commit_submission(&submission);
                self.execute_submission(submission)?;
                if submission_kind != SubmissionKind::Command {
                    self.finish_pending_command_log();
                }
                Ok(true)
            }
            EditorOutcome::OpenMultiline(mode, text) => {
                let recalled_command = match mode {
                    EditorMode::Ask => "/ask",
                    EditorMode::Shell => "/shell",
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.commit_submission(&EditorSubmission {
                    kind: SubmissionKind::Command,
                    text: recalled_command.to_string(),
                });
                let (kind, history_mode, guidance) = match mode {
                    EditorMode::Ask => (
                        HistoryKind::Agent,
                        HistoryMode::MultiLineAsk,
                        format!(
                            "Enter multiline input. Type {} on a new line to finish.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    EditorMode::Shell => (
                        HistoryKind::Shell,
                        HistoryMode::MultiLineShell,
                        format!(
                            "Enter multiline shell command. Type {} on a new line to run.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.append_text(&guidance);
                self.interaction = Interaction::Editor(match mode {
                    EditorMode::Ask => UnifiedEditor::ask(Some(text), &self.history),
                    EditorMode::Shell => UnifiedEditor::shell(Some(text), &self.history),
                    _ => unreachable!(),
                });
                self.active_draft = Some((self.history.len(), kind, history_mode));
                Ok(true)
            }
            EditorOutcome::Cancel => {
                let cancelled_submission = match &self.interaction {
                    Interaction::Editor(editor) => Some(EditorSubmission {
                        kind: match editor.mode {
                            EditorMode::Command => SubmissionKind::Command,
                            EditorMode::Ask => SubmissionKind::Ask,
                            EditorMode::Shell => SubmissionKind::Shell,
                            EditorMode::ApiKey => SubmissionKind::ApiKey,
                        },
                        text: editor.buffer.text(),
                    }),
                    _ => None,
                };
                let cancelled_kind = cancelled_submission
                    .as_ref()
                    .map(|submission| submission.kind);
                if let Some(submission) = cancelled_submission {
                    self.commit_submission(&submission);
                }
                match cancelled_kind {
                    Some(SubmissionKind::Command) => {
                        self.append_text("Interrupted. Type /exit to exit the shell.\n");
                    }
                    Some(SubmissionKind::Ask) => self.append_text("\nAsk cancelled.\n"),
                    Some(SubmissionKind::Shell) => self.append_text("\nShell cancelled.\n"),
                    Some(SubmissionKind::ApiKey) => self.append_text("Config cancelled.\n"),
                    None => self.append_text("Cancelled.\n"),
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            EditorOutcome::Exit => {
                self.exit_requested = true;
                Ok(true)
            }
            EditorOutcome::Redraw => {
                self.physical_invalidated = true;
                Ok(true)
            }
            EditorOutcome::Changed => {
                self.sync_multiline_draft();
                Ok(true)
            }
            EditorOutcome::Unchanged => Ok(false),
        }
    }

    fn commit_submission(&mut self, submission: &EditorSubmission) {
        let (prompt, continuation) = match submission.kind {
            SubmissionKind::Command => (
                shell_prompt(self.working_dir.as_deref()),
                input::DEFAULT_COMMAND_CONTINUATION_PROMPT.to_string(),
            ),
            SubmissionKind::Ask | SubmissionKind::Shell => (
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
            ),
            SubmissionKind::ApiKey => ("Openrouter API key: ".to_string(), String::new()),
        };
        let mut committed = Vec::new();
        for (index, line) in submission.text.split('\n').enumerate() {
            let shown = if submission.kind == SubmissionKind::ApiKey {
                "*".repeat(char_len(line))
            } else {
                line.to_string()
            };
            let render_line = RenderLine::new(
                if index == 0 {
                    prompt.clone()
                } else {
                    continuation.clone()
                },
                shown,
            );
            committed.push(render_line);
        }
        if matches!(
            submission.kind,
            SubmissionKind::Command | SubmissionKind::Shell
        ) {
            style_shell_lines(
                &mut committed,
                &submission.text,
                &self.config.shell_settings.shell_highlight,
            );
        }
        for line in &mut committed {
            style_prompt(line);
        }
        self.transcript.extend(committed);
    }

    fn execute_submission(&mut self, submission: EditorSubmission) -> io::Result<()> {
        match submission.kind {
            SubmissionKind::Command => self.execute_command(&submission.text),
            SubmissionKind::Ask => {
                let text = submission.text.trim();
                if !text.is_empty() {
                    self.store_history(HistoryEntry {
                        text: text.to_string(),
                        kind: HistoryKind::Agent,
                        mode: HistoryMode::MultiLineAsk,
                    });
                    self.run_agent(text)?;
                }
                self.return_to_command_editor();
                Ok(())
            }
            SubmissionKind::Shell => {
                let text = submission.text.trim();
                if !text.is_empty() {
                    self.store_history(HistoryEntry {
                        text: text.to_string(),
                        kind: HistoryKind::Shell,
                        mode: HistoryMode::MultiLineShell,
                    });
                    self.run_shell(text)?;
                }
                self.return_to_command_editor();
                Ok(())
            }
            SubmissionKind::ApiKey => {
                let key = submission.text.trim();
                let authorization = if key.starts_with("Bearer ") {
                    key.to_string()
                } else {
                    format!("Bearer {key}")
                };
                self.save_config_patch(ConfigPatch::SetAuthorization(authorization), false)?;
                self.append_text(&format!("Config saved to {}\n", self.config_path.display()));
                self.return_to_command_editor();
                Ok(())
            }
        }
    }

    fn execute_command(&mut self, input: &str) -> io::Result<()> {
        let _ = self.logger.event(
            "info",
            "command_start",
            json!({
                "input": input,
                "renderer": "diff_v2",
            }),
        );
        let result = self.execute_command_inner(input);
        if result.is_ok() && interaction_needs_input(&self.interaction) {
            self.pending_command_log = Some(input.to_string());
        } else {
            self.log_command_finish(input, result.as_ref().err());
        }
        result
    }

    fn log_command_finish(&self, input: &str, error: Option<&io::Error>) {
        let _ = self.logger.event(
            if error.is_none() && self.last_command_status == 0 {
                "info"
            } else {
                "error"
            },
            "command_finish",
            json!({
                "input": input,
                "renderer": "diff_v2",
                "status_code": self.last_command_status,
                "error": error.map(ToString::to_string),
            }),
        );
    }

    fn finish_pending_command_log(&mut self) {
        if let Some(input) = self.pending_command_log.take() {
            self.log_command_finish(&input, None);
        }
    }

    fn execute_command_inner(&mut self, input: &str) -> io::Result<()> {
        self.last_output_streamed = false;
        self.last_command_status = 0;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            self.return_to_command_editor();
            return Ok(());
        }
        match parse_slash_command(trimmed) {
            Some(SlashCommand::Exit) => self.exit_requested = true,
            Some(SlashCommand::Help) => {
                self.last_shell_command = None;
                self.append_text(&common::info::render_info());
            }
            Some(SlashCommand::Status) => {
                self.last_shell_command = None;
                self.append_markdown(&self.agent.status_text());
            }
            Some(SlashCommand::Mcp) => {
                self.last_shell_command = None;
                let status = self.agent.mcp_status_text();
                if status.is_empty() {
                    self.append_text("No MCP servers configured.\n");
                } else {
                    self.append_markdown(&status);
                }
            }
            Some(SlashCommand::Reset) => {
                self.last_shell_command = None;
                self.reset_agent()?;
            }
            Some(SlashCommand::Compact) => {
                self.last_shell_command = None;
                self.compact_agent()?;
            }
            Some(SlashCommand::Config) => {
                self.last_shell_command = None;
                self.store_special_history(trimmed);
                self.open_config();
                return Ok(());
            }
            Some(SlashCommand::Resume) => {
                self.last_shell_command = None;
                self.store_special_history(trimmed);
                self.open_resume()?;
                return Ok(());
            }
            Some(SlashCommand::Ask) => {
                let rest = trimmed.strip_prefix("/ask").unwrap_or_default().trim();
                if rest.is_empty() {
                    self.interaction = Interaction::Editor(UnifiedEditor::ask(None, &self.history));
                    self.active_draft = Some((
                        self.history.len(),
                        HistoryKind::Agent,
                        HistoryMode::MultiLineAsk,
                    ));
                    self.append_text(&format!(
                        "Enter multiline input. Type {} on a new line to finish.\n",
                        input::MULTILINE_SUBMIT_COMMAND
                    ));
                    return Ok(());
                }
                self.store_history(HistoryEntry {
                    text: rest.to_string(),
                    kind: HistoryKind::Agent,
                    mode: HistoryMode::SingleLineAsk,
                });
                self.run_agent(rest)?;
                shell_terminal_v2::discard_pending_terminal_input()?;
            }
            Some(SlashCommand::Shell) => {
                let rest = trimmed.strip_prefix("/shell").unwrap_or_default().trim();
                if rest.is_empty() {
                    self.interaction =
                        Interaction::Editor(UnifiedEditor::shell(None, &self.history));
                    self.active_draft = Some((
                        self.history.len(),
                        HistoryKind::Shell,
                        HistoryMode::MultiLineShell,
                    ));
                    self.append_text(&format!(
                        "Enter multiline shell command. Type {} on a new line to run.\n",
                        input::MULTILINE_SUBMIT_COMMAND
                    ));
                    return Ok(());
                }
                self.store_history(HistoryEntry {
                    text: rest.to_string(),
                    kind: HistoryKind::Shell,
                    mode: HistoryMode::SingleLine,
                });
                self.run_shell(rest)?;
            }
            Some(SlashCommand::History) => {
                self.last_shell_command = None;
                self.append_text(&self.formatted_history());
            }
            None if trimmed == "exit" => self.exit_requested = true,
            None if classify_command(trimmed, self.working_dir.as_deref())
                == CommandRoute::Agent =>
            {
                self.store_history(HistoryEntry {
                    text: trimmed.to_string(),
                    kind: HistoryKind::Agent,
                    mode: HistoryMode::SingleLine,
                });
                self.run_agent(trimmed)?;
            }
            None => {
                self.store_history(HistoryEntry {
                    text: trimmed.to_string(),
                    kind: HistoryKind::Shell,
                    mode: HistoryMode::SingleLine,
                });
                self.run_shell(trimmed)?;
            }
        }
        if !matches!(
            parse_slash_command(trimmed),
            Some(SlashCommand::Ask | SlashCommand::Shell)
        ) {
            self.store_special_history_if_needed(trimmed);
        }
        if !self.exit_requested {
            self.return_to_command_editor();
        }
        Ok(())
    }

    fn run_shell(&mut self, command: &str) -> io::Result<()> {
        if self.shell_session.is_none() {
            self.shell_session = Some(PersistentShellSession::start(PersistentShellConfig {
                shell: self.shell_path.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
            })?);
        }
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let session = self.shell_session.as_mut().expect("shell initialized");
        let output = session.run_command(command)?;
        self.last_output_streamed = output.streamed;
        self.last_command_status = output.status_code.unwrap_or(1);
        if let Ok(working_dir) = session.current_working_dir()
            && env::set_current_dir(&working_dir).is_ok()
        {
            self.working_dir = Some(working_dir);
        }
        let text = output.transcript_lossy();
        if !output.streamed || !uses_alternate_screen(&text) {
            if let Some(visible_suffix) = suffix_after_last_display_clear(&text) {
                // The streamed terminal has already discarded everything that
                // preceded ED 2. Mirror that state transition in the
                // persistent virtual scene so the recovery frame cannot bring
                // the old transcript back.
                self.transcript.clear();
                self.append_text(visible_suffix);
            } else {
                self.append_text(&text);
            }
        }
        self.last_shell_command = Some(ShellCommandContext {
            command: command.to_string(),
            output: common::text::truncate_utf8_to_bytes(
                &text,
                MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES,
                common::text::TruncatePosition::End,
            ),
        });
        self.command_records.push(CommandRecordV2 {
            input: command.to_string(),
            output: text,
            status_code: output.status_code,
        });
        Ok(())
    }

    fn run_agent(&mut self, prompt: &str) -> io::Result<()> {
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let last_shell_command = self.last_shell_command.take();
        common::cancellation::clear_sigint_request();
        let output = self.agent.run_with_context(
            prompt,
            AgentRunContext {
                shell: self.shell_path.clone(),
                shell_prompt: shell_prompt(self.working_dir.as_deref()),
                shell_highlight: self.config.shell_settings.shell_highlight.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
                last_shell_command,
                logger: Some(self.logger.clone()),
                ..AgentRunContext::default()
            },
        );
        match output {
            Ok(output) => {
                self.last_command_status = 0;
                let rendered = render_markdown_v2(&output);
                self.append_text(&pad_agent_answer(&rendered));
                self.command_records.push(CommandRecordV2 {
                    input: prompt.to_string(),
                    output,
                    status_code: Some(0),
                });
            }
            Err(error) => {
                self.last_command_status = 1;
                self.append_text(&pad_agent_answer(&format!("agent: {error}\n")));
                self.command_records.push(CommandRecordV2 {
                    input: prompt.to_string(),
                    output: error.to_string(),
                    status_code: Some(1),
                });
            }
        }
        Ok(())
    }

    fn reset_agent(&mut self) -> io::Result<()> {
        let init = AgentConfig::load_or_create_at(self.config_path.clone())?;
        self.config = init.config;
        self.logger = AppLogger::start_session()?;
        self.agent = Agent::new(self.config.clone()).with_logger(self.logger.clone());
        self.append_text("Agent context has been reset.\n");
        Ok(())
    }

    fn compact_agent(&mut self) -> io::Result<()> {
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let message = match self.agent.compact_context() {
            Ok(CompactOutcome::AlreadyMinimal) => "Agent context is already minimal.\n".to_string(),
            Ok(CompactOutcome::MissingAuthorization) => {
                "LLM Authorization header is empty. Run /config first.\n".to_string()
            }
            Ok(CompactOutcome::Compacted(result)) => {
                self.logger = AppLogger::start_session()?;
                self.agent.set_logger(self.logger.clone());
                self.agent.log_event(
                    "info",
                    "agent_compact_finish",
                    json!({
                        "previous_log_path": result.previous_log_path,
                        "previous_trajectory_path": result.previous_trajectory_path,
                        "new_log_path": self.logger.log_path(),
                        "new_trajectory_path": self.logger.trajectory_path(),
                        "messages_before": result.before_messages,
                        "messages_after": result.after_messages,
                        "compact_trim_retries": result.compact_trim_retries,
                        "recent_user_messages": result.recent_user_messages,
                    }),
                );
                format!(
                    "Agent context compacted: {} -> {} messages. New trajectory: {}.\n",
                    result.before_messages,
                    result.after_messages,
                    self.logger.trajectory_path().display()
                )
            }
            Err(error) => {
                self.last_command_status = 1;
                format!("agent: {error}\n")
            }
        };
        self.append_text(&message);
        Ok(())
    }

    fn open_config(&mut self) {
        if !self
            .config
            .llm_request_settings
            .base_url
            .contains("openrouter.ai")
        {
            self.append_text(
                "Warning: /config updates OpenRouter-like fields, but base_url is not an OpenRouter endpoint.\n",
            );
        }
        let items = vec![
            PickerItem {
                id: "model".to_string(),
                label: "1. Change model".to_string(),
                detail: "Select a different model".to_string(),
            },
            PickerItem {
                id: "api_key".to_string(),
                label: "2. Set OpenRouter API key".to_string(),
                detail: "Update the API key".to_string(),
            },
        ];
        self.interaction = Interaction::Config(PickerState::new(
            "What would you like to configure?",
            items,
            false,
        ));
    }

    fn finish_config_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) if id == "model" => {
                let catalog = model_catalog::load_openrouter_models();
                let title = format!(
                    "Select model {}",
                    model_catalog_source_label(&catalog.source)
                );
                let current = self
                    .config
                    .llm_request_settings
                    .body
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let items = catalog
                    .models
                    .into_iter()
                    .map(|model| {
                        let is_current = current.as_deref() == Some(model.id.as_str());
                        let context = model
                            .context_length
                            .map(format_context_length)
                            .unwrap_or_else(|| "n/a".to_string());
                        PickerItem {
                            label: format!(
                                "{}{}",
                                model.id,
                                if is_current { " (current)" } else { "" }
                            ),
                            id: model.id,
                            detail: model.name.map_or_else(
                                || format!("ctx: {context}"),
                                |name| format!("ctx: {context}  {name}"),
                            ),
                        }
                    })
                    .collect();
                self.interaction = Interaction::Models(
                    PickerState::new(title, items, true).with_selected_id(current.as_deref()),
                );
                Ok(true)
            }
            PickerOutcome::Submit(id) if id == "api_key" => {
                self.interaction = Interaction::Editor(UnifiedEditor::api_key());
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Config cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn finish_model_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(model) => {
                let model_changed = self
                    .config
                    .llm_request_settings
                    .body
                    .get("model")
                    .and_then(Value::as_str)
                    != Some(model.as_str());
                self.save_config_patch(ConfigPatch::SetModel(model), model_changed)?;
                self.append_text(&format!("Config saved to {}\n", self.config_path.display()));
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Config cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn save_config_patch(&mut self, patch: ConfigPatch, model_changed: bool) -> io::Result<()> {
        if self.config_path.exists() {
            self.config = patch_config_jsonc_file(&self.config_path, patch)?;
        } else {
            match patch {
                ConfigPatch::SetModel(model) => {
                    self.config
                        .llm_request_settings
                        .body
                        .insert("model".to_string(), json!(model));
                }
                ConfigPatch::SetAuthorization(authorization) => {
                    self.config
                        .llm_request_settings
                        .header
                        .insert("Authorization".to_string(), authorization);
                }
            }
            self.config.save_at(&self.config_path)?;
        }
        if model_changed {
            self.logger = AppLogger::start_session()?;
        }
        self.agent = Agent::new(self.config.clone()).with_logger(self.logger.clone());
        Ok(())
    }

    fn open_resume(&mut self) -> io::Result<()> {
        let sessions = resume_sessions(self.agent.max_resume_traj())?;
        if sessions.is_empty() {
            self.append_text("No resumable sessions found.\n");
            self.return_to_command_editor();
            return Ok(());
        }
        let items = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| PickerItem {
                id: index.to_string(),
                label: session.date.clone(),
                detail: truncate_for_width(
                    &session
                        .question
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    96,
                ),
            })
            .collect();
        self.interaction =
            Interaction::Resume(PickerState::new("Resume session", items, true), sessions);
        Ok(())
    }

    fn finish_resume_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) => {
                let index = id.parse::<usize>().ok();
                let session = match (&self.interaction, index) {
                    (Interaction::Resume(_, sessions), Some(index)) => sessions.get(index).cloned(),
                    _ => None,
                };
                if let Some(session) = session {
                    let count = self.agent.resume_trajectory_from_path(&session.path)?;
                    self.append_text(&format!(
                        "Resumed session from {} ({count} messages).\n",
                        session.path.display()
                    ));
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Resume cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn return_to_command_editor(&mut self) {
        self.active_draft = None;
        let prompt = shell_prompt(self.working_dir.as_deref());
        self.interaction = Interaction::Editor(UnifiedEditor::command(prompt, &self.history));
    }

    fn sync_multiline_draft(&mut self) {
        let Some((mut slot, kind, mode)) = self.active_draft else {
            return;
        };
        let text = match &self.interaction {
            Interaction::Editor(editor)
                if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell) =>
            {
                editor.buffer.text().trim().to_string()
            }
            _ => return,
        };

        self.history.truncate(slot.min(self.history.len()));
        if text.is_empty() {
            self.active_draft = Some((self.history.len(), kind, mode));
        } else {
            let entry = HistoryEntry { text, kind, mode };
            self.history.retain(|existing| existing != &entry);
            self.history.push(entry);
            if self.history.len() > MAX_PERSISTED_HISTORY {
                let removed = self.history.len() - MAX_PERSISTED_HISTORY;
                self.history.drain(..removed);
            }
            slot = self.history.len().saturating_sub(1);
            self.active_draft = Some((slot, kind, mode));
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    fn append_text(&mut self, text: &str) {
        self.transcript.extend(ansi_render_lines(text));
    }

    fn append_markdown(&mut self, text: &str) {
        self.transcript
            .extend(ansi_render_lines(&render_markdown_v2(text)));
    }

    fn store_special_history_if_needed(&mut self, input: &str) {
        if parse_slash_command(input).is_some() {
            self.store_special_history(input);
        }
    }

    fn store_special_history(&mut self, input: &str) {
        self.store_history(HistoryEntry {
            text: input.to_string(),
            kind: HistoryKind::Special,
            mode: HistoryMode::SingleLine,
        });
    }

    fn store_history(&mut self, mut entry: HistoryEntry) {
        entry.text = entry.text.trim().to_string();
        if entry.text.is_empty() {
            return;
        }
        self.history.retain(|existing| existing != &entry);
        self.history.push(entry);
        if self.history.len() > MAX_PERSISTED_HISTORY {
            self.history
                .drain(..self.history.len() - MAX_PERSISTED_HISTORY);
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    fn formatted_history(&self) -> String {
        if self.command_records.is_empty() {
            return String::new();
        }
        self.command_records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let status = record
                    .status_code
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                format!(
                    "{}  {}  [status: {}]\noutput:\n{}",
                    index + 1,
                    record.input,
                    status,
                    record.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn take_physical_invalidation(&mut self) -> bool {
        std::mem::take(&mut self.physical_invalidated)
    }
}

struct ExternalTerminalGuard {
    was_raw: bool,
}

impl ExternalTerminalGuard {
    fn enter() -> io::Result<Self> {
        let was_raw = crossterm::terminal::is_raw_mode_enabled()?;
        if !was_raw {
            return Ok(Self { was_raw });
        }
        let mut stdout = io::stdout();
        execute!(stdout, DisableBracketedPaste, Show)?;
        write!(stdout, "\r\n")?;
        stdout.flush()?;
        disable_raw_mode()?;
        Ok(Self { was_raw })
    }
}

impl Drop for ExternalTerminalGuard {
    fn drop(&mut self) {
        if !self.was_raw {
            return;
        }
        let _ = enable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, EnableBracketedPaste);
    }
}

fn run_application(args: Vec<String>) -> io::Result<i32> {
    common::cancellation::install_sigint_handler();
    let mut app = Application::new()?;
    if !args.is_empty() {
        let command = args.join(" ");
        print!("{}", common::info::render_info());
        io::stdout().flush()?;
        app.transcript.clear();
        app.execute_command(&command)?;
        if interaction_needs_input(&app.interaction) {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                let mut transcript = ansi_render_lines(&common::info::render_info());
                transcript.append(&mut app.transcript);
                app.transcript = transcript;
                return run_interactive_application(app, true);
            }
            print_plain_transcript(&app.transcript, true);
            io::stdout().flush()?;
            app.transcript.clear();
            let stdin = io::stdin();
            let mut input = stdin.lock();
            finish_plain_interactions(&mut app, &mut input)?;
        }
        if !app.last_output_streamed {
            print_plain_transcript(&app.transcript, true);
        }
        return Ok(app.last_command_status);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return run_plain_application(app);
    }

    run_interactive_application(app, false)
}

fn run_interactive_application(
    mut app: Application,
    exit_when_command_editor_returns: bool,
) -> io::Result<i32> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    let mut renderer = DiffRenderer::new();
    let mut handled_event = false;

    loop {
        if app.exit_requested {
            return Ok(app.last_command_status);
        }
        let (width, height) = size()?;
        let terminal_size = TerminalSize::new(width, height);
        renderer.render(&mut stdout, app.screen(terminal_size), terminal_size)?;
        if exit_when_command_editor_returns
            && handled_event
            && !interaction_needs_input(&app.interaction)
        {
            return Ok(app.last_command_status);
        }
        let event = read()?;
        handled_event = true;
        if app.handle_event(event)? && app.take_physical_invalidation() {
            renderer.invalidate();
        }
    }
}

fn run_plain_application(mut app: Application) -> io::Result<i32> {
    print_plain_transcript(&app.transcript, false);
    io::stdout().flush()?;
    app.transcript.clear();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        if interaction_needs_input(&app.interaction) {
            finish_plain_interactions(&mut app, &mut input)?;
            flush_plain_application_transcript(&mut app)?;
            continue;
        }
        let Some(line) = read_plain_line(&mut input)? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        app.execute_command(&line)?;
        flush_plain_application_transcript(&mut app)?;
        if app.exit_requested {
            break;
        }
    }
    Ok(app.last_command_status)
}

fn flush_plain_application_transcript(app: &mut Application) -> io::Result<()> {
    print_plain_transcript(&app.transcript, false);
    io::stdout().flush()?;
    app.transcript.clear();
    Ok(())
}

fn interaction_needs_input(interaction: &Interaction) -> bool {
    !matches!(
        interaction,
        Interaction::Editor(UnifiedEditor {
            mode: EditorMode::Command,
            ..
        })
    )
}

fn finish_plain_interactions(app: &mut Application, input: &mut impl BufRead) -> io::Result<()> {
    while interaction_needs_input(&app.interaction) {
        match &app.interaction {
            Interaction::Config(picker) => {
                println!("{}", picker.title);
                for item in &picker.items {
                    println!("{} — {}", item.label, item.detail);
                }
                let selected = read_plain_line(input)?
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .and_then(|index| picker.items.get(index.saturating_sub(1)))
                    .or_else(|| picker.items.first())
                    .map(|item| item.id.clone());
                app.finish_config_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Models(picker) => {
                let query = read_plain_line(input)?.unwrap_or_default();
                let selected = picker
                    .items
                    .iter()
                    .find(|item| item.id == query.trim())
                    .or_else(|| {
                        let terms = query
                            .split_whitespace()
                            .map(str::to_ascii_lowercase)
                            .collect::<Vec<_>>();
                        picker.items.iter().find(|item| {
                            let haystack =
                                format!("{} {}", item.id, item.label).to_ascii_lowercase();
                            terms.iter().all(|term| haystack.contains(term))
                        })
                    })
                    .or_else(|| picker.selected_item())
                    .map(|item| item.id.clone());
                app.finish_model_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Resume(picker, _) => {
                println!("{}", picker.title);
                for (index, item) in picker.items.iter().enumerate() {
                    println!("{}. {} — {}", index + 1, item.label, item.detail);
                }
                let selected = read_plain_line(input)?
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .and_then(|index| picker.items.get(index.saturating_sub(1)))
                    .or_else(|| picker.items.first())
                    .map(|item| item.id.clone());
                app.finish_resume_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Editor(editor) => match editor.mode {
                EditorMode::ApiKey => {
                    if let Some(text) = read_plain_line(input)? {
                        app.execute_submission(EditorSubmission {
                            kind: SubmissionKind::ApiKey,
                            text,
                        })?;
                    } else {
                        app.append_text("Config cancelled.\n");
                        app.return_to_command_editor();
                        app.finish_pending_command_log();
                    }
                }
                EditorMode::Ask | EditorMode::Shell => {
                    let mut lines = Vec::new();
                    while let Some(line) = read_plain_line(input)? {
                        if line.trim() == input::MULTILINE_SUBMIT_COMMAND {
                            break;
                        }
                        lines.push(line);
                    }
                    app.execute_submission(EditorSubmission {
                        kind: if editor.mode == EditorMode::Ask {
                            SubmissionKind::Ask
                        } else {
                            SubmissionKind::Shell
                        },
                        text: lines.join("\n"),
                    })?;
                }
                EditorMode::Command => break,
            },
        }
    }
    Ok(())
}

fn read_plain_line(input: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

fn print_plain_transcript(lines: &[RenderLine], only_unprefixed: bool) {
    for line in lines {
        if !only_unprefixed || line.prefix.is_empty() {
            println!(
                "{}{}",
                if only_unprefixed {
                    String::new()
                } else {
                    terminal_styled_text(&line.prefix, &line.prefix_styles)
                },
                terminal_styled_text(&line.text, &line.styles)
            );
        }
    }
}

fn terminal_styled_text(text: &str, styles: &[CellStyle]) -> String {
    let mut output = String::new();
    let mut current = CellStyle::default();
    for (index, ch) in text.chars().enumerate() {
        let style = styles.get(index).copied().unwrap_or_default();
        if style != current {
            push_style_escape(&mut output, style);
            current = style;
        }
        output.push(ch);
    }
    if current != CellStyle::default() {
        push_style_escape(&mut output, CellStyle::default());
    }
    output
}

fn run_headless(prompt: &str) -> io::Result<i32> {
    let init = AgentConfig::load_or_create_default()?;
    cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
    let logger = AppLogger::start_session()?;
    let mut agent = Agent::new(init.config).with_logger(logger.clone());
    match agent.run_with_context(
        prompt,
        AgentRunContext {
            logger: Some(logger),
            ..AgentRunContext::default()
        },
    ) {
        Ok(output) => {
            print!("{output}");
            io::stdout().flush()?;
            Ok(0)
        }
        Err(error) => {
            eprintln!("theseus: agent run failed: {error}");
            Ok(1)
        }
    }
}

fn default_shell_path() -> PathBuf {
    #[cfg(unix)]
    {
        env::var_os("SHELL")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
    #[cfg(windows)]
    {
        env::var_os("COMSPEC")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe"))
    }
}

fn shell_prompt(working_dir: Option<&Path>) -> String {
    let user = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "user".to_string());
    let directory = working_dir
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("theseus");
    format!("{user} {directory}> ")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn load_history(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let history: Vec<HistoryEntry> =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    Ok(normalize_history(history))
}

fn normalize_history(history: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut history = history
        .into_iter()
        .filter_map(|mut entry| {
            entry.text = entry.text.trim().to_string();
            (!entry.text.is_empty()).then_some(entry)
        })
        .collect::<Vec<_>>();
    let mut seen = HashSet::with_capacity(history.len());
    history.reverse();
    history.retain(|entry| seen.insert(entry.clone()));
    history.reverse();
    if history.len() > MAX_PERSISTED_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_HISTORY);
    }
    history
}

fn save_history(path: &Path, history: &[HistoryEntry]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let normalized = normalize_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&normalized).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

fn resume_sessions(limit: usize) -> io::Result<Vec<ResumeSessionV2>> {
    let directory = default_logs_dir()?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_trajectory.json"))
        })
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    Ok(paths
        .into_iter()
        .take(limit)
        .filter_map(|path| resume_session_from_path(&path).ok())
        .collect())
}

fn resume_session_from_path(path: &Path) -> io::Result<ResumeSessionV2> {
    #[derive(Deserialize)]
    struct Snapshot {
        messages: Vec<ResumeMessage>,
    }
    #[derive(Deserialize)]
    struct ResumeMessage {
        role: Option<String>,
        content: Option<Value>,
    }
    let snapshot: Snapshot =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    let question = snapshot
        .messages
        .iter()
        .filter(|message| message.role.as_deref() == Some("user"))
        .filter_map(|message| message.content.as_ref())
        .filter_map(content_value_to_string)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && !text.starts_with("Last shell command:"))
        .next_back()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no user question"))?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-date");
    let timestamp = file.strip_suffix("_trajectory.json").unwrap_or(file);
    let parts = timestamp.split('-').collect::<Vec<_>>();
    let date = if parts.len() == 6 {
        format!(
            "{}-{}-{} {}:{}:{}",
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
        )
    } else {
        timestamp.to_string()
    };
    Ok(ResumeSessionV2 {
        path: path.to_path_buf(),
        date,
        question,
    })
}

fn content_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(content_value_to_string)
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(object) => object
            .get("text")
            .and_then(content_value_to_string)
            .or_else(|| object.get("content").and_then(content_value_to_string)),
        other => Some(other.to_string()),
    }
}

fn pad_agent_answer(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    if !text.starts_with('\n') {
        output.push('\n');
    }
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn ansi_render_lines(text: &str) -> Vec<RenderLine> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut chars = normalized.chars().peekable();
    let mut current_style = CellStyle::default();
    let mut line = String::new();
    let mut styles = Vec::new();
    let mut lines = Vec::new();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    while let Some(next) = chars.next() {
                        if ('@'..='~').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        parameters.push(next);
                    }
                    if final_byte == Some('m') {
                        apply_sgr(&parameters, &mut current_style);
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => {
                lines.push(RenderLine::styled(
                    std::mem::take(&mut line),
                    std::mem::take(&mut styles),
                ));
            }
            '\t' => {
                // Keep the control character in the logical scene. Expanding
                // it here loses the current physical column and cannot match
                // the terminal's native tab stops when streamed PTY output is
                // later reconstructed by the diff renderer.
                line.push('\t');
                styles.push(current_style);
            }
            ch if !ch.is_control() => {
                line.push(ch);
                styles.push(current_style);
            }
            _ => {}
        }
    }
    if !line.is_empty() || !styles.is_empty() || !normalized.ends_with('\n') {
        lines.push(RenderLine::styled(line, styles));
    }
    lines
}

fn apply_sgr(parameters: &str, style: &mut CellStyle) {
    let values = if parameters.is_empty() {
        vec![0]
    } else {
        parameters
            .split(';')
            .filter_map(|value| value.parse::<u16>().ok())
            .collect::<Vec<_>>()
    };
    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = CellStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.reverse = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.reverse = false,
            29 => style.strikethrough = false,
            30..=37 => style.foreground = basic_color(values[index] - 30, false),
            39 => style.foreground = None,
            40..=47 => style.background = basic_color(values[index] - 40, false),
            49 => style.background = None,
            90..=97 => style.foreground = basic_color(values[index] - 90, true),
            100..=107 => style.background = basic_color(values[index] - 100, true),
            38 if values.get(index + 1) == Some(&5) => {
                if let Some(value) = values.get(index + 2) {
                    style.foreground = ansi_256_color(*value);
                    index += 2;
                }
            }
            38 if values.get(index + 1) == Some(&2) => {
                if let (Some(red), Some(green), Some(blue)) = (
                    values.get(index + 2),
                    values.get(index + 3),
                    values.get(index + 4),
                ) {
                    if let (Ok(red), Ok(green), Ok(blue)) = (
                        u8::try_from(*red),
                        u8::try_from(*green),
                        u8::try_from(*blue),
                    ) {
                        style.foreground = Some(TerminalColor::Rgb(red, green, blue));
                    }
                }
                index = (index + 4).min(values.len());
            }
            48 if values.get(index + 1) == Some(&2) => {
                if let (Some(red), Some(green), Some(blue)) = (
                    values.get(index + 2),
                    values.get(index + 3),
                    values.get(index + 4),
                ) && let (Ok(red), Ok(green), Ok(blue)) = (
                    u8::try_from(*red),
                    u8::try_from(*green),
                    u8::try_from(*blue),
                ) {
                    style.background = Some(TerminalColor::Rgb(red, green, blue));
                }
                index = (index + 4).min(values.len());
            }
            48 if values.get(index + 1) == Some(&5) => {
                if let Some(value) = values.get(index + 2) {
                    style.background = ansi_256_color(*value);
                }
                index = (index + 2).min(values.len());
            }
            _ => {}
        }
        index += 1;
    }
}

fn uses_alternate_screen(text: &str) -> bool {
    ["\x1b[?1049h", "\x1b[?47h", "\x1b[?1047h"]
        .iter()
        .any(|sequence| text.contains(sequence))
}

fn suffix_after_last_display_clear(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut suffix_start = None;

    while index + 2 < bytes.len() {
        if bytes[index] != b'\x1b' || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }

        let parameters_start = index + 2;
        let mut final_index = parameters_start;
        while final_index < bytes.len() && !(0x40..=0x7e).contains(&bytes[final_index]) {
            final_index += 1;
        }
        if final_index == bytes.len() {
            break;
        }

        if bytes[final_index] == b'J' && &bytes[parameters_start..final_index] == b"2" {
            suffix_start = Some(final_index + 1);
        }
        index = final_index + 1;
    }

    suffix_start.map(|start| &text[start..])
}

fn basic_color(index: u16, bright: bool) -> Option<TerminalColor> {
    Some(match (index, bright) {
        (0, false) => TerminalColor::Black,
        (1, false) => TerminalColor::Red,
        (2, false) => TerminalColor::Green,
        (3, false) => TerminalColor::Yellow,
        (4, false) => TerminalColor::Blue,
        (5, false) => TerminalColor::Magenta,
        (6, false) => TerminalColor::Cyan,
        (7, false) => TerminalColor::White,
        (0, true) => TerminalColor::BrightBlack,
        (1, true) => TerminalColor::BrightRed,
        (2, true) => TerminalColor::BrightGreen,
        (3, true) => TerminalColor::BrightYellow,
        (4, true) => TerminalColor::BrightBlue,
        (5, true) => TerminalColor::BrightMagenta,
        (6, true) => TerminalColor::BrightCyan,
        (7, true) => TerminalColor::BrightWhite,
        _ => return None,
    })
}

fn ansi_256_color(value: u16) -> Option<TerminalColor> {
    match value {
        0..=7 => basic_color(value, false),
        8..=15 => basic_color(value - 8, true),
        16..=255 => Some(TerminalColor::Indexed(value as u8)),
        _ => None,
    }
}

fn style_interaction_lines(
    interaction: &Interaction,
    lines: &mut [RenderLine],
    palette: &input::ShellHighlightPalette,
) {
    match interaction {
        Interaction::Editor(editor) => {
            let recalled_multiline = editor.history_is_browsing()
                && matches!(
                    editor.recalled_mode,
                    Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
                );
            if recalled_multiline {
                if editor.recalled_mode == Some(HistoryMode::MultiLineShell) && lines.len() > 2 {
                    style_shell_lines(&mut lines[2..], &editor.buffer.text(), palette);
                }
                if let Some(command) = lines.first_mut() {
                    command.style_range(
                        0,
                        char_len(&command.text),
                        CellStyle {
                            foreground: Some(TerminalColor::BrightCyan),
                            ..CellStyle::default()
                        },
                    );
                }
            } else if matches!(editor.mode, EditorMode::Command | EditorMode::Shell) {
                style_shell_lines(lines, &editor.buffer.text(), palette);
            }
            for (index, line) in lines.iter_mut().enumerate() {
                if matches!(editor.mode, EditorMode::Ask)
                    && line.text.trim() == input::MULTILINE_SUBMIT_COMMAND
                {
                    let style = palette_style(palette, "multiline_submit").unwrap_or_default();
                    line.style_range(0, char_len(&line.text), style);
                }
                if editor.history_is_browsing() {
                    if recalled_multiline && index == 1 {
                        line.style_range(
                            0,
                            char_len(&line.text),
                            CellStyle {
                                foreground: Some(TerminalColor::BrightBlack),
                                ..CellStyle::default()
                            },
                        );
                    } else {
                        for style in &mut line.styles {
                            style.italic = true;
                        }
                    }
                }
                style_prompt(line);
            }
        }
        Interaction::Config(_) | Interaction::Models(_) | Interaction::Resume(_, _) => {
            for (index, line) in lines.iter_mut().enumerate() {
                if index == 0 {
                    line.style_range(
                        0,
                        char_len(&line.text),
                        CellStyle {
                            bold: true,
                            ..CellStyle::default()
                        },
                    );
                }
                if line.text.starts_with("> ") {
                    line.style_range(
                        0,
                        char_len(&line.text),
                        CellStyle {
                            foreground: Some(TerminalColor::Cyan),
                            bold: true,
                            ..CellStyle::default()
                        },
                    );
                }
            }
        }
    }
}

fn style_prompt(line: &mut RenderLine) {
    if !line.prefix.ends_with("> ") || line.prefix == input::DEFAULT_MULTILINE_PREFIX {
        return;
    }
    let mut first_word = true;
    let final_marker = char_len(&line.prefix).saturating_sub(2);
    for (index, ch) in line.prefix.chars().enumerate() {
        if ch.is_whitespace() {
            first_word = false;
            continue;
        }
        if index == final_marker && ch == '>' {
            continue;
        }
        line.prefix_styles[index] = CellStyle {
            foreground: Some(if first_word {
                TerminalColor::Cyan
            } else {
                TerminalColor::Magenta
            }),
            bold: true,
            ..CellStyle::default()
        };
    }
}

fn style_shell_lines(
    lines: &mut [RenderLine],
    input_text: &str,
    palette: &input::ShellHighlightPalette,
) {
    if !lines.first().is_some_and(|line| line.text.starts_with('/')) {
        let analysis = shell::input_syntax::analyze_shell_input(input_text);
        let mut spans = analysis.spans.iter().collect::<Vec<_>>();
        spans.sort_by_key(|span| shell_span_priority(&span.kind));
        for span in spans {
            let Some(line) = lines.get_mut(span.row) else {
                continue;
            };
            if span.start > span.end
                || span.end > line.text.len()
                || !line.text.is_char_boundary(span.start)
                || !line.text.is_char_boundary(span.end)
            {
                continue;
            }
            let start = line.text[..span.start].chars().count();
            let end = line.text[..span.end].chars().count();
            line.style_range(
                start,
                end,
                palette_style(palette, shell_span_palette_key(&span.kind)).unwrap_or_default(),
            );
        }
    }

    for line in lines {
        if parse_slash_command(&line.text).is_some() {
            let end = line
                .text
                .chars()
                .position(|ch| ch.is_whitespace())
                .unwrap_or_else(|| char_len(&line.text));
            line.style_range(
                0,
                end,
                CellStyle {
                    foreground: Some(TerminalColor::BrightCyan),
                    ..CellStyle::default()
                },
            );
        }
        if line.text.trim() == input::MULTILINE_SUBMIT_COMMAND {
            line.style_range(
                0,
                char_len(&line.text),
                palette_style(palette, "multiline_submit").unwrap_or_default(),
            );
        }
    }
}

fn shell_span_palette_key(kind: &shell::input_syntax::ShellSpanKind) -> &'static str {
    use shell::input_syntax::ShellSpanKind;
    match kind {
        ShellSpanKind::Command => "command",
        ShellSpanKind::Builtin => "builtin",
        ShellSpanKind::FunctionName => "function_name",
        ShellSpanKind::Keyword => "keyword",
        ShellSpanKind::String => "string",
        ShellSpanKind::StringEscape => "string_escape",
        ShellSpanKind::HeredocBody { quoted: true } => "quoted_heredoc_body",
        ShellSpanKind::HeredocBody { quoted: false } => "heredoc_body",
        ShellSpanKind::Variable => "variable",
        ShellSpanKind::CommandSubstitution => "command_substitution",
        ShellSpanKind::Arithmetic => "arithmetic",
        ShellSpanKind::ProcessSubstitution => "process_substitution",
        ShellSpanKind::HeredocOperator => "heredoc_operator",
        ShellSpanKind::HeredocDelimiter => "heredoc_delimiter",
        ShellSpanKind::Redirection => "redirection",
        ShellSpanKind::Operator => "operator",
        ShellSpanKind::Comment => "comment",
        ShellSpanKind::Option => "option",
        ShellSpanKind::Glob => "glob",
        ShellSpanKind::ArraySyntax => "array_syntax",
        ShellSpanKind::Error => "error",
        ShellSpanKind::Plain => "plain",
    }
}

fn shell_span_priority(kind: &shell::input_syntax::ShellSpanKind) -> u8 {
    use shell::input_syntax::ShellSpanKind;
    match kind {
        ShellSpanKind::Comment => 100,
        ShellSpanKind::Variable
        | ShellSpanKind::CommandSubstitution
        | ShellSpanKind::Arithmetic
        | ShellSpanKind::ProcessSubstitution => 90,
        ShellSpanKind::HeredocOperator | ShellSpanKind::HeredocDelimiter => 80,
        ShellSpanKind::String | ShellSpanKind::StringEscape | ShellSpanKind::HeredocBody { .. } => {
            70
        }
        ShellSpanKind::Keyword => 60,
        ShellSpanKind::Redirection | ShellSpanKind::Operator => 50,
        ShellSpanKind::Option => 40,
        ShellSpanKind::Command | ShellSpanKind::Builtin | ShellSpanKind::FunctionName => 30,
        ShellSpanKind::Glob | ShellSpanKind::ArraySyntax | ShellSpanKind::Error => 20,
        ShellSpanKind::Plain => 0,
    }
}

fn palette_style(palette: &input::ShellHighlightPalette, key: &str) -> Option<CellStyle> {
    let tags = palette.get(key)?.as_ref()?.tags_slice();
    let mut style = CellStyle::default();
    for tag in tags {
        match tag.as_str() {
            "bold" => style.bold = true,
            "dim" => style.dim = true,
            "italic" => style.italic = true,
            "underline" => style.underline = true,
            "black" => style.foreground = Some(TerminalColor::Black),
            "red" => style.foreground = Some(TerminalColor::Red),
            "green" => style.foreground = Some(TerminalColor::Green),
            "yellow" | "orange" => style.foreground = Some(TerminalColor::Yellow),
            "blue" => style.foreground = Some(TerminalColor::Blue),
            "magenta" => style.foreground = Some(TerminalColor::Magenta),
            "cyan" => style.foreground = Some(TerminalColor::Cyan),
            "white" => style.foreground = Some(TerminalColor::White),
            "bright-black" => style.foreground = Some(TerminalColor::BrightBlack),
            "bright-red" => style.foreground = Some(TerminalColor::BrightRed),
            "bright-green" => style.foreground = Some(TerminalColor::BrightGreen),
            "bright-yellow" => style.foreground = Some(TerminalColor::BrightYellow),
            "bright-blue" => style.foreground = Some(TerminalColor::BrightBlue),
            "bright-magenta" => style.foreground = Some(TerminalColor::BrightMagenta),
            "bright-cyan" => style.foreground = Some(TerminalColor::BrightCyan),
            "bright-white" => style.foreground = Some(TerminalColor::BrightWhite),
            _ => {}
        }
    }
    Some(style)
}

fn render_markdown_v2(text: &str) -> String {
    let text = markdown_preprocessor_v2::preprocess_markdown(text);
    let mut skin = termimad::MadSkin::default();
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.object_style.background_color = None;
    let mut rendered = skin.term_text(&text).to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn is_plain_text_key_v2(key: KeyEvent) -> bool {
    if key.code == KeyCode::Char(' ') {
        return !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
    }
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

fn byte_offset(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSize {
    width: usize,
    height: usize,
}

impl TerminalSize {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            height: usize::from(height.max(1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalPosition {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalCursor {
    position: PhysicalPosition,
    visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PhysicalCell {
    Empty,
    Glyph {
        text: String,
        width: usize,
        style: CellStyle,
    },
    Continuation {
        leading_column: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhysicalRow {
    cells: Vec<PhysicalCell>,
}

impl PhysicalRow {
    fn empty(width: usize) -> Self {
        Self {
            cells: vec![PhysicalCell::Empty; width],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhysicalTerminal {
    size: TerminalSize,
    rows: Vec<PhysicalRow>,
    cursor: PhysicalCursor,
    viewport_top: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedLogicalLayout {
    prefix: String,
    text: String,
    prefix_styles: Vec<CellStyle>,
    styles: Vec<CellStyle>,
    cursor_offset: Option<usize>,
    rows: Vec<PhysicalRow>,
    cursor: Option<PhysicalPosition>,
}

impl CachedLogicalLayout {
    fn build(
        prefix: &str,
        text: &str,
        prefix_styles: &[CellStyle],
        styles: &[CellStyle],
        cursor_offset: Option<usize>,
        width: usize,
    ) -> Self {
        let mut builder = LayoutBuilder::new(width);
        builder.write_prompt(prefix, prefix_styles);
        builder.write_line(text, cursor_offset, styles);
        Self {
            prefix: prefix.to_string(),
            text: text.to_string(),
            prefix_styles: prefix_styles.to_vec(),
            styles: styles.to_vec(),
            cursor_offset,
            rows: builder.rows,
            cursor: builder.cursor,
        }
    }

    fn matches(
        &self,
        prefix: &str,
        text: &str,
        prefix_styles: &[CellStyle],
        styles: &[CellStyle],
        cursor_offset: Option<usize>,
    ) -> bool {
        self.prefix == prefix
            && self.text == text
            && self.prefix_styles == prefix_styles
            && self.styles == styles
            && self.cursor_offset == cursor_offset
    }
}

/// Prefix-sum index for logical-line physical heights. Updating one edited
/// line and resolving an absolute viewport row are both logarithmic.
#[derive(Debug, Clone, Default)]
struct HeightIndex {
    values: Vec<usize>,
    tree: Vec<usize>,
}

impl HeightIndex {
    fn rebuild(&mut self, values: impl IntoIterator<Item = usize>) {
        self.values = values.into_iter().collect();
        self.tree = vec![0; self.values.len() + 1];
        for index in 0..self.values.len() {
            let value = self.values[index];
            self.add(index, value as isize);
        }
    }

    fn set(&mut self, index: usize, value: usize) {
        let previous = self.values[index];
        self.values[index] = value;
        self.add(index, value as isize - previous as isize);
    }

    fn add(&mut self, index: usize, delta: isize) {
        let mut tree_index = index + 1;
        while tree_index < self.tree.len() {
            if delta >= 0 {
                self.tree[tree_index] += delta as usize;
            } else {
                self.tree[tree_index] -= (-delta) as usize;
            }
            tree_index += tree_index & tree_index.wrapping_neg();
        }
    }

    /// Sum of values strictly before `end`.
    fn prefix_sum(&self, end: usize) -> usize {
        let mut tree_index = end.min(self.values.len());
        let mut sum = 0;
        while tree_index > 0 {
            sum += self.tree[tree_index];
            tree_index &= tree_index - 1;
        }
        sum
    }

    fn total(&self) -> usize {
        self.prefix_sum(self.values.len())
    }

    fn line_containing_row(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total() {
            return None;
        }
        let mut low = 0;
        let mut high = self.values.len();
        while low < high {
            let middle = (low + high) / 2;
            if self.prefix_sum(middle + 1) <= row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Some((low, row - self.prefix_sum(low)))
    }
}

#[derive(Debug, Clone, Default)]
struct IndexedPhysicalLayout {
    width: usize,
    logical_lines: Vec<CachedLogicalLayout>,
    heights: HeightIndex,
    reflowed_last_frame: usize,
}

impl IndexedPhysicalLayout {
    fn layout(&mut self, screen: &VirtualScreen, size: TerminalSize) -> PhysicalTerminal {
        debug_assert_eq!(screen.lines.len(), screen.prefixes.len());
        let geometry_changed = self.width != size.width;
        if geometry_changed {
            self.width = size.width;
            self.logical_lines.clear();
        }
        self.reflowed_last_frame = 0;

        let old_length = self.logical_lines.len();
        let new_length = screen.lines.len();
        let common_length = old_length.min(new_length);
        let dirty_from = if geometry_changed {
            0
        } else {
            screen.dirty_from.min(common_length)
        };
        let length_changed = old_length != new_length;

        for index in dirty_from..common_length {
            let cursor_offset = (screen.cursor.line == index).then_some(screen.cursor.char_offset);
            if self.logical_lines[index].matches(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                cursor_offset,
            ) {
                continue;
            }
            let layout = CachedLogicalLayout::build(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                cursor_offset,
                size.width,
            );
            if !length_changed {
                self.heights.set(index, layout.rows.len());
            }
            self.logical_lines[index] = layout;
            self.reflowed_last_frame += 1;
        }

        self.logical_lines.truncate(new_length);
        for index in common_length..new_length {
            self.logical_lines.push(CachedLogicalLayout::build(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                (screen.cursor.line == index).then_some(screen.cursor.char_offset),
                size.width,
            ));
            self.reflowed_last_frame += 1;
        }

        if length_changed {
            self.heights
                .rebuild(self.logical_lines.iter().map(|line| line.rows.len()));
        }

        let cursor_line = &self.logical_lines[screen.cursor.line];
        let relative_cursor = cursor_line
            .cursor
            .expect("cursor layout must contain the virtual cursor");
        let absolute_cursor = PhysicalPosition {
            row: self.heights.prefix_sum(screen.cursor.line) + relative_cursor.row,
            column: relative_cursor.column,
        };
        let viewport_top = absolute_cursor
            .row
            .saturating_add(1)
            .saturating_sub(size.height);
        let rows = (0..size.height)
            .map(|viewport_row| {
                self.heights
                    .line_containing_row(viewport_top + viewport_row)
                    .and_then(|(line, row)| self.logical_lines[line].rows.get(row).cloned())
                    .unwrap_or_else(|| PhysicalRow::empty(size.width))
            })
            .collect();

        PhysicalTerminal {
            size,
            rows,
            cursor: PhysicalCursor {
                position: PhysicalPosition {
                    row: absolute_cursor.row - viewport_top,
                    column: absolute_cursor.column.min(size.width - 1),
                },
                visible: screen.cursor_visible,
            },
            viewport_top,
        }
    }
}

fn layout_virtual_screen(screen: &VirtualScreen, size: TerminalSize) -> PhysicalTerminal {
    let mut layout = LayoutBuilder::new(size.width);

    for (line_index, line) in screen.lines.iter().enumerate() {
        if line_index > 0 {
            layout.start_row();
        }

        layout.write_prompt(
            &screen.prefixes[line_index],
            &screen.prefix_styles[line_index],
        );
        layout.write_line(
            line,
            (screen.cursor.line == line_index).then_some(screen.cursor.char_offset),
            &screen.line_styles[line_index],
        );
    }

    let absolute_cursor = layout
        .cursor
        .expect("the virtual cursor must belong to one of the virtual lines");
    let viewport_top = absolute_cursor
        .row
        .saturating_add(1)
        .saturating_sub(size.height);

    let rows = (0..size.height)
        .map(|viewport_row| {
            layout
                .rows
                .get(viewport_top + viewport_row)
                .cloned()
                .unwrap_or_else(|| PhysicalRow::empty(size.width))
        })
        .collect();

    PhysicalTerminal {
        size,
        rows,
        cursor: PhysicalCursor {
            position: PhysicalPosition {
                row: absolute_cursor.row - viewport_top,
                column: absolute_cursor.column.min(size.width - 1),
            },
            visible: screen.cursor_visible,
        },
        viewport_top,
    }
}

struct LayoutBuilder {
    width: usize,
    rows: Vec<PhysicalRow>,
    row: usize,
    column: usize,
    cursor: Option<PhysicalPosition>,
}

impl LayoutBuilder {
    fn new(width: usize) -> Self {
        Self {
            width,
            rows: vec![PhysicalRow::empty(width)],
            row: 0,
            column: 0,
            cursor: None,
        }
    }

    fn start_row(&mut self) {
        self.rows.push(PhysicalRow::empty(self.width));
        self.row = self.rows.len() - 1;
        self.column = 0;
    }

    fn write_prompt(&mut self, prompt: &str, styles: &[CellStyle]) {
        // Keep at least one physical cell available for the input cursor when
        // the terminal is narrower than the prompt.
        let prompt_limit = self.width.saturating_sub(1);
        for (char_offset, ch) in prompt.chars().enumerate() {
            let width = display_width(ch);
            if width == 0 {
                self.append_zero_width(ch);
                continue;
            }
            if self.column + width > prompt_limit {
                break;
            }
            self.put_glyph(
                ch,
                width,
                styles.get(char_offset).copied().unwrap_or_default(),
            );
        }
    }

    fn write_line(&mut self, text: &str, cursor_offset: Option<usize>, styles: &[CellStyle]) {
        let mut char_offset = 0;

        for ch in text.chars() {
            let mut width = if ch == '\t' {
                (TERMINAL_TAB_STOP - self.column % TERMINAL_TAB_STOP).min(self.width)
            } else {
                display_width(ch)
            };
            let mut rendered = ch;
            if width > self.width {
                rendered = '\u{fffd}';
                width = 1;
            }

            if width > 0 && self.column + width > self.width {
                self.start_row();
                if ch == '\t' {
                    width = TERMINAL_TAB_STOP.min(self.width);
                }
            }

            if cursor_offset == Some(char_offset) {
                self.cursor = Some(PhysicalPosition {
                    row: self.row,
                    column: self.column.min(self.width - 1),
                });
            }

            if width == 0 {
                self.append_zero_width(rendered);
            } else if rendered == '\t' {
                self.put_text(
                    " ".repeat(width),
                    width,
                    styles.get(char_offset).copied().unwrap_or_default(),
                );
            } else {
                self.put_glyph(
                    rendered,
                    width,
                    styles.get(char_offset).copied().unwrap_or_default(),
                );
            }
            char_offset += 1;
        }

        if cursor_offset == Some(char_offset) {
            if self.column == self.width {
                self.start_row();
            }
            self.cursor = Some(PhysicalPosition {
                row: self.row,
                column: self.column.min(self.width - 1),
            });
        }
    }

    fn put_glyph(&mut self, ch: char, width: usize, style: CellStyle) {
        self.put_text(ch.to_string(), width, style);
    }

    fn put_text(&mut self, text: String, width: usize, style: CellStyle) {
        debug_assert!(width > 0);
        debug_assert!(self.column + width <= self.width);

        let leading_column = self.column;
        self.rows[self.row].cells[leading_column] = PhysicalCell::Glyph { text, width, style };
        for column in leading_column + 1..leading_column + width {
            self.rows[self.row].cells[column] = PhysicalCell::Continuation { leading_column };
        }
        self.column += width;
    }

    fn append_zero_width(&mut self, ch: char) {
        let Some((row, column)) = self.previous_glyph_position() else {
            return;
        };
        if let PhysicalCell::Glyph { text, .. } = &mut self.rows[row].cells[column] {
            text.push(ch);
        }
    }

    fn previous_glyph_position(&self) -> Option<(usize, usize)> {
        let (row, column) = if self.column > 0 {
            (self.row, self.column - 1)
        } else if self.row > 0 {
            (self.row - 1, self.width - 1)
        } else {
            return None;
        };

        match self.rows[row].cells.get(column)? {
            PhysicalCell::Glyph { .. } => Some((row, column)),
            PhysicalCell::Continuation { leading_column } => Some((row, *leading_column)),
            PhysicalCell::Empty => None,
        }
    }
}

fn display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffPhysicalTerminal {
    operations: Vec<PhysicalOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PhysicalOperation {
    ClearAll,
    ScrollUp(usize),
    MoveCursor(PhysicalPosition),
    Write(String),
    ClearToEndOfLine,
    SetCursorVisibility(bool),
}

fn diff_physical_terminal(
    previous: Option<&PhysicalTerminal>,
    next: &PhysicalTerminal,
) -> DiffPhysicalTerminal {
    let mut content_operations = if previous.is_none_or(|previous| previous.size != next.size) {
        full_redraw_operations(next)
    } else {
        let previous = previous.expect("checked above");
        scrolling_row_operations(previous, next)
            .unwrap_or_else(|| changed_row_operations(previous, next))
    };

    let content_changed = !content_operations.is_empty();
    let cursor_changed = previous.is_none_or(|previous| previous.cursor != next.cursor);
    let visibility_changed =
        previous.is_none_or(|previous| previous.cursor.visible != next.cursor.visible);
    let mut operations = Vec::new();

    if content_changed {
        operations.push(PhysicalOperation::SetCursorVisibility(false));
        operations.append(&mut content_operations);
    }

    if content_changed || cursor_changed {
        operations.push(PhysicalOperation::MoveCursor(next.cursor.position));
    }

    if content_changed || visibility_changed {
        operations.push(PhysicalOperation::SetCursorVisibility(next.cursor.visible));
    }

    DiffPhysicalTerminal { operations }
}

fn scrolling_row_operations(
    previous: &PhysicalTerminal,
    next: &PhysicalTerminal,
) -> Option<Vec<PhysicalOperation>> {
    let amount = next.viewport_top.checked_sub(previous.viewport_top)?;
    if amount == 0 || amount >= next.size.height {
        return None;
    }
    let mut operations = vec![PhysicalOperation::ScrollUp(amount)];
    let mut shifted_rows = previous.rows[amount..].to_vec();
    shifted_rows
        .extend(std::iter::repeat_with(|| PhysicalRow::empty(next.size.width)).take(amount));
    let shifted = PhysicalTerminal {
        size: previous.size,
        rows: shifted_rows,
        cursor: previous.cursor,
        viewport_top: next.viewport_top,
    };
    operations.extend(changed_row_operations(&shifted, next));
    Some(operations)
}

fn full_redraw_operations(next: &PhysicalTerminal) -> Vec<PhysicalOperation> {
    let mut operations = vec![PhysicalOperation::ClearAll];

    for (row_index, row) in next.rows.iter().enumerate() {
        let Some(end) = row_content_end(row) else {
            continue;
        };
        operations.push(PhysicalOperation::MoveCursor(PhysicalPosition {
            row: row_index,
            column: 0,
        }));
        operations.push(PhysicalOperation::Write(row_terminal_text(row, 0, end)));
    }

    operations
}

fn row_content_end(row: &PhysicalRow) -> Option<usize> {
    row.cells
        .iter()
        .rposition(|cell| !matches!(cell, PhysicalCell::Empty))
        .map(|index| index + 1)
}

fn changed_row_operations(
    previous: &PhysicalTerminal,
    next: &PhysicalTerminal,
) -> Vec<PhysicalOperation> {
    let mut operations = Vec::new();

    for row_index in 0..next.size.height {
        let previous_row = &previous.rows[row_index];
        let next_row = &next.rows[row_index];
        if previous_row == next_row {
            continue;
        }

        let Some((start, end)) = changed_cell_span(previous_row, next_row) else {
            continue;
        };
        operations.push(PhysicalOperation::MoveCursor(PhysicalPosition {
            row: row_index,
            column: start,
        }));

        if next_row.cells[start..]
            .iter()
            .all(|cell| matches!(cell, PhysicalCell::Empty))
        {
            operations.push(PhysicalOperation::ClearToEndOfLine);
        } else {
            operations.push(PhysicalOperation::Write(row_terminal_text(
                next_row, start, end,
            )));
        }
    }

    operations
}

fn changed_cell_span(previous: &PhysicalRow, next: &PhysicalRow) -> Option<(usize, usize)> {
    let width = previous.cells.len();
    let first_difference = (0..width).find(|&index| previous.cells[index] != next.cells[index])?;
    let last_difference = (0..width)
        .rev()
        .find(|&index| previous.cells[index] != next.cells[index])
        .expect("a first difference implies a last difference");

    let mut start = first_difference;
    for row in [previous, next] {
        if let PhysicalCell::Continuation { leading_column } = row.cells[start] {
            start = start.min(leading_column);
        }
    }

    let mut end = last_difference + 1;
    for row in [previous, next] {
        end = end.max(glyph_end_covering(row, last_difference));
    }

    Some((start, end.min(width)))
}

fn glyph_end_covering(row: &PhysicalRow, column: usize) -> usize {
    match &row.cells[column] {
        PhysicalCell::Glyph { width, .. } => column + width,
        PhysicalCell::Continuation { leading_column } => match &row.cells[*leading_column] {
            PhysicalCell::Glyph { width, .. } => leading_column + width,
            _ => column + 1,
        },
        PhysicalCell::Empty => column + 1,
    }
}

fn row_text(row: &PhysicalRow, start: usize, end: usize) -> String {
    let mut output = String::new();
    for cell in &row.cells[start..end] {
        match cell {
            PhysicalCell::Empty => output.push(' '),
            PhysicalCell::Glyph { text, .. } => output.push_str(text),
            PhysicalCell::Continuation { .. } => {}
        }
    }
    output
}

fn row_terminal_text(row: &PhysicalRow, start: usize, end: usize) -> String {
    let mut output = String::new();
    let mut current_style = CellStyle::default();
    for cell in &row.cells[start..end] {
        match cell {
            PhysicalCell::Empty => {
                if current_style != CellStyle::default() {
                    push_style_escape(&mut output, CellStyle::default());
                    current_style = CellStyle::default();
                }
                output.push(' ');
            }
            PhysicalCell::Glyph { text, style, .. } => {
                if *style != current_style {
                    push_style_escape(&mut output, *style);
                    current_style = *style;
                }
                output.push_str(text);
            }
            PhysicalCell::Continuation { .. } => {}
        }
    }
    if current_style != CellStyle::default() {
        push_style_escape(&mut output, CellStyle::default());
    }
    output
}

fn push_style_escape(output: &mut String, style: CellStyle) {
    output.push_str("\x1b[0m");
    if style.bold {
        output.push_str("\x1b[1m");
    }
    if style.dim {
        output.push_str("\x1b[2m");
    }
    if style.italic {
        output.push_str("\x1b[3m");
    }
    if style.underline {
        output.push_str("\x1b[4m");
    }
    if style.blink {
        output.push_str("\x1b[5m");
    }
    if style.reverse {
        output.push_str("\x1b[7m");
    }
    if style.strikethrough {
        output.push_str("\x1b[9m");
    }
    if let Some(color) = style.foreground {
        push_color_escape(output, color, true);
    }
    if let Some(color) = style.background {
        push_color_escape(output, color, false);
    }
}

fn push_color_escape(output: &mut String, color: TerminalColor, foreground: bool) {
    let standard_code = match color {
        TerminalColor::Black => Some(if foreground { 30 } else { 40 }),
        TerminalColor::Red => Some(if foreground { 31 } else { 41 }),
        TerminalColor::Green => Some(if foreground { 32 } else { 42 }),
        TerminalColor::Yellow => Some(if foreground { 33 } else { 43 }),
        TerminalColor::Blue => Some(if foreground { 34 } else { 44 }),
        TerminalColor::Magenta => Some(if foreground { 35 } else { 45 }),
        TerminalColor::Cyan => Some(if foreground { 36 } else { 46 }),
        TerminalColor::White => Some(if foreground { 37 } else { 47 }),
        TerminalColor::BrightBlack => Some(if foreground { 90 } else { 100 }),
        TerminalColor::BrightRed => Some(if foreground { 91 } else { 101 }),
        TerminalColor::BrightGreen => Some(if foreground { 92 } else { 102 }),
        TerminalColor::BrightYellow => Some(if foreground { 93 } else { 103 }),
        TerminalColor::BrightBlue => Some(if foreground { 94 } else { 104 }),
        TerminalColor::BrightMagenta => Some(if foreground { 95 } else { 105 }),
        TerminalColor::BrightCyan => Some(if foreground { 96 } else { 106 }),
        TerminalColor::BrightWhite => Some(if foreground { 97 } else { 107 }),
        TerminalColor::Indexed(_) | TerminalColor::Rgb(_, _, _) => None,
    };
    if let Some(code) = standard_code {
        output.push_str(&format!("\x1b[{code}m"));
        return;
    }
    let channel = if foreground { 38 } else { 48 };
    match color {
        TerminalColor::Indexed(index) => output.push_str(&format!("\x1b[{channel};5;{index}m")),
        TerminalColor::Rgb(red, green, blue) => {
            output.push_str(&format!("\x1b[{channel};2;{red};{green};{blue}m"));
        }
        _ => unreachable!("standard colors returned above"),
    }
}

struct DiffRenderer {
    previous: Option<PhysicalTerminal>,
    layout: IndexedPhysicalLayout,
}

impl DiffRenderer {
    fn new() -> Self {
        Self {
            previous: None,
            layout: IndexedPhysicalLayout::default(),
        }
    }

    fn invalidate(&mut self) {
        self.previous = None;
    }

    fn render(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        size: TerminalSize,
    ) -> io::Result<()> {
        let next = self.layout.layout(screen, size);
        let diff = diff_physical_terminal(self.previous.as_ref(), &next);
        apply_diff(output, &diff)?;
        output.flush()?;
        self.previous = Some(next);
        Ok(())
    }
}

fn apply_diff(output: &mut impl Write, diff: &DiffPhysicalTerminal) -> io::Result<()> {
    for operation in &diff.operations {
        match operation {
            PhysicalOperation::ClearAll => {
                queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
            }
            PhysicalOperation::ScrollUp(amount) => {
                queue!(output, ScrollUp((*amount).min(u16::MAX as usize) as u16))?;
            }
            PhysicalOperation::MoveCursor(position) => {
                queue!(output, MoveTo(position.column as u16, position.row as u16))?;
            }
            PhysicalOperation::Write(text) => {
                // Every write is self-contained: terminal style is reset at
                // both boundaries so a partial diff never depends on the
                // style left by an earlier row or an external application.
                queue!(output, Print("\x1b[0m"), Print(text), Print("\x1b[0m"))?;
            }
            PhysicalOperation::ClearToEndOfLine => {
                queue!(output, Clear(ClearType::UntilNewLine))?;
            }
            PhysicalOperation::SetCursorVisibility(true) => {
                queue!(output, Show)?;
            }
            PhysicalOperation::SetCursorVisibility(false) => {
                queue!(output, Hide)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_size(width: u16, height: u16) -> TerminalSize {
        TerminalSize::new(width, height)
    }

    fn visible_row_text(row: &PhysicalRow) -> String {
        row_text(row, 0, row.cells.len())
            .trim_end_matches(' ')
            .to_string()
    }

    fn temporary_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "theseus-render-v2-{name}-{}-{}.jsonc",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn cli_matches_main_prompt_and_shell_argument_rules() {
        assert_eq!(
            parse_cli(&["--prompt".to_string(), "hello".to_string()]),
            Ok(Cli::Headless("hello".to_string()))
        );
        assert!(parse_cli(&["--prompt".to_string()]).is_err());
        assert!(
            parse_cli(&[
                "--prompt".to_string(),
                "hello".to_string(),
                "extra".to_string()
            ])
            .is_err()
        );
        assert_eq!(
            parse_cli(&["printf".to_string(), "ok".to_string()]),
            Ok(Cli::Shell(vec!["printf".to_string(), "ok".to_string()]))
        );
    }

    #[test]
    fn initial_cursor_is_after_prompt() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(), test_size(20, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 6 }
        );
    }

    #[test]
    fn long_virtual_line_wraps_without_repeating_prompt() {
        let screen = VirtualScreen::with_line("abcdefghij", 10);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&terminal.rows[1]), "efghij");
        assert_eq!(
            terminal
                .rows
                .iter()
                .map(visible_row_text)
                .filter(|row| row.contains(SHELL_PROMPT))
                .count(),
            1
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn logical_tab_is_preserved_and_maps_to_physical_spaces() {
        let screen = VirtualScreen::with_line("a\tb", 3);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines[0], "a\tb");
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> a b");
        assert_eq!(terminal.cursor.position.column, 9);
    }

    #[test]
    fn config_patch_preserves_jsonc_comments_and_validates_result() {
        let path = temporary_test_path("config-patch");
        let init = AgentConfig::load_or_create_at(path.clone()).unwrap();
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replacen("{\n", "{\n  // keep me\n", 1)).unwrap();

        let config = patch_config_jsonc_file(
            &path,
            ConfigPatch::SetModel("example/new-model".to_string()),
        )
        .unwrap();
        let patched = fs::read_to_string(&path).unwrap();

        assert_eq!(init.path, path);
        assert!(patched.contains("// keep me"));
        assert!(patched.contains(r#""model": "example/new-model""#));
        assert_eq!(
            config
                .llm_request_settings
                .body
                .get("model")
                .and_then(Value::as_str),
            Some("example/new-model")
        );
        let validation_prefix = format!(
            ".{}.render-v2-",
            path.file_name().unwrap().to_string_lossy()
        );
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(&validation_prefix) || !name.ends_with(".tmp")
                })
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enter_creates_new_prompt_and_moves_virtual_cursor() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::new(SHELL_PROMPT, "hello"),
                RenderLine::new(SHELL_PROMPT, ""),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 0,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines, ["hello", ""]);
        assert_eq!(
            screen.cursor,
            VirtualCursor {
                line: 1,
                char_offset: 0
            }
        );
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> hello");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn cursor_navigation_never_enters_prompt() {
        let screen = VirtualScreen::new();
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.cursor.char_offset, 0);
        assert_eq!(terminal.cursor.position.column, SHELL_PROMPT.len());
    }

    #[test]
    fn paste_is_split_into_virtual_lines() {
        let mut buffer = EditorBuffer::new();
        buffer.insert_text("one\r\ntwo\nthree");

        assert_eq!(buffer.lines, ["one", "two", "three"]);
        assert_eq!(
            buffer.cursor,
            VirtualCursor {
                line: 2,
                char_offset: 5
            }
        );
    }

    #[test]
    fn resize_reflows_virtual_line() {
        let screen = VirtualScreen::with_line("abcdefghij", 10);
        let wide = layout_virtual_screen(&screen, test_size(10, 6));
        let narrow = layout_virtual_screen(&screen, test_size(7, 6));

        assert_eq!(visible_row_text(&wide.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&wide.rows[1]), "efghij");
        assert_eq!(visible_row_text(&narrow.rows[0]), "user> a");
        assert_eq!(visible_row_text(&narrow.rows[1]), "bcdefgh");
        assert_eq!(visible_row_text(&narrow.rows[2]), "ij");
    }

    #[test]
    fn exact_terminal_boundary_places_cursor_on_next_row() {
        let screen = VirtualScreen::with_line("abcd", 4);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 0 }
        );
    }

    #[test]
    fn wide_character_uses_leading_and_continuation_cells() {
        let screen = VirtualScreen::with_line("界", 1);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(
            terminal.rows[0].cells[6],
            PhysicalCell::Glyph {
                text: "界".to_string(),
                width: 2,
                style: CellStyle::default(),
            }
        );
        assert_eq!(
            terminal.rows[0].cells[7],
            PhysicalCell::Continuation { leading_column: 6 }
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 8 }
        );
    }

    #[test]
    fn unchanged_frame_produces_no_operations() {
        let terminal =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let diff = diff_physical_terminal(Some(&terminal), &terminal);

        assert!(diff.operations.is_empty());
    }

    #[test]
    fn full_redraw_does_not_write_empty_row_tail() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(), test_size(20, 5));
        let diff = diff_physical_terminal(None, &terminal);

        let writes = diff
            .operations
            .iter()
            .filter_map(|operation| match operation {
                PhysicalOperation::Write(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(writes, ["user> "]);
    }

    #[test]
    fn changed_character_produces_incremental_row_write() {
        let previous =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let next = layout_virtual_screen(&VirtualScreen::with_line("hallo", 5), test_size(20, 5));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(
            !diff.operations.contains(&PhysicalOperation::ClearAll),
            "same-size frames should not trigger a full redraw"
        );
        assert!(
            diff.operations.iter().any(
                |operation| matches!(operation, PhysicalOperation::Write(text) if text == "a")
            )
        );
    }

    #[test]
    fn geometry_change_triggers_full_redraw() {
        let previous =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let next = layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(10, 5));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn viewport_keeps_cursor_visible_for_tall_content() {
        let screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(|line| RenderLine::new(SHELL_PROMPT, line))
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(20, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> three");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user> four");
        assert_eq!(terminal.cursor.position.row, 1);
    }

    #[test]
    fn advancing_viewport_uses_terminal_scroll_and_preserves_scrollback() {
        let previous_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        let next_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let size = test_size(20, 2);
        let previous = layout_virtual_screen(&previous_screen, size);
        let next = layout_virtual_screen(&next_screen, size);

        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ScrollUp(1)));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn indexed_layout_matches_reference_layout() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("first line"),
                RenderLine::new("prompt> ", "wide 界 value that wraps"),
                RenderLine::plain("tail"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 7,
            },
            true,
        );
        let size = test_size(12, 4);
        let reference = layout_virtual_screen(&screen, size);
        let indexed = IndexedPhysicalLayout::default().layout(&screen, size);

        assert_eq!(indexed, reference);
    }

    #[test]
    fn indexed_layout_reflows_only_edited_cursor_line() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);
        assert_eq!(layout.reflowed_last_frame, 3);

        let mut edited = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft!"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 6,
            },
            true,
        );
        edited.dirty_from = 1;
        layout.layout(&edited, size);

        assert_eq!(layout.reflowed_last_frame, 1);
    }

    #[test]
    fn indexed_layout_keeps_stable_prefix_when_lines_are_appended() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "draft"),
            ],
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);

        let mut appended = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "submitted"),
                RenderLine::plain("command output"),
                RenderLine::new("user> ", ""),
            ],
            VirtualCursor {
                line: 4,
                char_offset: 0,
            },
            true,
        );
        appended.dirty_from = 2;
        let indexed = layout.layout(&appended, size);

        assert_eq!(layout.reflowed_last_frame, 3);
        assert_eq!(indexed, layout_virtual_screen(&appended, size));
    }

    #[test]
    fn virtual_screen_tail_can_be_replaced_without_cloning_prefix() {
        let mut screen = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("stable"), RenderLine::plain("old tail")],
            VirtualCursor {
                line: 1,
                char_offset: 8,
            },
            true,
        );
        let stable_pointer = screen.lines[0].as_ptr();

        screen.truncate(1);
        screen.push_render_line(&RenderLine::plain("new tail"));

        assert_eq!(screen.lines, ["stable", "new tail"]);
        assert_eq!(screen.lines[0].as_ptr(), stable_pointer);
    }

    #[test]
    fn command_history_can_be_recalled_from_non_empty_draft() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.buffer.replace_with("draft");

        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(editor.buffer.text(), "git status");
        assert_eq!(editor.history_draft, "draft");
    }

    #[test]
    fn typed_history_normalization_keeps_latest_trimmed_duplicate() {
        let shell = |text: &str| HistoryEntry {
            text: text.to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        };

        let normalized = normalize_history(vec![
            shell(" first "),
            shell("second"),
            shell("first"),
            shell("  "),
        ]);

        assert_eq!(normalized, vec![shell("second"), shell("first")]);
    }

    #[test]
    fn recalled_single_line_history_is_immediately_editable() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.buffer.text(), "git statusx");
        assert!(editor.history_position.is_some());
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn recalled_multiline_history_reopens_unified_multiline_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn recalled_multiline_history_renders_command_hint_and_hidden_cursor() {
        let history = vec![HistoryEntry {
            text: "line one\nline two".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::MultiLineAsk,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        let (lines, cursor) = editor.render_lines();

        assert_eq!(lines[0].prefix, "user> ");
        assert_eq!(lines[0].text, "/ask");
        assert!(lines[1].text.contains(input::MULTILINE_SUBMIT_COMMAND));
        assert_eq!(lines[2].prefix, input::DEFAULT_COMMAND_CONTINUATION_PROMPT);
        assert_eq!(lines[2].text, "line one");
        assert_eq!(cursor.line, 3);
        assert!(editor.history_is_browsing());
    }

    #[test]
    fn plain_arrow_reopens_recalled_multiline_special_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn multiline_editor_history_is_preview_until_enter() {
        let history = vec![HistoryEntry {
            text: "previous question".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::ask(None, &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert!(editor.history_is_browsing());
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.buffer.text(), "previous question");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn api_key_editor_masks_rendered_text_but_submits_original() {
        let mut editor = UnifiedEditor::api_key();
        editor.buffer.replace_with("secret");
        let (lines, _) = editor.render_lines();

        assert_eq!(lines[0].text, "******");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::ApiKey,
                text: "secret".to_string(),
            })
        );
    }

    #[test]
    fn api_key_paste_discards_line_breaks() {
        let mut editor = UnifiedEditor::api_key();

        editor.handle_event(Event::Paste("first\r\nsecond\n".to_string()));

        assert_eq!(editor.buffer.text(), "firstsecond");
        assert_eq!(editor.buffer.lines.len(), 1);
    }

    #[test]
    fn multiline_ctrl_d_submits_without_end_marker() {
        let mut editor = UnifiedEditor::shell(None, &[]);
        editor.buffer.replace_with("echo one\necho two");

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::Shell,
                text: "echo one\necho two".to_string(),
            })
        );
    }

    #[test]
    fn completion_includes_git_subcommands() {
        let (_, candidates) = completion_candidates("git che", 7, true).unwrap();

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.replacement == "checkout")
        );
    }

    #[test]
    fn ansi_styles_are_cells_not_layout_characters() {
        let lines = ansi_render_lines("\x1b[1;32mgreen\x1b[0m");
        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: 5,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(10, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "green");
        assert_eq!(terminal.cursor.position.column, 5);
        assert!(matches!(
            terminal.rows[0].cells[0],
            PhysicalCell::Glyph {
                style: CellStyle {
                    foreground: Some(TerminalColor::Green),
                    bold: true,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn ansi_tab_is_preserved_and_uses_native_terminal_tab_stop() {
        let lines = ansi_render_lines("X\tTAB_RIGHT\n");

        assert_eq!(lines[0].text, "X\tTAB_RIGHT");
        assert_eq!(lines[0].styles.len(), char_len("X\tTAB_RIGHT"));

        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: char_len("X\tTAB_RIGHT"),
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(80, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "X       TAB_RIGHT");
        assert_eq!(terminal.cursor.position.column, 17);
    }

    #[test]
    fn terminal_clear_keeps_only_output_after_last_display_clear() {
        assert_eq!(suffix_after_last_display_clear("plain text"), None);
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[H\x1b[2Jkeep"),
            Some("keep")
        );
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[2Jmiddle\x1b[2Jfinal"),
            Some("final")
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[0Jtoo"),
            None,
            "ED 0 only clears below the cursor and must not discard the transcript"
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[3Jtoo"),
            None,
            "ED 3 clears scrollback, not the visible display"
        );
    }

    #[test]
    fn ansi_true_color_indexed_background_and_attributes_round_trip() {
        let lines = ansi_render_lines("\x1b[38;2;1;2;3;48;5;208;7;9mX\x1b[0m");
        let style = lines[0].styles[0];

        assert_eq!(style.foreground, Some(TerminalColor::Rgb(1, 2, 3)));
        assert_eq!(style.background, Some(TerminalColor::Indexed(208)));
        assert!(style.reverse);
        assert!(style.strikethrough);

        let rendered = terminal_styled_text(&lines[0].text, &lines[0].styles);
        assert!(rendered.contains("\x1b[38;2;1;2;3m"));
        assert!(rendered.contains("\x1b[48;5;208m"));
        assert!(rendered.contains("\x1b[7m"));
        assert!(rendered.contains("\x1b[9m"));
    }

    #[test]
    fn style_only_change_is_emitted_as_physical_diff() {
        let plain = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("x")],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let styled = VirtualScreen::from_render_lines(
            vec![RenderLine::styled(
                "x",
                vec![CellStyle {
                    foreground: Some(TerminalColor::Red),
                    ..CellStyle::default()
                }],
            )],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let previous = layout_virtual_screen(&plain, test_size(10, 2));
        let next = layout_virtual_screen(&styled, test_size(10, 2));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.iter().any(|operation| {
            matches!(operation, PhysicalOperation::Write(text) if text.contains("\x1b[31m") && text.contains('x'))
        }));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn shell_styles_follow_configured_palette() {
        let mut palette = input::default_shell_highlight_palette();
        palette.insert(
            "keyword".to_string(),
            Some(input::ShellHighlightStyle::tags(vec![
                "bold".to_string(),
                "red".to_string(),
            ])),
        );
        let mut lines = vec![RenderLine::plain("if true; then")];

        style_shell_lines(&mut lines, "if true; then", &palette);

        assert_eq!(lines[0].styles[0].foreground, Some(TerminalColor::Red));
        assert!(lines[0].styles[0].bold);
    }

    #[test]
    fn slash_command_only_styles_command_name_in_command_editor() {
        let mut lines = vec![RenderLine::new("user> ", "/ask echo \"$USER\"")];

        style_shell_lines(
            &mut lines,
            "/ask echo \"$USER\"",
            &input::default_shell_highlight_palette(),
        );

        assert_eq!(
            lines[0].styles[0].foreground,
            Some(TerminalColor::BrightCyan)
        );
        assert_eq!(lines[0].styles[5], CellStyle::default());
        assert_eq!(lines[0].styles.last(), Some(&CellStyle::default()));
    }

    #[test]
    fn picker_anchors_viewport_after_its_last_row() {
        let mut picker = PickerState::new(
            "Choose",
            vec![
                PickerItem {
                    id: "one".to_string(),
                    label: "One".to_string(),
                    detail: String::new(),
                },
                PickerItem {
                    id: "two".to_string(),
                    label: "Two".to_string(),
                    detail: String::new(),
                },
            ],
            false,
        );
        let (lines, cursor, visible) = picker.render_lines(10, 80);

        assert_eq!(cursor.line, lines.len() - 1);
        assert!(!visible);
    }
}

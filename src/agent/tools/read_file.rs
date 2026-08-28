use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Cursor, Read},
    path::Path,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use image::{GenericImageView, ImageReader, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};

use super::{
    AgentTool, DEFAULT_READ_FILE_END_LINE, DEFAULT_READ_FILE_START_LINE, ToolAttachment,
    ToolOutput, args::optional_usize_arg, format_tool_call_name,
    paths::expanded_path_arg_with_home,
};
use crate::agent::{AgentRunContext, config::ImageInputSettings};

pub(super) struct ReadFileTool {
    image_input: ImageInputSettings,
}

impl ReadFileTool {
    pub(super) fn new(image_input: ImageInputSettings) -> Self {
        Self { image_input }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new(ImageInputSettings::default())
    }
}

impl AgentTool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn schema(&self) -> Value {
        let description = if self.image_input.enable {
            "Read a UTF-8 text file or a local image file (JPEG, PNG, or WebP) from the local filesystem. Text output includes line numbers (e.g. '42│ content') so you can target specific lines in subsequent calls. Image output is attached to the LLM context."
        } else {
            "Read a UTF-8 text file from the local filesystem. Output includes line numbers (e.g. '42│ content') so you can target specific lines in subsequent calls."
        };

        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file." },
                        "start_line": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "1-based first line number to read. Defaults to 1."
                        },
                        "end_line": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "1-based last line number to read, inclusive. When omitted, reads at most 200 lines starting at start_line and reports how many lines remain; if start_line is also omitted, reads only the first 200 lines."
                        },
                        "line_numbers": {
                            "type": "boolean",
                            "description": "Prefix each line with its 1-based line number. Defaults to true."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn display(&self, arguments: &Value, _context: &AgentRunContext) -> String {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return format_tool_call_name(self.name());
        };
        if is_supported_image_path(Path::new(path)) {
            return format!("{} {path}", format_tool_call_name(self.name()));
        }

        let start_line = arguments.get("start_line").and_then(Value::as_u64);
        let start_display = start_line.unwrap_or(DEFAULT_READ_FILE_START_LINE as u64);
        let end_line = arguments.get("end_line").and_then(Value::as_u64);

        match end_line {
            Some(end_line) if end_line < start_display => {
                format!("{} {path}:empty", format_tool_call_name(self.name()))
            }
            Some(end_line) => format!(
                "{} {path}:{start_display}-{end_line}",
                format_tool_call_name(self.name())
            ),
            None if start_line.is_none() => format!(
                "{} {path}:{start_display}-{}",
                format_tool_call_name(self.name()),
                DEFAULT_READ_FILE_END_LINE
            ),
            None => {
                let end_display = start_display
                    .saturating_add(DEFAULT_READ_FILE_END_LINE as u64)
                    .saturating_sub(1);
                format!(
                    "{} {path}:{start_display}-{end_display}",
                    format_tool_call_name(self.name())
                )
            }
        }
    }

    fn execute(&self, arguments: &Value, context: &AgentRunContext) -> io::Result<ToolOutput> {
        read_file(
            arguments,
            context.working_dir.as_deref(),
            &context.image_input,
            context.max_tool_output_bytes,
        )
    }
}

fn read_file(
    arguments: &Value,
    working_dir: Option<&Path>,
    image_input: &ImageInputSettings,
    max_output_bytes: usize,
) -> io::Result<ToolOutput> {
    read_file_with_home(arguments, None, working_dir, image_input, max_output_bytes)
}

fn read_file_with_home(
    arguments: &Value,
    home_dir: Option<&Path>,
    working_dir: Option<&Path>,
    image_input: &ImageInputSettings,
    max_output_bytes: usize,
) -> io::Result<ToolOutput> {
    let path = expanded_path_arg_with_home(arguments, "path", home_dir, working_dir)?;
    if is_supported_image_path(&path) {
        if !image_input.enable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "image input is disabled",
            ));
        }
        return read_image_file(&path, image_input);
    }

    let start_line_arg = optional_usize_arg(arguments, "start_line")?;
    let end_line_arg = optional_usize_arg(arguments, "end_line")?;
    let start_line = start_line_arg.unwrap_or(DEFAULT_READ_FILE_START_LINE);
    let report_remaining_lines = start_line_arg.is_some() && end_line_arg.is_none();
    // Without an explicit end, read a bounded 200-line window. Only an explicit
    // start requests the remaining-line notice; the fully default range keeps
    // its existing concise output.
    let end_line = match (start_line_arg, end_line_arg) {
        (None, None) => Some(DEFAULT_READ_FILE_END_LINE),
        (Some(start_line), None) => Some(
            start_line
                .saturating_add(DEFAULT_READ_FILE_END_LINE)
                .saturating_sub(1),
        ),
        (_, end_line) => end_line,
    };
    if start_line == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "read_file start_line must be greater than or equal to 1",
        ));
    }
    if let Some(end_line) = end_line
        && end_line < start_line
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "read_file end_line must be greater than or equal to start_line",
        ));
    }
    let line_numbers = arguments
        .get("line_numbers")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    Ok(ToolOutput::text(read_line_chunk(
        &path,
        start_line,
        end_line,
        line_numbers,
        max_output_bytes,
        report_remaining_lines,
    )?))
}

fn is_supported_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp"
            )
        })
        .unwrap_or(false)
}

fn read_image_file(path: &Path, image_input: &ImageInputSettings) -> io::Result<ToolOutput> {
    let bytes = fs::read(path)?;
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(io::Error::other)?
        .decode()
        .map_err(io::Error::other)?;
    let resized = image.thumbnail(image_input.max_width as u32, image_input.max_height as u32);
    let (width, height) = resized.dimensions();
    let rgb = resized.to_rgb8();
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut encoded, 85);
    encoder.encode_image(&rgb).map_err(io::Error::other)?;
    let data_url = format!("data:image/jpeg;base64,{}", BASE64_STANDARD.encode(encoded));

    Ok(ToolOutput {
        text: format!(
            "Read image {} (image/jpeg, {width}x{height}).",
            path.display()
        ),
        attachments: vec![ToolAttachment::Image {
            path: path.to_path_buf(),
            mime_type: "image/jpeg".to_string(),
            width,
            height,
            data_url,
        }],
    })
}

fn read_line_chunk(
    path: &Path,
    start_line: usize,
    end_line: Option<usize>,
    line_numbers: bool,
    max_output_bytes: usize,
    report_remaining_lines: bool,
) -> io::Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut output = String::new();
    let mut line_number = 1usize;

    while line_number < start_line {
        if reader.skip_until(b'\n')? == 0 {
            return Ok(output);
        }
        line_number = line_number.saturating_add(1);
    }

    loop {
        if end_line.is_some_and(|end_line| line_number > end_line) {
            break;
        }
        if reader.fill_buf()?.is_empty() {
            break;
        }
        if line_numbers {
            let prefix = format!("{line_number:>6}│ ");
            if !append_bounded(&mut output, &prefix, max_output_bytes) {
                break;
            }
        }

        let available = max_output_bytes.saturating_sub(output.len());
        if available == 0 {
            mark_truncated(&mut output, max_output_bytes);
            break;
        }
        let read_limit = available.saturating_add(1);
        let mut bytes = Vec::with_capacity(read_limit.min(8192));
        {
            let mut limited = (&mut reader).take(u64::try_from(read_limit).unwrap_or(u64::MAX));
            limited.read_until(b'\n', &mut bytes)?;
        }

        let line_complete = bytes.ends_with(b"\n") || reader.fill_buf()?.is_empty();
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() && !line_complete => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix")
            }
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        if !append_bounded(&mut output, text, max_output_bytes) {
            break;
        }
        if !line_complete {
            mark_truncated(&mut output, max_output_bytes);
            break;
        }
        line_number = line_number.saturating_add(1);
    }

    if report_remaining_lines && end_line.is_some_and(|end_line| line_number > end_line) {
        let remaining_lines = count_remaining_lines(&mut reader)?;
        if remaining_lines > 0 {
            let separator = if output.is_empty() || output.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let notice = format!("{separator}... {remaining_lines} lines to end...");
            append_bounded(&mut output, &notice, max_output_bytes);
        }
    }

    Ok(output)
}

fn count_remaining_lines(reader: &mut impl BufRead) -> io::Result<usize> {
    let mut count = 0usize;
    while reader.skip_until(b'\n')? != 0 {
        count = count.saturating_add(1);
    }
    Ok(count)
}

fn append_bounded(output: &mut String, text: &str, max_bytes: usize) -> bool {
    if output.len().saturating_add(text.len()) <= max_bytes {
        output.push_str(text);
        return true;
    }

    let content_limit = truncation_content_limit(max_bytes);
    truncate_to_utf8_boundary(output, content_limit);
    let remaining = content_limit.saturating_sub(output.len());
    let end = utf8_prefix_end(text, remaining);
    output.push_str(&text[..end]);
    mark_truncated(output, max_bytes);
    false
}

const READ_FILE_TRUNCATION_MARKER: &str = "[truncated: read_file output limit reached]";

fn truncation_content_limit(max_bytes: usize) -> usize {
    max_bytes.saturating_sub(READ_FILE_TRUNCATION_MARKER.len().saturating_add(1))
}

fn mark_truncated(output: &mut String, max_bytes: usize) {
    if max_bytes <= READ_FILE_TRUNCATION_MARKER.len() {
        output.clear();
        output.push_str(&READ_FILE_TRUNCATION_MARKER[..max_bytes]);
        return;
    }

    truncate_to_utf8_boundary(output, truncation_content_limit(max_bytes));
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(READ_FILE_TRUNCATION_MARKER);
}

fn truncate_to_utf8_boundary(text: &mut String, max_bytes: usize) {
    let end = utf8_prefix_end(text, max_bytes);
    text.truncate(end);
}

fn utf8_prefix_end(text: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::agent::tools::args::string_arg;

    #[test]
    fn read_file_tool_reads_text() {
        let path =
            std::env::temp_dir().join(format!("theseus-agent-read-{}.txt", std::process::id()));
        fs::write(&path, "hello").unwrap();
        let arguments = json!({ "path": path, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "hello");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_expands_home_prefix() {
        let home =
            std::env::temp_dir().join(format!("theseus-agent-read-home-{}", std::process::id()));
        let config_dir = home.join(".theseus");
        let config_path = config_dir.join("config.jsonc");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(&config_path, "{\"model\":\"test\"}\n").unwrap();
        let arguments = json!({
            "path": "~/.theseus/config.jsonc",
            "line_numbers": false,
        });

        let output = read_file_with_home(
            &arguments,
            Some(&home),
            None,
            &ImageInputSettings::default(),
            crate::agent::config::models::DEFAULT_MAX_TOOL_OUTPUT_BYTES,
        )
        .unwrap();

        assert_eq!(output.text, "{\"model\":\"test\"}\n");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn read_file_tool_resolves_relative_path_against_working_dir() {
        let base =
            std::env::temp_dir().join(format!("theseus-agent-read-cwd-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("notes.txt"), "hello from base").unwrap();
        let arguments = json!({ "path": "notes.txt", "line_numbers": false });

        let output = read_file_with_home(
            &arguments,
            None,
            Some(&base),
            &ImageInputSettings::default(),
            crate::agent::config::models::DEFAULT_MAX_TOOL_OUTPUT_BYTES,
        )
        .unwrap();

        assert_eq!(output.text, "hello from base");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn read_file_tool_includes_line_numbers_by_default() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-linenos-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let arguments = json!({ "path": path });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "     1│ alpha\n     2│ beta\n     3│ gamma\n");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_can_disable_line_numbers() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-nolinenos-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "alpha\nbeta\n").unwrap();
        let arguments = json!({ "path": path, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "alpha\nbeta\n");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_counts_lines_through_start_line() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-start-nos-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        let arguments = json!({
            "path": path,
            "start_line": 1,
            "end_line": 2,
        });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "     1│ one\n     2│ two\n");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_limits_start_only_read_to_200_lines_and_reports_remaining_lines() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-start-window-{}.txt",
            std::process::id()
        ));
        let content = (1..=500)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(&path, content).unwrap();
        let arguments = json!({ "path": path, "start_line": 1, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert!(output.text.contains("line 1\n"));
        assert!(output.text.contains("line 200\n"));
        assert!(!output.text.contains("line 201\n"));
        assert!(output.text.ends_with("... 300 lines to end..."));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_omits_remaining_lines_notice_when_start_only_window_reaches_eof() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-start-near-eof-{}.txt",
            std::process::id()
        ));
        let content = (1..=300)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(&path, content).unwrap();
        let arguments = json!({ "path": path, "start_line": 250, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text.lines().count(), 51);
        assert!(output.text.contains("line 250\n"));
        assert!(output.text.contains("line 300\n"));
        assert!(!output.text.contains("lines to end"));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_bounds_open_ended_output_before_returning() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-bounded-tail-{}.txt",
            std::process::id()
        ));
        let content = "abcdefghij\n".repeat(10_000);
        fs::write(&path, content).unwrap();
        let arguments = json!({ "path": path, "start_line": 1, "line_numbers": false });
        let max_output_bytes = 256;
        let context = AgentRunContext {
            max_tool_output_bytes: max_output_bytes,
            ..AgentRunContext::default()
        };

        let output = ReadFileTool::default()
            .execute(&arguments, &context)
            .unwrap();

        assert!(output.text.len() <= max_output_bytes);
        assert!(output.text.contains("[truncated"));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_bounds_a_single_line_before_returning() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-bounded-line-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "Ж".repeat(100_000)).unwrap();
        let arguments = json!({ "path": path, "start_line": 1, "line_numbers": false });
        let max_output_bytes = 256;
        let context = AgentRunContext {
            max_tool_output_bytes: max_output_bytes,
            ..AgentRunContext::default()
        };

        let output = ReadFileTool::default()
            .execute(&arguments, &context)
            .unwrap();

        assert!(output.text.len() <= max_output_bytes);
        assert!(output.text.contains("[truncated"));
        assert!(output.text.is_char_boundary(output.text.len()));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_returns_empty_text_when_start_line_is_past_eof() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-past-eof-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let arguments = json!({ "path": path, "start_line": 10, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_limits_default_read_to_200_lines() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-default-limit-{}.txt",
            std::process::id()
        ));
        let content = (1..=201)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(&path, content).unwrap();
        let arguments = json!({ "path": path, "line_numbers": false });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(
            output.text.lines().count(),
            DEFAULT_READ_FILE_END_LINE - DEFAULT_READ_FILE_START_LINE + 1
        );
        assert!(output.text.contains("line 200\n"));
        assert!(!output.text.contains("line 201\n"));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_reads_line_chunk() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-chunk-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        let arguments = json!({
            "path": path,
            "start_line": 2,
            "end_line": 3,
            "line_numbers": false,
        });

        let output = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert_eq!(output.text, "two\nthree\n");
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_rejects_image_when_image_input_is_disabled() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-img-disabled-{}.png",
            std::process::id()
        ));
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0]));
        image.save(&path).unwrap();
        let arguments = json!({ "path": path });

        let err = ReadFileTool::default()
            .execute(&arguments, &AgentRunContext::default())
            .unwrap_err();

        assert!(err.to_string().contains("image input is disabled"));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_reads_image_when_image_input_is_enabled() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-read-img-enabled-{}.png",
            std::process::id()
        ));
        let image = image::RgbImage::from_pixel(4, 2, image::Rgb([255, 0, 0]));
        image.save(&path).unwrap();
        let arguments = json!({ "path": path });
        let context = AgentRunContext {
            image_input: ImageInputSettings {
                enable: true,
                max_width: 2,
                max_height: 2,
            },
            ..Default::default()
        };

        let output = ReadFileTool::default()
            .execute(&arguments, &context)
            .unwrap();

        assert!(output.text.contains("image/jpeg, 2x1"));
        let [
            ToolAttachment::Image {
                mime_type,
                width,
                height,
                data_url,
                ..
            },
        ] = output.attachments.as_slice()
        else {
            panic!("expected one image attachment");
        };
        assert_eq!(mime_type, "image/jpeg");
        assert_eq!((*width, *height), (2, 1));
        assert!(data_url.starts_with("data:image/jpeg;base64,"));
        fs::remove_file(string_arg(&arguments, "path").unwrap()).unwrap();
    }

    #[test]
    fn read_file_tool_description_mentions_images_only_when_enabled() {
        let disabled = ReadFileTool::default().schema().to_string();
        let enabled = ReadFileTool::new(ImageInputSettings {
            enable: true,
            max_width: 640,
            max_height: 640,
        })
        .schema()
        .to_string();

        assert!(!disabled.contains("local image"));
        assert!(enabled.contains("local image"));
    }

    #[test]
    fn read_file_tool_rejects_invalid_line_arguments() {
        let output = ReadFileTool::default().execute(
            &json!({
                "path": "Cargo.toml",
                "start_line": 0,
            }),
            &AgentRunContext::default(),
        );

        assert!(output.unwrap_err().to_string().contains("start_line"));
    }

    #[test]
    fn read_file_tool_rejects_end_line_before_start_line() {
        let output = ReadFileTool::default().execute(
            &json!({
                "path": "Cargo.toml",
                "start_line": 10,
                "end_line": 9,
            }),
            &AgentRunContext::default(),
        );

        assert!(output.unwrap_err().to_string().contains("end_line"));
    }

    #[test]
    fn formats_read_file_tool_call_with_line_range() {
        let display = ReadFileTool::default().display(
            &json!({
                "path": "src/input/mod.rs",
                "start_line": 9,
                "end_line": 30,
            }),
            &AgentRunContext::default(),
        );

        assert_eq!(display, "• \x1b[1mread_file\x1b[0m src/input/mod.rs:9-30");
    }

    #[test]
    fn formats_read_file_tool_call_with_implicit_200_line_window() {
        let display = ReadFileTool::default().display(
            &json!({
                "path": "src/input/mod.rs",
                "start_line": 9,
            }),
            &AgentRunContext::default(),
        );

        assert_eq!(display, "• \x1b[1mread_file\x1b[0m src/input/mod.rs:9-208");
    }

    #[test]
    fn formats_read_file_tool_call_for_image_without_line_range() {
        let display = ReadFileTool::default().display(
            &json!({
                "path": "/tmp/photo.jpeg",
            }),
            &AgentRunContext::default(),
        );

        assert_eq!(display, "• \x1b[1mread_file\x1b[0m /tmp/photo.jpeg");
    }
}

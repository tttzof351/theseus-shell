use std::{fs, io, io::Cursor, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use image::{GenericImageView, ImageReader, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};

use super::{
    AgentTool, DEFAULT_READ_FILE_END_LINE, DEFAULT_READ_FILE_START_LINE, ToolAttachment,
    ToolOutput,
    args::optional_usize_arg,
    format_tool_call_name,
    paths::{expanded_path_arg, expanded_path_arg_with_home},
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
                            "description": "1-based last line number to read, inclusive. Defaults to 200."
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

    fn display(&self, arguments: &Value) -> String {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return format_tool_call_name(self.name());
        };
        if is_supported_image_path(Path::new(path)) {
            return format!("{} {path}", format_tool_call_name(self.name()));
        }

        let start_line = arguments
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_FILE_START_LINE as u64);
        let end_line = arguments
            .get("end_line")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_FILE_END_LINE as u64);

        if end_line < start_line {
            format!("{} {path}:empty", format_tool_call_name(self.name()))
        } else {
            format!(
                "{} {path}:{start_line}-{end_line}",
                format_tool_call_name(self.name())
            )
        }
    }

    fn execute(&self, arguments: &Value, context: &AgentRunContext) -> io::Result<ToolOutput> {
        read_file(arguments, &context.image_input)
    }
}

fn read_file(arguments: &Value, image_input: &ImageInputSettings) -> io::Result<ToolOutput> {
    read_file_with_home(arguments, None, image_input)
}

fn read_file_with_home(
    arguments: &Value,
    home_dir: Option<&Path>,
    image_input: &ImageInputSettings,
) -> io::Result<ToolOutput> {
    let path = match home_dir {
        Some(home_dir) => expanded_path_arg_with_home(arguments, "path", Some(home_dir))?,
        None => expanded_path_arg(arguments, "path")?,
    };
    if is_supported_image_path(&path) {
        if !image_input.enable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "image input is disabled",
            ));
        }
        return read_image_file(&path, image_input);
    }

    let content = fs::read_to_string(path)?;
    let start_line =
        optional_usize_arg(arguments, "start_line")?.unwrap_or(DEFAULT_READ_FILE_START_LINE);
    let end_line = optional_usize_arg(arguments, "end_line")?.unwrap_or(DEFAULT_READ_FILE_END_LINE);
    if start_line == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "read_file start_line must be greater than or equal to 1",
        ));
    }
    if end_line < start_line {
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
        &content,
        start_line,
        end_line,
        line_numbers,
    )))
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
    content: &str,
    start_line: usize,
    end_line: usize,
    line_numbers: bool,
) -> String {
    let max_lines = end_line - start_line + 1;
    content
        .split_inclusive('\n')
        .skip(start_line - 1)
        .take(max_lines)
        .enumerate()
        .map(|(index, line)| {
            let line_no = start_line + index;
            if line_numbers {
                format!("{:>6}│ {line}", line_no)
            } else {
                line.to_string()
            }
        })
        .collect()
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

        let output =
            read_file_with_home(&arguments, Some(&home), &ImageInputSettings::default()).unwrap();

        assert_eq!(output.text, "{\"model\":\"test\"}\n");
        fs::remove_dir_all(home).unwrap();
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
        let display = ReadFileTool::default().display(&json!({
            "path": "src/input/mod.rs",
            "start_line": 9,
            "end_line": 30,
        }));

        assert_eq!(display, "• \x1b[1mread_file\x1b[0m src/input/mod.rs:9-30");
    }

    #[test]
    fn formats_read_file_tool_call_for_image_without_line_range() {
        let display = ReadFileTool::default().display(&json!({
            "path": "/tmp/photo.jpeg",
        }));

        assert_eq!(display, "• \x1b[1mread_file\x1b[0m /tmp/photo.jpeg");
    }
}

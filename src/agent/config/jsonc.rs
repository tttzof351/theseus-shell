use std::{collections::BTreeMap, io};

use jsonc_parser::{ParseOptions, parse_to_serde_value};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use super::{
    AgentConfig, AgentSettings, ImageInputSettings, LlmRequestSettings, McpServerConfig,
    McpTransport, default_compact_prompt, models,
};

const DEFAULT_MCP_TIMEOUT_SECONDS: usize = 60;

impl AgentConfig {
    pub(super) fn to_jsonc(&self) -> String {
        format!(
            "{{\n  \"llm_request_settings\": {},\n  \"agent_settings\": {},\n  \"mcp_servers\": {}\n}}\n",
            llm_request_settings_jsonc(&self.llm_request_settings),
            agent_settings_jsonc(&self.agent_settings),
            mcp_servers_jsonc(&self.mcp_servers),
        )
    }

    pub(super) fn from_jsonc(text: &str) -> io::Result<Self> {
        let value = parse_to_serde_value::<Value>(text, &jsonc_parse_options()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config must be valid JSONC: {err}"),
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "config root must be an object")
        })?;

        Ok(Self {
            agent_settings: read_agent_settings(object)?,
            llm_request_settings: read_llm_request_settings(object)?,
            mcp_servers: read_mcp_servers(object)?,
        })
    }
}

pub(super) fn jsonc_parse_options() -> ParseOptions {
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

fn llm_request_settings_jsonc(settings: &LlmRequestSettings) -> String {
    format!(
        "{{\n    \"base_url\": {},\n    \"retries\": {},\n    \"connect_timeout_seconds\": {},\n    \"request_timeout_seconds\": {},\n    \"body\": {},\n    \"header\": {}\n  }}",
        pretty_json(&json!(settings.base_url)),
        settings.retries,
        settings.connect_timeout_seconds,
        settings.request_timeout_seconds,
        pretty_json_indented(&Value::Object(settings.body.clone()), "      "),
        pretty_json_indented(&json!(settings.header), "      "),
    )
}

fn agent_settings_jsonc(settings: &AgentSettings) -> String {
    format!(
        "{{\n    \"max_tool_output_bytes\": {},\n    \"max_tool_bash_bytes\": {},\n    \"max_turns\": {},\n    \"max_context_tokens\": {},\n    \"compact_token_overdraft\": {},\n    \"compact_recent_user_messages_max_bytes\": {},\n    \"compact_trim_retry_limit\": {},\n    \"max_resume_traj\": {},\n    \"tmp_files_ttl_min\": {},\n    \"system_prompt\": {},\n    \"compact_prompt\": {},\n    \"build_in_tools\": {},\n    \"image_input\": {}\n  }}",
        settings.max_tool_output_bytes,
        settings.max_tool_bash_bytes,
        settings.max_turns,
        settings.max_context_tokens,
        settings.compact_token_overdraft,
        settings.compact_recent_user_messages_max_bytes,
        settings.compact_trim_retry_limit,
        settings.max_resume_traj,
        settings.tmp_files_ttl_min,
        pretty_json_indented(&json!(settings.system_prompt), "      "),
        pretty_json_indented(&json!(settings.compact_prompt), "      "),
        pretty_json_indented(&json!(settings.build_in_tools), "      "),
        image_input_settings_jsonc(&settings.image_input),
    )
}

fn image_input_settings_jsonc(settings: &ImageInputSettings) -> String {
    format!(
        "{{\n      \"enable\": {},\n      \"max_width\": {},\n      \"max_height\": {}\n    }}",
        settings.enable, settings.max_width, settings.max_height,
    )
}

fn mcp_servers_jsonc(servers: &BTreeMap<String, McpServerConfig>) -> String {
    let mut result = String::from("{");
    if !servers.is_empty() {
        result.push('\n');
    }

    for (index, (server_id, server)) in servers.iter().enumerate() {
        if index > 0 {
            result.push_str(",\n");
        }
        result.push_str(&format!(
            "    {}: {}",
            pretty_json(&json!(server_id)),
            pretty_json_indented_string(&mcp_server_jsonc(server), "    ")
        ));
    }

    if !servers.is_empty() {
        result.push('\n');
        result.push_str("  ");
    }
    result.push('}');
    result
}

fn mcp_server_jsonc(server: &McpServerConfig) -> String {
    let mut fields = Vec::new();
    match server.transport {
        McpTransport::Stdio => {
            fields.push(format!(
                "\"command\": {}",
                pretty_json(&json!(server.command.as_deref().unwrap_or_default()))
            ));
            if !server.args.is_empty() {
                fields.push(format!(
                    "\"args\": {}",
                    pretty_json_indented(&json!(server.args), "  ")
                ));
            }
            if !server.env.is_empty() {
                fields.push(format!(
                    "\"env\": {}",
                    pretty_json_indented(&json!(server.env), "  ")
                ));
            }
        }
        McpTransport::StreamableHttp => {
            fields.push("\"type\": \"http\"".to_string());
            fields.push(format!(
                "\"url\": {}",
                pretty_json(&json!(server.url.as_deref().unwrap_or_default()))
            ));
            if !server.headers.is_empty() {
                fields.push(format!(
                    "\"headers\": {}",
                    pretty_json_indented(&json!(server.headers), "  ")
                ));
            }
        }
    }

    if server.tools != default_mcp_tools() {
        fields.push(format!(
            "\"tools\": {}",
            pretty_json_indented(&json!(server.tools), "  ")
        ));
    }
    if server.timeout_seconds != DEFAULT_MCP_TIMEOUT_SECONDS {
        fields.push(format!("\"timeout\": {}", server.timeout_seconds * 1000));
    }
    if !server.enabled {
        fields.push("\"disabled\": true".to_string());
    }

    let body = fields
        .into_iter()
        .map(|field| format!("  {field}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}\n}}")
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("config serializes")
}

fn pretty_json_indented(value: &Value, indent: &str) -> String {
    pretty_json_indented_string(&pretty_json(value), indent)
}

fn pretty_json_indented_string(value: &str, indent: &str) -> String {
    value
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.to_string()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_agent_settings(object: &Map<String, Value>) -> io::Result<AgentSettings> {
    let settings = read_object_ref(object, "agent_settings")?;
    let build_in_tools = read_string_array_field(settings, "build_in_tools")?;
    validate_build_in_tools(&build_in_tools)?;

    let compact_token_overdraft = read_optional_usize_field(settings, "compact_token_overdraft")?
        .unwrap_or(models::DEFAULT_COMPACT_TOKEN_OVERDRAFT);
    validate_compact_token_overdraft(compact_token_overdraft)?;
    let compact_recent_user_messages_max_bytes =
        read_optional_usize_field(settings, "compact_recent_user_messages_max_bytes")?
            .unwrap_or(models::DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES);
    validate_compact_recent_user_messages_max_bytes(compact_recent_user_messages_max_bytes)?;
    let compact_trim_retry_limit = read_optional_usize_field(settings, "compact_trim_retry_limit")?
        .unwrap_or(models::DEFAULT_COMPACT_TRIM_RETRY_LIMIT);
    validate_compact_trim_retry_limit(compact_trim_retry_limit)?;
    let max_tool_bash_bytes = read_optional_usize_field(settings, "max_tool_bash_bytes")?
        .unwrap_or(models::DEFAULT_MAX_TOOL_BASH_BYTES);
    let tmp_files_ttl_min = read_optional_usize_field(settings, "tmp_files_ttl_min")?
        .unwrap_or(models::DEFAULT_TMP_FILES_TTL_MIN);

    Ok(AgentSettings {
        max_turns: read_usize_field(settings, "max_turns")?,
        max_tool_output_bytes: read_usize_field(settings, "max_tool_output_bytes")?,
        max_tool_bash_bytes,
        max_context_tokens: read_usize_field(settings, "max_context_tokens")?,
        compact_token_overdraft,
        compact_recent_user_messages_max_bytes,
        compact_trim_retry_limit,
        max_resume_traj: read_usize_field(settings, "max_resume_traj")?,
        tmp_files_ttl_min,
        build_in_tools,
        image_input: read_image_input_settings(settings)?,
        system_prompt: read_string_array_field(settings, "system_prompt")?,
        compact_prompt: read_optional_string_array_field(settings, "compact_prompt")?
            .unwrap_or_else(default_compact_prompt),
    })
}

fn read_image_input_settings(settings: &Map<String, Value>) -> io::Result<ImageInputSettings> {
    let Some(value) = settings.get("image_input") else {
        return Ok(ImageInputSettings::default());
    };
    let object = value.as_object().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "config field `agent_settings.image_input` must be an object",
        )
    })?;

    let image_input = ImageInputSettings {
        enable: read_bool_field(object, "enable")?,
        max_width: read_usize_field(object, "max_width")?,
        max_height: read_usize_field(object, "max_height")?,
    };
    validate_image_input_settings(&image_input)?;
    Ok(image_input)
}

fn validate_image_input_settings(settings: &ImageInputSettings) -> io::Result<()> {
    const MAX_IMAGE_DIMENSION: usize = 4096;

    if settings.max_width == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config field `agent_settings.image_input.max_width` must be greater than 0",
        ));
    }
    if settings.max_height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config field `agent_settings.image_input.max_height` must be greater than 0",
        ));
    }
    if settings.max_width > MAX_IMAGE_DIMENSION || settings.max_height > MAX_IMAGE_DIMENSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `agent_settings.image_input` dimensions must be at most {MAX_IMAGE_DIMENSION}"
            ),
        ));
    }

    Ok(())
}

fn validate_compact_token_overdraft(value: usize) -> io::Result<()> {
    const MAX_COMPACT_TOKEN_OVERDRAFT: usize = 262_144;

    if value > MAX_COMPACT_TOKEN_OVERDRAFT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `agent_settings.compact_token_overdraft` must be at most {MAX_COMPACT_TOKEN_OVERDRAFT}"
            ),
        ));
    }

    Ok(())
}

fn validate_compact_recent_user_messages_max_bytes(value: usize) -> io::Result<()> {
    const MAX_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES: usize = 1024 * 1024;

    if value > MAX_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `agent_settings.compact_recent_user_messages_max_bytes` must be at most {MAX_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES}"
            ),
        ));
    }

    Ok(())
}

fn validate_compact_trim_retry_limit(value: usize) -> io::Result<()> {
    const MAX_COMPACT_TRIM_RETRY_LIMIT: usize = 64;

    if value > MAX_COMPACT_TRIM_RETRY_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `agent_settings.compact_trim_retry_limit` must be at most {MAX_COMPACT_TRIM_RETRY_LIMIT}"
            ),
        ));
    }

    Ok(())
}

fn validate_build_in_tools(build_in_tools: &[String]) -> io::Result<()> {
    let known_tools = super::super::tools::built_in_tool_names();
    for tool_name in build_in_tools {
        if !known_tools.contains(tool_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "config field `agent_settings.build_in_tools` contains unknown tool `{tool_name}`"
                ),
            ));
        }
    }

    Ok(())
}

fn read_llm_request_settings(object: &Map<String, Value>) -> io::Result<LlmRequestSettings> {
    let settings = read_object_ref(object, "llm_request_settings")?;

    Ok(LlmRequestSettings {
        base_url: read_string_field(settings, "base_url")?,
        retries: read_usize_field(settings, "retries")?,
        request_timeout_seconds: read_usize_field(settings, "request_timeout_seconds")?,
        connect_timeout_seconds: read_usize_field(settings, "connect_timeout_seconds")?,
        body: read_object_field(settings, "body")?,
        header: read_string_map_field(settings, "header")?,
    })
}

fn read_mcp_servers(object: &Map<String, Value>) -> io::Result<BTreeMap<String, McpServerConfig>> {
    read_object_ref(object, "mcp_servers")?
        .iter()
        .map(|(server_id, value)| {
            validate_mcp_server_id(server_id)?;
            let server = value.as_object().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `mcp_servers.{server_id}` must be an object"),
                )
            })?;
            Ok((server_id.clone(), read_mcp_server(server_id, server)?))
        })
        .collect()
}

fn read_mcp_server(server_id: &str, object: &Map<String, Value>) -> io::Result<McpServerConfig> {
    reject_legacy_mcp_fields(server_id, object)?;
    reject_unsupported_mcp_fields(server_id, object)?;
    let transport = read_mcp_transport(object, server_id)?;
    let enabled = !read_optional_bool_field(object, "disabled")?.unwrap_or(false);
    let tools =
        read_optional_string_array_field(object, "tools")?.unwrap_or_else(default_mcp_tools);
    let timeout_seconds = read_optional_mcp_timeout_seconds(object, "timeout")?
        .unwrap_or(DEFAULT_MCP_TIMEOUT_SECONDS);

    match transport {
        McpTransport::Stdio => {
            if object.contains_key("url") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "config field `mcp_servers.{server_id}.url` cannot be used with stdio MCP servers"
                    ),
                ));
            }
            if object.contains_key("headers") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "config field `mcp_servers.{server_id}.headers` cannot be used with stdio MCP servers"
                    ),
                ));
            }
            Ok(McpServerConfig {
                enabled,
                transport,
                command: Some(read_string_field(object, "command")?),
                args: read_optional_string_array_field(object, "args")?.unwrap_or_default(),
                env: read_optional_string_map_field(object, "env")?,
                url: None,
                headers: BTreeMap::new(),
                tools,
                timeout_seconds,
            })
        }
        McpTransport::StreamableHttp => {
            if object.contains_key("command") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "config field `mcp_servers.{server_id}.command` cannot be used with http MCP servers"
                    ),
                ));
            }
            for field in ["args", "env"] {
                if object.contains_key(field) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "config field `mcp_servers.{server_id}.{field}` cannot be used with http MCP servers"
                        ),
                    ));
                }
            }
            Ok(McpServerConfig {
                enabled,
                transport,
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some(read_string_field(object, "url")?),
                headers: read_optional_header_map_field(object, "headers")?,
                tools,
                timeout_seconds,
            })
        }
    }
}

fn read_mcp_transport(object: &Map<String, Value>, server_id: &str) -> io::Result<McpTransport> {
    let transport = object.get("type").map(|value| {
        value.as_str().map(str::to_string).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config field `mcp_servers.{server_id}.type` must be a string"),
            )
        })
    });
    let has_command = object.contains_key("command");
    let has_url = object.contains_key("url");

    let Some(transport) = transport else {
        if has_command && has_url {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "config field `mcp_servers.{server_id}` is ambiguous: specify `type` when both `command` and `url` are present"
                ),
            ));
        }
        if has_command {
            return Ok(McpTransport::Stdio);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `mcp_servers.{server_id}` must contain `command` for stdio or `type` for http"
            ),
        ));
    };

    let transport = transport?;
    match transport.as_str() {
        "stdio" => Ok(McpTransport::Stdio),
        "http" | "streamable-http" => Ok(McpTransport::StreamableHttp),
        "sse" => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `mcp_servers.{server_id}.type` contains unsupported transport `sse`; use `http` instead"
            ),
        )),
        "ws" | "websocket" => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `mcp_servers.{server_id}.type` contains unsupported transport `{transport}`"
            ),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "config field `mcp_servers.{server_id}.type` contains unsupported transport `{}`",
                transport
            ),
        )),
    }
}

fn reject_legacy_mcp_fields(server_id: &str, object: &Map<String, Value>) -> io::Result<()> {
    for field in ["enabled", "transport", "timeout_seconds"] {
        if object.contains_key(field) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "config field `mcp_servers.{server_id}.{field}` is no longer supported; use Claude-like MCP fields"
                ),
            ));
        }
    }

    Ok(())
}

fn reject_unsupported_mcp_fields(server_id: &str, object: &Map<String, Value>) -> io::Result<()> {
    for field in ["oauth", "headersHelper", "alwaysLoad", "channels"] {
        if object.contains_key(field) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "config field `mcp_servers.{server_id}.{field}` is not supported by Theseus"
                ),
            ));
        }
    }

    Ok(())
}

fn default_mcp_tools() -> Vec<String> {
    vec!["*".to_string()]
}

fn validate_mcp_server_id(server_id: &str) -> io::Result<()> {
    let valid = !server_id.is_empty()
        && server_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');

    if valid {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "config field `mcp_servers` contains invalid server id `{server_id}`; allowed characters are [a-zA-Z0-9_-]"
        ),
    ))
}

fn read_string_field(object: &Map<String, Value>, key: &str) -> io::Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config field `{key}` must be a string"),
            )
        })
}

fn read_optional_string_array_field(
    object: &Map<String, Value>,
    key: &str,
) -> io::Result<Option<Vec<String>>> {
    if object.get(key).is_none() {
        return Ok(None);
    }

    read_string_array_field(object, key).map(Some)
}

fn read_string_array_field(object: &Map<String, Value>, key: &str) -> io::Result<Vec<String>> {
    let values = object.get(key).and_then(Value::as_array).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be an array of strings"),
        )
    })?;

    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}[{index}]` must be a string"),
                )
            })
        })
        .collect()
}

fn read_optional_string_map_field(
    object: &Map<String, Value>,
    key: &str,
) -> io::Result<BTreeMap<String, String>> {
    match object.get(key) {
        Some(Value::Object(values)) => values
            .iter()
            .map(|(name, value)| {
                let value = value.as_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("config field `{key}.{name}` must be a string"),
                    )
                })?;
                Ok((name.clone(), value.to_string()))
            })
            .collect(),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be an object"),
        )),
        None => Ok(BTreeMap::new()),
    }
}

fn read_optional_bool_field(object: &Map<String, Value>, key: &str) -> io::Result<Option<bool>> {
    match object.get(key) {
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be a boolean"),
        )),
        None => Ok(None),
    }
}

fn read_bool_field(object: &Map<String, Value>, key: &str) -> io::Result<bool> {
    object.get(key).and_then(Value::as_bool).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be a boolean"),
        )
    })
}

fn read_optional_mcp_timeout_seconds(
    object: &Map<String, Value>,
    key: &str,
) -> io::Result<Option<usize>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let milliseconds = read_usize_value(key, value)?;
    if milliseconds < 1000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be at least 1000 milliseconds"),
        ));
    }
    if milliseconds % 1000 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be a whole number of seconds in milliseconds"),
        ));
    }

    Ok(Some(milliseconds / 1000))
}

fn read_optional_header_map_field(
    object: &Map<String, Value>,
    key: &str,
) -> io::Result<BTreeMap<String, String>> {
    if object.get(key).is_none() {
        return Ok(BTreeMap::new());
    }

    read_string_map_field(object, key)
}

fn read_usize_field(object: &Map<String, Value>, key: &str) -> io::Result<usize> {
    let value = object.get(key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("missing config field `{key}`"),
        )
    })?;
    read_usize_value(key, value)
}

fn read_optional_usize_field(object: &Map<String, Value>, key: &str) -> io::Result<Option<usize>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    read_usize_value(key, value).map(Some)
}

fn read_usize_value(key: &str, value: &Value) -> io::Result<usize> {
    let number = value.as_u64().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be an unsigned integer"),
        )
    })?;

    usize::try_from(number).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must fit in usize: {err}"),
        )
    })
}

fn read_object_field(object: &Map<String, Value>, key: &str) -> io::Result<Map<String, Value>> {
    Ok(read_object_ref(object, key)?.clone())
}

fn read_object_ref<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> io::Result<&'a Map<String, Value>> {
    object.get(key).and_then(Value::as_object).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be an object"),
        )
    })
}

fn read_string_map_field(
    object: &Map<String, Value>,
    key: &str,
) -> io::Result<BTreeMap<String, String>> {
    read_object_ref(object, key)?
        .iter()
        .map(|(header, value)| {
            HeaderName::from_bytes(header.as_bytes()).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}` contains invalid header name `{header}`: {err}"),
                )
            })?;
            let value = value.as_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}.{header}` must be a string"),
                )
            })?;
            HeaderValue::from_str(value).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}.{header}` must be a valid header value: {err}"),
                )
            })?;
            Ok((header.clone(), value.to_string()))
        })
        .collect()
}

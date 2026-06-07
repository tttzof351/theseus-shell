use std::{collections::BTreeMap, path::PathBuf};

use serde_json::{Map, Value, json};

mod document;
mod interactive;
mod jsonc;
mod model_catalog;
pub(crate) mod models;
mod store;

pub use store::default_config_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub agent_settings: AgentSettings,
    pub llm_request_settings: LlmRequestSettings,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    pub max_turns: usize,
    pub max_tool_output_bytes: usize,
    pub max_tool_bash_bytes: usize,
    pub max_context_tokens: usize,
    pub compact_token_overdraft: usize,
    pub compact_recent_user_messages_max_bytes: usize,
    pub compact_trim_retry_limit: usize,
    pub max_resume_traj: usize,
    pub tmp_files_ttl_min: usize,
    pub build_in_tools: Vec<String>,
    pub image_input: ImageInputSettings,
    pub system_prompt: Vec<String>,
    pub compact_prompt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInputSettings {
    pub enable: bool,
    pub max_width: usize,
    pub max_height: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequestSettings {
    pub base_url: String,
    pub retries: usize,
    pub request_timeout_seconds: usize,
    pub connect_timeout_seconds: usize,
    pub body: Map<String, Value>,
    pub header: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub tools: Vec<String>,
    pub timeout_seconds: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInit {
    pub config: AgentConfig,
    pub path: PathBuf,
    pub created: bool,
}

impl AgentConfig {
    pub(crate) fn default_empty() -> Self {
        let mut body = Map::new();
        body.insert("model".to_string(), json!(models::DEFAULT_MODEL));
        body.insert("tool_choice".to_string(), json!("auto"));
        body.insert("parallel_tool_calls".to_string(), json!(true));
        body.insert("include_reasoning".to_string(), json!(true));
        body.insert(
            "max_tokens".to_string(),
            json!(models::DEFAULT_MAX_COMPLETION_TOKENS),
        );

        let mut header = BTreeMap::new();
        header.insert("Authorization".to_string(), String::new());
        header.insert("Content-Type".to_string(), "application/json".to_string());

        Self {
            agent_settings: AgentSettings {
                max_turns: models::DEFAULT_MAX_AGENT_TURNS,
                max_tool_output_bytes: models::DEFAULT_MAX_TOOL_OUTPUT_BYTES,
                max_tool_bash_bytes: models::DEFAULT_MAX_TOOL_BASH_BYTES,
                max_context_tokens: models::DEFAULT_MAX_CONTEXT_TOKENS,
                compact_token_overdraft: models::DEFAULT_COMPACT_TOKEN_OVERDRAFT,
                compact_recent_user_messages_max_bytes:
                    models::DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES,
                compact_trim_retry_limit: models::DEFAULT_COMPACT_TRIM_RETRY_LIMIT,
                max_resume_traj: models::DEFAULT_MAX_RESUME_TRAJ,
                tmp_files_ttl_min: models::DEFAULT_TMP_FILES_TTL_MIN,
                build_in_tools: super::tools::built_in_tool_names(),
                image_input: ImageInputSettings::default(),
                system_prompt: default_system_prompt(),
                compact_prompt: default_compact_prompt(),
            },
            llm_request_settings: LlmRequestSettings {
                base_url: models::DEFAULT_BASE_URL.to_string(),
                retries: models::DEFAULT_LLM_REQUEST_RETRIES,
                request_timeout_seconds: models::DEFAULT_LLM_REQUEST_TIMEOUT_SECONDS,
                connect_timeout_seconds: models::DEFAULT_LLM_CONNECT_TIMEOUT_SECONDS,
                body,
                header,
            },
            mcp_servers: BTreeMap::new(),
        }
    }
}

impl Default for ImageInputSettings {
    fn default() -> Self {
        Self {
            enable: false,
            max_width: 640,
            max_height: 640,
        }
    }
}

pub(super) fn default_system_prompt() -> Vec<String> {
    [
        "You are an autonomous coding agent running inside the theseus shell.",
        "Use tools when you need local context or need to make a change.",
        "Prefer small, explicit steps. After tool use, explain the result plainly.",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub(super) fn default_compact_prompt() -> Vec<String> {
    [
        "You are performing a CONTEXT CHECKPOINT COMPACTION.",
        "Create a concise handoff summary for the same coding agent that will continue this session.",
        "",
        "Include:",
        "- current user goal and active task;",
        "- repository and files touched;",
        "- environment details relevant to the task: OS, shell, working directory, toolchain, commands run, logs/trajectory paths, config values, and any external files referenced;",
        "- code context in enough detail to continue without rereading everything: modules, functions, structs/enums, data flow, contracts, invariants, and important line-level references when known;",
        "- implemented changes and key decisions;",
        "- constraints, user preferences, and rejected approaches;",
        "- current test/build status;",
        "- unresolved blockers, risks, and exact next steps;",
        "- important paths, commands, config values, or error messages needed to continue.",
        "",
        "Do not invent results. If something is unknown, say so.",
        "Keep the summary structured and compact.",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, env, fs};

    use serde_json::json;

    use super::{
        AgentConfig, AgentSettings, ImageInputSettings, LlmRequestSettings, McpServerConfig,
        McpTransport, default_compact_prompt, default_system_prompt, models,
    };

    #[test]
    fn creates_default_config_file() {
        let path = env::temp_dir().join(format!(
            "theseus-config-create-{}.jsonc",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);

        let init = AgentConfig::load_or_create_at(&path).unwrap();

        assert!(init.created);
        assert_eq!(
            init.config.llm_request_settings.base_url,
            models::DEFAULT_BASE_URL
        );
        assert_eq!(
            init.config.llm_request_settings.body.get("model"),
            Some(&json!(models::DEFAULT_MODEL))
        );
        assert_eq!(
            init.config.llm_request_settings.body.get("tool_choice"),
            Some(&json!("auto"))
        );
        assert_eq!(
            init.config
                .llm_request_settings
                .body
                .get("parallel_tool_calls"),
            Some(&json!(true))
        );
        assert_eq!(
            init.config
                .llm_request_settings
                .body
                .get("include_reasoning"),
            Some(&json!(true))
        );
        assert_eq!(
            init.config.llm_request_settings.body.get("max_tokens"),
            Some(&json!(models::DEFAULT_MAX_COMPLETION_TOKENS))
        );
        assert_eq!(
            init.config.llm_request_settings.header.get("Authorization"),
            Some(&String::new())
        );
        assert_eq!(
            init.config.llm_request_settings.header.get("Content-Type"),
            Some(&"application/json".to_string())
        );
        assert_eq!(
            init.config.agent_settings.max_turns,
            models::DEFAULT_MAX_AGENT_TURNS
        );
        assert_eq!(
            init.config.agent_settings.max_tool_output_bytes,
            models::DEFAULT_MAX_TOOL_OUTPUT_BYTES
        );
        assert_eq!(
            init.config.agent_settings.max_tool_bash_bytes,
            models::DEFAULT_MAX_TOOL_BASH_BYTES
        );
        assert_eq!(
            init.config.agent_settings.max_context_tokens,
            models::DEFAULT_MAX_CONTEXT_TOKENS
        );
        assert_eq!(
            init.config.agent_settings.compact_token_overdraft,
            models::DEFAULT_COMPACT_TOKEN_OVERDRAFT
        );
        assert_eq!(
            init.config
                .agent_settings
                .compact_recent_user_messages_max_bytes,
            models::DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES
        );
        assert_eq!(
            init.config.agent_settings.compact_trim_retry_limit,
            models::DEFAULT_COMPACT_TRIM_RETRY_LIMIT
        );
        assert_eq!(
            init.config.agent_settings.max_resume_traj,
            models::DEFAULT_MAX_RESUME_TRAJ
        );
        assert_eq!(
            init.config.agent_settings.tmp_files_ttl_min,
            models::DEFAULT_TMP_FILES_TTL_MIN
        );
        assert_eq!(
            init.config.agent_settings.build_in_tools,
            ["read_file", "write_file", "edit_file", "bash"]
        );
        assert_eq!(
            init.config.agent_settings.image_input,
            ImageInputSettings::default()
        );
        assert_eq!(
            init.config.agent_settings.system_prompt,
            default_system_prompt()
        );
        assert_eq!(
            init.config.agent_settings.compact_prompt,
            default_compact_prompt()
        );
        assert_eq!(
            init.config.llm_request_settings.retries,
            models::DEFAULT_LLM_REQUEST_RETRIES
        );
        assert_eq!(
            init.config.llm_request_settings.request_timeout_seconds,
            models::DEFAULT_LLM_REQUEST_TIMEOUT_SECONDS
        );
        assert_eq!(
            init.config.llm_request_settings.connect_timeout_seconds,
            models::DEFAULT_LLM_CONNECT_TIMEOUT_SECONDS
        );
        assert!(init.config.mcp_servers.is_empty());
        assert!(path.exists());

        let loaded = AgentConfig::load_or_create_at(&path).unwrap();
        assert!(!loaded.created);
        assert_eq!(loaded.config, init.config);

        let text = fs::read_to_string(&path).unwrap();
        assert_field_order(
            &text,
            &[
                "\"llm_request_settings\"",
                "\"agent_settings\"",
                "\"mcp_servers\"",
            ],
        );
        assert_field_order(
            &text,
            &[
                "\"base_url\"",
                "\"retries\"",
                "\"connect_timeout_seconds\"",
                "\"request_timeout_seconds\"",
                "\"body\"",
                "\"header\"",
            ],
        );
        assert_field_order(
            &text,
            &[
                "\"max_tool_output_bytes\"",
                "\"max_tool_bash_bytes\"",
                "\"max_turns\"",
                "\"max_context_tokens\"",
                "\"compact_token_overdraft\"",
                "\"compact_recent_user_messages_max_bytes\"",
                "\"compact_trim_retry_limit\"",
                "\"max_resume_traj\"",
                "\"tmp_files_ttl_min\"",
                "\"system_prompt\"",
                "\"compact_prompt\"",
                "\"build_in_tools\"",
                "\"image_input\"",
            ],
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_jsonc_config_with_comments() {
        let text = r#"
        {
          // request
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": {
              "model": "test/model",
              "provider": {
                "allow_fallbacks": true
              }
            },
            "header": {
              "Authorization": "Bearer secret",
              "Content-Type": "application/json"
            }
          },
            "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_tool_bash_bytes": 2048,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "tmp_files_ttl_min": 30,
            "build_in_tools": [
              "read_file",
              "bash"
            ],
            "image_input": {
              "enable": true,
              "max_width": 320,
              "max_height": 240
            },
            "system_prompt": [
              "line one",
              "line two"
            ]
          },
          "mcp_servers": {
            "filesystem": {
              "command": "npx",
              "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
              "env": {
                "DEBUG": "1"
              },
              "tools": ["read_file"],
              "timeout": 30000
            }
          }
        }
        "#;

        let config = AgentConfig::from_jsonc(text).unwrap();

        assert_eq!(
            config.llm_request_settings.base_url,
            "https://example.test/chat"
        );
        assert_eq!(
            config.llm_request_settings.body.get("model"),
            Some(&json!("test/model"))
        );
        assert_eq!(
            config.llm_request_settings.body.get("provider"),
            Some(&json!({ "allow_fallbacks": true }))
        );
        assert_eq!(
            config.llm_request_settings.header.get("Authorization"),
            Some(&"Bearer secret".to_string())
        );
        assert_eq!(config.agent_settings.max_turns, 12);
        assert_eq!(config.agent_settings.max_tool_output_bytes, 4096);
        assert_eq!(config.agent_settings.max_tool_bash_bytes, 2048);
        assert_eq!(config.agent_settings.max_context_tokens, 8192);
        assert_eq!(
            config.agent_settings.compact_token_overdraft,
            models::DEFAULT_COMPACT_TOKEN_OVERDRAFT
        );
        assert_eq!(
            config.agent_settings.compact_recent_user_messages_max_bytes,
            models::DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES
        );
        assert_eq!(
            config.agent_settings.compact_trim_retry_limit,
            models::DEFAULT_COMPACT_TRIM_RETRY_LIMIT
        );
        assert_eq!(config.agent_settings.max_resume_traj, 100);
        assert_eq!(config.agent_settings.tmp_files_ttl_min, 30);
        assert_eq!(config.agent_settings.build_in_tools, ["read_file", "bash"]);
        assert_eq!(
            config.agent_settings.image_input,
            ImageInputSettings {
                enable: true,
                max_width: 320,
                max_height: 240,
            }
        );
        assert_eq!(
            config.agent_settings.system_prompt,
            ["line one", "line two"]
        );
        assert_eq!(
            config.agent_settings.compact_prompt,
            default_compact_prompt()
        );
        assert_eq!(config.llm_request_settings.retries, 4);
        assert_eq!(config.llm_request_settings.request_timeout_seconds, 240);
        assert_eq!(config.llm_request_settings.connect_timeout_seconds, 45);
        let filesystem = config.mcp_servers.get("filesystem").unwrap();
        assert!(filesystem.enabled);
        assert_eq!(filesystem.transport, McpTransport::Stdio);
        assert_eq!(filesystem.command.as_deref(), Some("npx"));
        assert_eq!(filesystem.env.get("DEBUG"), Some(&"1".to_string()));
        assert_eq!(filesystem.tools, ["read_file"]);
        assert_eq!(filesystem.timeout_seconds, 30);
    }

    #[test]
    fn keeps_url_slashes_inside_strings() {
        let text = r#"{ "llm_request_settings": { "base_url": "https://example.test/a//b", "retries": 3, "request_timeout_seconds": 180, "connect_timeout_seconds": 30, "body": { "model": "m" }, "header": { "Authorization": "Bearer k", "Content-Type": "application/json" } }, "agent_settings": { "max_turns": 32, "max_tool_output_bytes": 32768, "max_context_tokens": 131072, "max_resume_traj": 100, "build_in_tools": ["read_file"], "system_prompt": ["prompt"] }, "mcp_servers": {} }"#;

        let config = AgentConfig::from_jsonc(text).unwrap();

        assert_eq!(
            config.llm_request_settings.base_url,
            "https://example.test/a//b"
        );
    }

    #[test]
    fn reads_jsonc_config_with_trailing_commas() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": {
              "model": "test/model",
            },
            "header": {
              "Authorization": "Bearer secret",
              "Content-Type": "application/json",
            },
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file",],
            "system_prompt": ["prompt",],
          },
          "mcp_servers": {},
        }
        "#;

        let config = AgentConfig::from_jsonc(text).unwrap();

        assert_eq!(
            config.llm_request_settings.body.get("model"),
            Some(&json!("test/model"))
        );
        assert_eq!(config.agent_settings.build_in_tools, ["read_file"]);
    }

    #[test]
    fn rejects_non_string_header_values() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": 42
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("Authorization"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_unknown_build_in_tools() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file", "unknown_tool"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("unknown_tool"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_invalid_image_input_dimensions() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "image_input": {
              "enable": true,
              "max_width": 640,
              "max_height": 0
            },
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("max_height"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn accepts_zero_compact_token_overdraft() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "compact_token_overdraft": 0,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let config = AgentConfig::from_jsonc(text).unwrap();

        assert_eq!(config.agent_settings.compact_token_overdraft, 0);
    }

    #[test]
    fn accepts_zero_compact_recent_user_messages_and_trim_limit() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "compact_recent_user_messages_max_bytes": 0,
            "compact_trim_retry_limit": 0,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let config = AgentConfig::from_jsonc(text).unwrap();

        assert_eq!(
            config.agent_settings.compact_recent_user_messages_max_bytes,
            0
        );
        assert_eq!(config.agent_settings.compact_trim_retry_limit, 0);
    }

    #[test]
    fn rejects_huge_compact_token_overdraft() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "compact_token_overdraft": 262145,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("compact_token_overdraft"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_huge_compact_recent_user_messages_max_bytes() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "compact_recent_user_messages_max_bytes": 1048577,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(
            err.to_string()
                .contains("compact_recent_user_messages_max_bytes")
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_huge_compact_trim_retry_limit() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": {
              "Authorization": "Bearer secret"
            }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "compact_trim_retry_limit": 65,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {}
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("compact_trim_retry_limit"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn saves_config_file() {
        let path =
            env::temp_dir().join(format!("theseus-config-save-{}.jsonc", std::process::id()));
        let _ = fs::remove_file(&path);
        let config = AgentConfig {
            agent_settings: AgentSettings {
                max_turns: 16,
                max_tool_output_bytes: 8192,
                max_tool_bash_bytes: 4096,
                max_context_tokens: 65_536,
                compact_token_overdraft: 4096,
                compact_recent_user_messages_max_bytes: 16 * 1024,
                compact_trim_retry_limit: 4,
                max_resume_traj: 24,
                tmp_files_ttl_min: 60,
                build_in_tools: vec!["read_file".to_string(), "edit_file".to_string()],
                image_input: ImageInputSettings {
                    enable: true,
                    max_width: 800,
                    max_height: 600,
                },
                system_prompt: vec!["custom".to_string(), "prompt".to_string()],
                compact_prompt: vec!["compact".to_string(), "prompt".to_string()],
            },
            llm_request_settings: LlmRequestSettings {
                base_url: "https://example.test/chat".to_string(),
                retries: 5,
                request_timeout_seconds: 300,
                connect_timeout_seconds: 60,
                body: serde_json::Map::from_iter([
                    ("model".to_string(), json!("minimax/minimax-m2.7")),
                    ("temperature".to_string(), json!(0.2)),
                ]),
                header: std::collections::BTreeMap::from([
                    ("Authorization".to_string(), "Bearer secret".to_string()),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ]),
            },
            mcp_servers: BTreeMap::from([(
                "docs".to_string(),
                McpServerConfig {
                    enabled: false,
                    transport: McpTransport::StreamableHttp,
                    command: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    url: Some("https://example.test/mcp".to_string()),
                    headers: BTreeMap::from([(
                        "Authorization".to_string(),
                        "Bearer token".to_string(),
                    )]),
                    tools: vec!["search".to_string()],
                    timeout_seconds: 45,
                },
            )]),
        };

        config.save_at(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"type\": \"http\""));
        assert!(text.contains("\"timeout\": 45000"));
        assert!(text.contains("\"disabled\": true"));
        assert!(!text.contains("\"enabled\""));
        assert!(!text.contains("\"transport\""));
        assert!(!text.contains("\"timeout_seconds\""));
        let loaded = AgentConfig::load_or_create_at(&path).unwrap();

        assert!(!loaded.created);
        assert_eq!(loaded.config, config);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_invalid_mcp_server_id() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": { "Authorization": "Bearer secret" }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {
            "bad server": {
              "command": "npx",
              "args": [],
              "env": {},
              "tools": ["*"],
              "timeout": 60000
            }
          }
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("bad server"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_invalid_mcp_transport() {
        let text = r#"
        {
          "llm_request_settings": {
            "base_url": "https://example.test/chat",
            "retries": 4,
            "request_timeout_seconds": 240,
            "connect_timeout_seconds": 45,
            "body": { "model": "test/model" },
            "header": { "Authorization": "Bearer secret" }
          },
          "agent_settings": {
            "max_turns": 12,
            "max_tool_output_bytes": 4096,
            "max_context_tokens": 8192,
            "max_resume_traj": 100,
            "build_in_tools": ["read_file"],
            "system_prompt": ["prompt"]
          },
          "mcp_servers": {
            "docs": {
              "type": "websocket",
              "url": "https://example.test/mcp",
              "headers": {}
            }
          }
        }
        "#;

        let err = AgentConfig::from_jsonc(text).unwrap_err();

        assert!(err.to_string().contains("websocket"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn reads_minimal_stdio_mcp_server_with_defaults() {
        let text = base_config_with_mcp(
            r#"
            "pdf-mcp": {
              "command": "pdf-mcp"
            }
            "#,
        );

        let config = AgentConfig::from_jsonc(&text).unwrap();
        let server = config.mcp_servers.get("pdf-mcp").unwrap();

        assert!(server.enabled);
        assert_eq!(server.transport, McpTransport::Stdio);
        assert_eq!(server.command.as_deref(), Some("pdf-mcp"));
        assert!(server.args.is_empty());
        assert!(server.env.is_empty());
        assert_eq!(server.tools, ["*"]);
        assert_eq!(server.timeout_seconds, 60);
    }

    #[test]
    fn reads_http_mcp_server_with_http_type() {
        let text = base_config_with_mcp(
            r#"
            "tavily-remote-mcp": {
              "type": "http",
              "url": "https://mcp.tavily.com/mcp/?tavilyApiKey=...",
              "headers": {
                "Authorization": "Bearer token"
              },
              "tools": ["search"],
              "timeout": 45000,
              "disabled": true
            }
            "#,
        );

        let config = AgentConfig::from_jsonc(&text).unwrap();
        let server = config.mcp_servers.get("tavily-remote-mcp").unwrap();

        assert!(!server.enabled);
        assert_eq!(server.transport, McpTransport::StreamableHttp);
        assert_eq!(
            server.url.as_deref(),
            Some("https://mcp.tavily.com/mcp/?tavilyApiKey=...")
        );
        assert_eq!(
            server.headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
        assert_eq!(server.tools, ["search"]);
        assert_eq!(server.timeout_seconds, 45);
    }

    #[test]
    fn reads_http_mcp_server_with_streamable_http_alias() {
        let text = base_config_with_mcp(
            r#"
            "docs": {
              "type": "streamable-http",
              "url": "https://example.test/mcp"
            }
            "#,
        );

        let config = AgentConfig::from_jsonc(&text).unwrap();
        let server = config.mcp_servers.get("docs").unwrap();

        assert_eq!(server.transport, McpTransport::StreamableHttp);
        assert_eq!(server.timeout_seconds, 60);
    }

    #[test]
    fn save_omits_default_mcp_fields() {
        let path = env::temp_dir().join(format!(
            "theseus-config-save-default-mcp-{}.jsonc",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut config = AgentConfig::default_empty();
        config.mcp_servers.insert(
            "pdf-mcp".to_string(),
            McpServerConfig {
                enabled: true,
                transport: McpTransport::Stdio,
                command: Some("pdf-mcp".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["*".to_string()],
                timeout_seconds: 60,
            },
        );

        config.save_at(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert!(text.contains("\"command\": \"pdf-mcp\""));
        assert!(!text.contains("\"args\""));
        assert!(!text.contains("\"env\""));
        assert!(!text.contains("\"tools\""));
        assert!(!text.contains("\"timeout\""));
        assert!(!text.contains("\"disabled\""));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_legacy_mcp_fields() {
        for field in ["enabled", "transport", "timeout_seconds"] {
            let value = match field {
                "enabled" => "true",
                "transport" => "\"stdio\"",
                "timeout_seconds" => "60",
                _ => unreachable!(),
            };
            let text = base_config_with_mcp(&format!(
                r#"
                "docs": {{
                  "command": "docs",
                  "{field}": {value}
                }}
                "#
            ));

            let err = AgentConfig::from_jsonc(&text).unwrap_err();

            assert!(err.to_string().contains(field));
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn rejects_unsupported_mcp_fields() {
        for field in ["oauth", "headersHelper", "alwaysLoad", "channels"] {
            let text = base_config_with_mcp(&format!(
                r#"
                "docs": {{
                  "command": "docs",
                  "{field}": {{}}
                }}
                "#
            ));

            let err = AgentConfig::from_jsonc(&text).unwrap_err();

            assert!(err.to_string().contains(field));
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn rejects_ambiguous_mcp_transport() {
        let text = base_config_with_mcp(
            r#"
            "docs": {
              "command": "docs",
              "url": "https://example.test/mcp"
            }
            "#,
        );

        let err = AgentConfig::from_jsonc(&text).unwrap_err();

        assert!(err.to_string().contains("ambiguous"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_transport_specific_mcp_fields() {
        let stdio_headers = base_config_with_mcp(
            r#"
            "docs": {
              "command": "docs",
              "headers": {}
            }
            "#,
        );
        let http_args = base_config_with_mcp(
            r#"
            "docs": {
              "type": "http",
              "url": "https://example.test/mcp",
              "args": []
            }
            "#,
        );
        let http_env = base_config_with_mcp(
            r#"
            "docs": {
              "type": "http",
              "url": "https://example.test/mcp",
              "env": {}
            }
            "#,
        );

        for (text, field) in [
            (stdio_headers, "headers"),
            (http_args, "args"),
            (http_env, "env"),
        ] {
            let err = AgentConfig::from_jsonc(&text).unwrap_err();

            assert!(err.to_string().contains(field));
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn rejects_subsecond_mcp_timeout() {
        let text = base_config_with_mcp(
            r#"
            "docs": {
              "command": "docs",
              "timeout": 500
            }
            "#,
        );

        let err = AgentConfig::from_jsonc(&text).unwrap_err();

        assert!(err.to_string().contains("timeout"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    fn assert_field_order(text: &str, fields: &[&str]) {
        let mut previous = 0;
        for field in fields {
            let index = text
                .find(field)
                .unwrap_or_else(|| panic!("missing {field}"));
            assert!(
                index >= previous,
                "{field} appears before previous field in:\n{text}"
            );
            previous = index;
        }
    }

    fn base_config_with_mcp(mcp_servers: &str) -> String {
        format!(
            r#"
            {{
              "llm_request_settings": {{
                "base_url": "https://example.test/chat",
                "retries": 4,
                "request_timeout_seconds": 240,
                "connect_timeout_seconds": 45,
                "body": {{ "model": "test/model" }},
                "header": {{ "Authorization": "Bearer secret" }}
              }},
              "agent_settings": {{
                "max_turns": 12,
                "max_tool_output_bytes": 4096,
                "max_context_tokens": 8192,
                "max_resume_traj": 100,
                "build_in_tools": ["read_file"],
                "system_prompt": ["prompt"]
              }},
              "mcp_servers": {{
                {mcp_servers}
              }}
            }}
            "#
        )
    }
}

use std::{fs, io, path::Path};

use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};

use super::{AgentConfig, jsonc::jsonc_parse_options};

pub(super) enum ConfigPatch {
    SetModel(String),
    SetAuthorization(String),
}

pub(super) fn patch_config_jsonc_file(
    path: impl AsRef<Path>,
    patch: ConfigPatch,
) -> io::Result<AgentConfig> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)?;
    let patched = patch_config_jsonc_text(&text, patch)?;
    let config = AgentConfig::from_jsonc(&patched)?;
    fs::write(path, patched)?;
    Ok(config)
}

pub(super) fn patch_config_jsonc_text(text: &str, patch: ConfigPatch) -> io::Result<String> {
    let root = CstRootNode::parse(text, &jsonc_parse_options()).map_err(config_parse_error)?;
    let object = root
        .object_value()
        .ok_or_else(|| invalid_config("config root must be an object"))?;

    match patch {
        ConfigPatch::SetModel(model) => set_model(&object, model)?,
        ConfigPatch::SetAuthorization(value) => set_authorization(&object, value)?,
    }

    Ok(root.to_string())
}

fn set_model(root: &CstObject, model: String) -> io::Result<()> {
    let settings = object_field(root, "llm_request_settings", "llm_request_settings")?;
    let body = object_field(&settings, "body", "llm_request_settings.body")?;
    set_or_insert_before(
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
    Ok(())
}

fn set_authorization(root: &CstObject, value: String) -> io::Result<()> {
    let settings = object_field(root, "llm_request_settings", "llm_request_settings")?;
    let header = object_field(&settings, "header", "llm_request_settings.header")?;
    set_or_insert_before(&header, "Authorization", value, &["Content-Type"]);
    Ok(())
}

fn object_field(object: &CstObject, name: &str, path: &str) -> io::Result<CstObject> {
    object
        .object_value(name)
        .ok_or_else(|| invalid_config(format!("config field `{path}` must be an object")))
}

fn set_or_insert_before(object: &CstObject, name: &str, value: String, before: &[&str]) {
    let value = CstInputValue::String(value);
    match object.get(name) {
        Some(prop) => prop.set_value(value),
        None => {
            if let Some(index) = before
                .iter()
                .find_map(|candidate| object.get(candidate).map(|prop| prop.property_index()))
            {
                object.insert(index, name, value);
            } else {
                object.append(name, value);
            }
        }
    }
}

fn config_parse_error(err: jsonc_parser::errors::ParseError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("config must be valid JSONC: {err}"),
    )
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::{ConfigPatch, patch_config_jsonc_file, patch_config_jsonc_text};

    fn base_config(body: &str, header: &str) -> String {
        format!(
            r#"{{
  // request settings
  "llm_request_settings": {{
    "base_url": "https://example.test/chat",
    "retries": 4,
    "request_timeout_seconds": 240,
    "connect_timeout_seconds": 45,
    "body": {body},
    "header": {header}
  }},
  "agent_settings": {{
    "max_turns": 12,
    "max_tool_output_bytes": 4096,
    "max_context_tokens": 8192,
    "max_resume_traj": 100,
    "build_in_tools": ["read_file"],
    "system_prompt": ["prompt"]
  }},
  "mcp_servers": {{}}
}}"#
        )
    }

    #[test]
    fn patch_model_preserves_comments() {
        let text = base_config(
            r#"{
      // selected model
      "model": "old/model",
      "tool_choice": "auto"
    }"#,
            r#"{ "Authorization": "Bearer secret" }"#,
        );

        let patched =
            patch_config_jsonc_text(&text, ConfigPatch::SetModel("new/model".to_string())).unwrap();

        assert!(patched.contains("// request settings"));
        assert!(patched.contains("// selected model"));
        assert!(patched.contains(r#""model": "new/model""#));
        assert!(!patched.contains("old/model"));
    }

    #[test]
    fn patch_api_key_preserves_comments() {
        let text = base_config(
            r#"{ "model": "test/model" }"#,
            r#"{
      // auth header
      "Authorization": "Bearer old",
      "Content-Type": "application/json"
    }"#,
        );

        let patched = patch_config_jsonc_text(
            &text,
            ConfigPatch::SetAuthorization("Bearer new".to_string()),
        )
        .unwrap();

        assert!(patched.contains("// auth header"));
        assert!(patched.contains(r#""Authorization": "Bearer new""#));
        assert!(!patched.contains("Bearer old"));
    }

    #[test]
    fn patch_inserts_model_deterministically_when_missing() {
        let text = base_config(
            r#"{
      "tool_choice": "auto"
    }"#,
            r#"{ "Authorization": "Bearer secret" }"#,
        );

        let patched =
            patch_config_jsonc_text(&text, ConfigPatch::SetModel("new/model".to_string())).unwrap();

        assert!(patched.contains(r#""model": "new/model""#));
        assert!(patched.find(r#""model""#).unwrap() < patched.find(r#""tool_choice""#).unwrap());
    }

    #[test]
    fn patch_inserts_authorization_when_missing() {
        let text = base_config(
            r#"{ "model": "test/model" }"#,
            r#"{ "Content-Type": "application/json" }"#,
        );

        let patched = patch_config_jsonc_text(
            &text,
            ConfigPatch::SetAuthorization("Bearer new".to_string()),
        )
        .unwrap();

        assert!(patched.contains(r#""Authorization": "Bearer new""#));
        assert!(
            patched.find(r#""Authorization""#).unwrap()
                < patched.find(r#""Content-Type""#).unwrap()
        );
    }

    #[test]
    fn patch_preserves_trailing_commas() {
        let text = base_config(
            r#"{
      "model": "old/model",
      "tool_choice": "auto",
    }"#,
            r#"{ "Authorization": "Bearer secret", }"#,
        );

        let patched =
            patch_config_jsonc_text(&text, ConfigPatch::SetModel("new/model".to_string())).unwrap();

        assert!(patched.contains(r#""tool_choice": "auto","#));
        assert!(patched.contains(r#""Authorization": "Bearer secret","#));
    }

    #[test]
    fn patch_rejects_missing_target_object() {
        let text = base_config(r#""not-object""#, r#"{ "Authorization": "Bearer secret" }"#);

        let err = patch_config_jsonc_text(&text, ConfigPatch::SetModel("new/model".to_string()))
            .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("llm_request_settings.body"));
    }

    #[test]
    fn patch_file_validates_before_write() {
        let path = env::temp_dir().join(format!(
            "theseus-config-patch-validate-{}.jsonc",
            std::process::id()
        ));
        let text = base_config(
            r#"{ "model": "old/model" }"#,
            r#"{ "Authorization": "Bearer secret" }"#,
        )
        .replace(r#""max_turns": 12"#, r#""max_turns": "invalid""#);
        fs::write(&path, &text).unwrap();

        let err = patch_config_jsonc_file(&path, ConfigPatch::SetModel("new/model".to_string()))
            .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        let _ = fs::remove_file(path);
    }
}

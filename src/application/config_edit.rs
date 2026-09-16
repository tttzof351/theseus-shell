use crate::agent::AgentConfig;
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(super) enum ConfigPatch {
    SetModel(String),
    SetAuthorization(String),
}

pub(super) fn patch_config_jsonc_file(path: &Path, patch: ConfigPatch) -> io::Result<AgentConfig> {
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

pub(super) fn patch_config_jsonc_text(text: &str, patch: ConfigPatch) -> io::Result<String> {
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
            ".{name}.application-{}-{suffix}.tmp",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::temporary_test_path;
    use serde_json::Value;

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
            ".{}.application-",
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
    fn config_model_and_key_edits_preserve_streaming_settings_and_comments() {
        let path = temporary_test_path("streaming-config-patch");
        AgentConfig::load_or_create_at(path.clone()).unwrap();
        let original = fs::read_to_string(&path).unwrap()
            .replacen("\"stream_idle_timeout_seconds\": 60", "// keep network timeout\n    \"stream_idle_timeout_seconds\": 17", 1)
            .replacen("\"body\": {", "\"body\": {\n      // explicit streaming options\n      \"stream_options\": { \"include_usage\": true },", 1);
        fs::write(&path, &original).unwrap();
        for patch in [
            ConfigPatch::SetModel("example/stream-model".into()),
            ConfigPatch::SetAuthorization("Bearer new-fixture-key".into()),
        ] {
            let config = patch_config_jsonc_file(&path, patch).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains("// keep network timeout"));
            assert!(text.contains("// explicit streaming options"));
            assert!(text.contains("\"stream_options\": { \"include_usage\": true }"));
            assert_eq!(
                config.llm_request_settings.stream_idle_timeout_seconds,
                Some(17)
            );
            assert_eq!(config.llm_request_settings.body["stream"], true);
            assert_eq!(
                config.llm_request_settings.body["stream_options"]["include_usage"],
                true
            );
            assert_eq!(
                config.llm_request_settings.body["model"],
                "example/stream-model"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

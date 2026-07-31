use super::*;

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

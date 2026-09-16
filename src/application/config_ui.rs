//! Configuration and model-selection interactions.
use super::{
    Application, Interaction,
    config_edit::{ConfigPatch, patch_config_jsonc_file},
    editor::UnifiedEditor,
    picker::{PickerItem, PickerOutcome, PickerState},
};
use crate::agent::config::model_catalog;
use crate::logging::AppLogger;
use serde_json::{Value, json};
use std::io;

impl Application {
    pub(super) fn open_config(&mut self) {
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

    pub(super) fn finish_config_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) if id == "model" => {
                let (reply, models) = std::sync::mpsc::channel();
                self.start_operation(
                    crate::agent::worker::Operation::ModelCatalog { reply },
                    "/config".into(),
                )?;
                self.pending_models = Some(models);
                self.return_to_command_editor();
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

    pub(super) fn show_model_catalog(&mut self, catalog: model_catalog::ModelCatalog) {
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
                    label: format!("{}{}", model.id, if is_current { " (current)" } else { "" }),
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
    }

    pub(super) fn finish_model_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
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
                self.return_to_command_editor();
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

    pub(super) fn save_config_patch(
        &mut self,
        patch: ConfigPatch,
        model_changed: bool,
    ) -> io::Result<()> {
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
        self.apply_configuration(
            format!("Config saved to {}\n", self.config_path.display()),
            "/config",
        )?;
        Ok(())
    }

    pub(super) fn apply_configuration(
        &mut self,
        confirmation: String,
        command: &str,
    ) -> io::Result<()> {
        self.start_operation(
            crate::agent::worker::Operation::Configure {
                config: Box::new(self.config.clone()),
                logger: self.logger.clone(),
            },
            command.into(),
        )?;
        self.pending_config_confirmation = Some(confirmation);
        Ok(())
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

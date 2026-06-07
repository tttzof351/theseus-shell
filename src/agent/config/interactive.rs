use std::{
    io,
    io::{IsTerminal, Write},
    path::Path,
};

use crossterm::{
    cursor::{Hide, MoveDown, MoveToColumn, MoveUp, Show},
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, Clear, ClearType},
};
use serde_json::json;

use crate::{
    common::text::truncate_chars_end,
    input::{
        RawModeGuard, ViewportState, colorize_tag, is_control_key, is_key_press, is_plain_text_key,
        read_masked_line,
    },
};

use super::{
    AgentConfig,
    document::{ConfigPatch, patch_config_jsonc_file},
    model_catalog::{self, ModelCatalog, ModelCatalogSource, ModelOption},
};

#[derive(Clone, Copy)]
enum ConfigOption {
    ChangeModel,
    SetApiKey,
}

impl ConfigOption {
    const ALL: &'static [Self] = &[Self::ChangeModel, Self::SetApiKey];

    fn label(self) -> &'static str {
        match self {
            Self::ChangeModel => "Change model",
            Self::SetApiKey => "Set OpenRouter API key",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ChangeModel => "Select a different model",
            Self::SetApiKey => "Update the API key",
        }
    }
}

impl AgentConfig {
    pub fn configure_interactive_at(
        current: Option<&AgentConfig>,
        path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        let path = path.as_ref();
        let mut config = current.cloned().unwrap_or_else(Self::default_empty);
        warn_if_non_openrouter_base_url(&config.llm_request_settings.base_url);
        let option = select_config_option()?;

        match option {
            ConfigOption::ChangeModel => {
                let model = select_model(current_model(&config))?;
                if path.exists() {
                    return patch_config_jsonc_file(path, ConfigPatch::SetModel(model));
                }
                config
                    .llm_request_settings
                    .body
                    .insert("model".to_string(), json!(model));
            }
            ConfigOption::SetApiKey => {
                let value = authorization_header_value(&prompt_api_key()?);
                if path.exists() {
                    return patch_config_jsonc_file(path, ConfigPatch::SetAuthorization(value));
                }
                config
                    .llm_request_settings
                    .header
                    .insert("Authorization".to_string(), value);
            }
        }

        config.save_at(path)?;

        Ok(config)
    }
}

fn warn_if_non_openrouter_base_url(base_url: &str) {
    if !base_url.contains("openrouter.ai") {
        eprintln!(
            "Warning: /config updates OpenRouter-like fields, but base_url is not an OpenRouter endpoint."
        );
    }
}

fn current_model(config: &AgentConfig) -> Option<&str> {
    config
        .llm_request_settings
        .body
        .get("model")
        .and_then(serde_json::Value::as_str)
}

fn authorization_header_value(input: &str) -> String {
    let input = input.trim();
    if input.starts_with("Bearer ") {
        input.to_string()
    } else {
        format!("Bearer {input}")
    }
}

fn select_config_option() -> io::Result<ConfigOption> {
    if !io::stdin().is_terminal() {
        return select_config_option_plain();
    }

    select_config_option_with_arrows()
}

fn select_config_option_plain() -> io::Result<ConfigOption> {
    println!("What would you like to configure?");
    for (index, option) in ConfigOption::ALL.iter().enumerate() {
        println!("{}", format_config_option_row(index, *option, index == 0));
    }
    print!("Option [1]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice = input
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|choice| (1..=ConfigOption::ALL.len()).contains(choice))
        .map(|choice| choice - 1)
        .unwrap_or(0);

    Ok(ConfigOption::ALL[choice])
}

fn select_config_option_with_arrows() -> io::Result<ConfigOption> {
    let mut selected: usize = 0;
    let _raw_mode = RawModeGuard::enable()?;
    let _cursor = HiddenCursorGuard::hide()?;
    render_config_option_select(selected)?;

    loop {
        if let Event::Key(key) = event::read()? {
            if !is_key_press(key) {
                continue;
            }
            match key.code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    render_config_option_select(selected)?;
                }
                KeyCode::Down => {
                    if selected + 1 < ConfigOption::ALL.len() {
                        selected += 1;
                    }
                    render_config_option_select(selected)?;
                }
                KeyCode::Enter => {
                    finish_config_option_select()?;
                    return Ok(ConfigOption::ALL[selected]);
                }
                KeyCode::Esc => {
                    finish_config_option_select()?;
                    return Err(config_cancelled_error());
                }
                KeyCode::Char('c') if is_control_key(key) => {
                    finish_config_option_select()?;
                    return Err(config_cancelled_error());
                }
                _ => {}
            }
        }
    }
}

fn render_config_option_select(selected: usize) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    writeln!(stdout, "What would you like to configure?")?;

    for (index, option) in ConfigOption::ALL.iter().enumerate() {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        writeln!(
            stdout,
            "{}",
            format_config_option_row(index, *option, index == selected)
        )?;
    }

    execute!(stdout, MoveUp(ConfigOption::ALL.len() as u16 + 1))?;
    stdout.flush()
}

fn format_config_option_row(index: usize, option: ConfigOption, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let label = format!("{}. {}", index + 1, option.label());
    let width = config_option_label_width();
    let line = format!("{marker} {label:<width$}  {}", option.description());

    if selected {
        return colorize_tag("cyan", &colorize_tag("bold", &line));
    }

    line
}

fn config_option_label_width() -> usize {
    ConfigOption::ALL
        .iter()
        .enumerate()
        .map(|(index, option)| format!("{}. {}", index + 1, option.label()).chars().count())
        .max()
        .unwrap_or(0)
}

fn finish_config_option_select() -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        MoveDown(ConfigOption::ALL.len() as u16 + 1),
        MoveToColumn(0),
        Clear(ClearType::CurrentLine)
    )?;
    stdout.flush()
}

fn select_model(current_model: Option<&str>) -> io::Result<String> {
    let catalog = model_catalog::load_openrouter_models();

    if !io::stdin().is_terminal() {
        return select_model_plain(current_model, &catalog);
    }

    select_model_with_search(current_model, catalog)
}

fn selected_model_index(models: &[ModelOption], current_model: Option<&str>) -> usize {
    current_model
        .and_then(|model| models.iter().position(|candidate| candidate.id == model))
        .unwrap_or(0)
}

fn select_model_plain(current_model: Option<&str>, catalog: &ModelCatalog) -> io::Result<String> {
    let fallback = current_model
        .or_else(|| catalog.models.first().map(|model| model.id.as_str()))
        .unwrap_or_default();
    println!();
    println!(
        "{}",
        colorize_tag(
            "bold",
            "Model id or search query. Leave empty to keep current/default model."
        )
    );
    print!("Model [{fallback}]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return Ok(fallback.to_string());
    }

    if let Some(model) = catalog.models.iter().find(|model| model.id == input) {
        return Ok(model.id.clone());
    }

    Ok(filter_models(&catalog.models, input)
        .first()
        .map(|index| catalog.models[*index].id.clone())
        .unwrap_or_else(|| input.to_string()))
}

fn select_model_with_search(
    current_model: Option<&str>,
    catalog: ModelCatalog,
) -> io::Result<String> {
    let mut state = ModelSelectState::new(catalog, current_model);
    let _raw_mode = RawModeGuard::enable()?;
    let _cursor = HiddenCursorGuard::hide()?;
    writeln!(io::stdout())?;
    render_model_select(&mut state)?;

    loop {
        if let Event::Key(key) = event::read()? {
            if !is_key_press(key) {
                continue;
            }
            match key.code {
                KeyCode::Up => {
                    state.move_selected(-1);
                    render_model_select(&mut state)?;
                }
                KeyCode::Down => {
                    state.move_selected(1);
                    render_model_select(&mut state)?;
                }
                KeyCode::PageUp => {
                    state.move_selected(-(state.viewport_rows() as isize));
                    render_model_select(&mut state)?;
                }
                KeyCode::PageDown => {
                    state.move_selected(state.viewport_rows() as isize);
                    render_model_select(&mut state)?;
                }
                KeyCode::Home => {
                    state.set_selected(0);
                    render_model_select(&mut state)?;
                }
                KeyCode::End => {
                    state.set_selected(state.filtered.len().saturating_sub(1));
                    render_model_select(&mut state)?;
                }
                KeyCode::Enter => {
                    if let Some(model) = state.selected_model() {
                        let model = model.id.clone();
                        finish_model_select(state.rendered_lines)?;
                        return Ok(model);
                    }
                }
                KeyCode::Backspace => {
                    state.query.pop();
                    state.refresh_filter();
                    render_model_select(&mut state)?;
                }
                KeyCode::Char(c) if is_plain_text_key(key) => {
                    state.query.push(c);
                    state.refresh_filter();
                    render_model_select(&mut state)?;
                }
                KeyCode::Esc => {
                    finish_model_select(state.rendered_lines)?;
                    return Err(config_cancelled_error());
                }
                KeyCode::Char('c') if is_control_key(key) => {
                    finish_model_select(state.rendered_lines)?;
                    return Err(config_cancelled_error());
                }
                _ => {}
            }
        }
    }
}

struct ModelSelectState {
    catalog: ModelCatalog,
    current_model: Option<String>,
    query: String,
    filtered: Vec<usize>,
    viewport: ViewportState,
    rendered_lines: u16,
}

impl ModelSelectState {
    fn new(catalog: ModelCatalog, current_model: Option<&str>) -> Self {
        let filtered = filter_models(&catalog.models, "");
        let selected = selected_model_index(&catalog.models, current_model);
        let selected = filtered
            .iter()
            .position(|index| *index == selected)
            .unwrap_or(0);
        let viewport =
            ViewportState::new(filtered.len(), model_viewport_rows()).with_selected(selected);
        Self {
            catalog,
            current_model: current_model.map(str::to_string),
            query: String::new(),
            filtered,
            viewport,
            rendered_lines: 0,
        }
    }

    fn refresh_filter(&mut self) {
        self.filtered = filter_models(&self.catalog.models, &self.query);
        self.viewport = ViewportState::new(self.filtered.len(), self.viewport.rows());
    }

    fn move_selected(&mut self, delta: isize) {
        self.viewport.move_selected(delta);
    }

    fn set_selected(&mut self, selected: usize) {
        self.viewport.set_selected(selected);
    }

    fn viewport_rows(&self) -> usize {
        self.viewport.rows()
    }

    fn selected_model(&self) -> Option<&ModelOption> {
        self.filtered
            .get(self.viewport.selected())
            .and_then(|index| self.catalog.models.get(*index))
    }
}

fn render_model_select(state: &mut ModelSelectState) -> io::Result<()> {
    let mut stdout = io::stdout();

    let lines = model_select_lines(state);
    state.rendered_lines = lines.len() as u16;

    for line in lines {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        writeln!(stdout, "{line}")?;
    }

    execute!(stdout, MoveUp(state.rendered_lines))?;
    stdout.flush()
}

fn model_select_lines(state: &ModelSelectState) -> Vec<String> {
    let mut lines = Vec::new();
    let columns = terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(80)
        .max(20)
        .saturating_sub(1);
    lines.push(format!(
        "{} {}",
        colorize_tag("bold", "Select model:"),
        model_catalog_source_label(&state.catalog.source)
    ));
    lines.push(truncate_chars_end(
        &format!("Search: {}", state.query),
        columns,
    ));

    let total = state.filtered.len();
    let from = total.min(state.viewport.offset() + 1);
    let to = total.min(state.viewport.offset() + state.viewport.rows());
    lines.push(format!("Matches: {total}  Showing: {from}-{to}"));

    if state.filtered.is_empty() {
        lines.push(colorize_tag(
            "orange",
            &truncate_chars_end("  No matching models", columns),
        ));
        for _ in 1..state.viewport.rows() {
            lines.push(String::new());
        }
    } else {
        for row in 0..state.viewport.rows() {
            let filtered_index = state.viewport.offset() + row;
            let line = state
                .filtered
                .get(filtered_index)
                .and_then(|model_index| state.catalog.models.get(*model_index))
                .map(|model| {
                    format_model_option_row(
                        model,
                        &state.current_model,
                        filtered_index == state.viewport.selected(),
                        columns,
                    )
                })
                .unwrap_or_default();
            lines.push(line);
        }
    }

    lines
}

fn model_catalog_source_label(source: &ModelCatalogSource) -> &'static str {
    match source {
        ModelCatalogSource::Fresh => "(OpenRouter)",
        ModelCatalogSource::Cache => "(cache)",
        ModelCatalogSource::StaleCache => "(stale cache)",
        ModelCatalogSource::Fallback => "(fallback)",
    }
}

fn filter_models(models: &[ModelOption], query: &str) -> Vec<usize> {
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();

    models
        .iter()
        .enumerate()
        .filter(|(_, model)| {
            if terms.is_empty() {
                return true;
            }

            let haystack = format!(
                "{} {}",
                model.id.to_lowercase(),
                model.name.as_deref().unwrap_or_default().to_lowercase()
            );
            terms.iter().all(|term| haystack.contains(term))
        })
        .map(|(index, _)| index)
        .collect()
}

fn model_viewport_rows() -> usize {
    let rows = terminal::size().map(|(_, rows)| rows).unwrap_or(24);
    usize::from(rows.saturating_sub(7)).clamp(5, 12)
}

fn format_model_option_row(
    model: &ModelOption,
    current_model: &Option<String>,
    selected: bool,
    max_width: usize,
) -> String {
    let marker = if selected { ">" } else { " " };
    let current = if current_model.as_deref() == Some(model.id.as_str()) {
        " (current)"
    } else {
        ""
    };
    let context = model
        .context_length
        .map(format_context_length)
        .unwrap_or_else(|| "n/a".to_string());
    let name = model.name.as_deref().unwrap_or_default();
    let line = if name.is_empty() {
        format!("{marker} {}{current}  ctx: {context}", model.id)
    } else {
        format!("{marker} {}{current}  ctx: {context}  {name}", model.id)
    };
    let line = truncate_chars_end(&line, max_width);

    if selected {
        return colorize_tag("cyan", &colorize_tag("bold", &line));
    }

    line
}

fn format_context_length(context_length: u64) -> String {
    if context_length >= 1_000_000 {
        return format!("{}m", context_length / 1_000_000);
    }

    if context_length >= 1_000 {
        return format!("{}k", context_length / 1_000);
    }

    context_length.to_string()
}

fn finish_model_select(rendered_lines: u16) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        MoveDown(rendered_lines),
        MoveToColumn(0),
        Clear(ClearType::CurrentLine)
    )?;
    stdout.flush()
}

fn prompt_api_key() -> io::Result<String> {
    read_masked_line("Openrouter API key: ")?
        .map(|input| input.trim().to_string())
        .ok_or_else(config_cancelled_error)
}

fn config_cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "config cancelled")
}

struct HiddenCursorGuard;

impl HiddenCursorGuard {
    fn hide() -> io::Result<Self> {
        execute!(io::stdout(), Hide)?;
        Ok(Self)
    }
}

impl Drop for HiddenCursorGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> ModelCatalog {
        ModelCatalog {
            source: ModelCatalogSource::Fallback,
            models: vec![
                ModelOption {
                    id: "openrouter/owl-alpha".to_string(),
                    name: Some("OpenRouter: Owl Alpha".to_string()),
                    context_length: Some(1_000_000),
                },
                ModelOption {
                    id: "mistralai/codestral-2508".to_string(),
                    name: Some("Mistral: Codestral 2508".to_string()),
                    context_length: Some(262_144),
                },
                ModelOption {
                    id: "minimax/minimax-m3".to_string(),
                    name: Some("MiniMax: MiniMax M3".to_string()),
                    context_length: Some(1_000_000),
                },
            ],
        }
    }

    #[test]
    fn model_select_lines_keep_stable_height_when_filter_changes() {
        let mut state = ModelSelectState::new(test_catalog(), Some("openrouter/owl-alpha"));
        state.viewport = ViewportState::new(state.filtered.len(), 5);

        let initial_lines = model_select_lines(&state);
        state.query = "missing".to_string();
        state.refresh_filter();
        state.viewport = ViewportState::new(state.filtered.len(), 5);
        let empty_lines = model_select_lines(&state);

        assert_eq!(initial_lines.len(), 3 + state.viewport.rows());
        assert_eq!(empty_lines.len(), 3 + state.viewport.rows());
    }

    #[test]
    fn filter_models_matches_id_and_name_terms() {
        let catalog = test_catalog();

        let matches = filter_models(&catalog.models, "mistral codestral");

        assert_eq!(matches, vec![1]);
    }
}

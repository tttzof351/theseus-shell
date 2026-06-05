use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::{LinesWithEndings, as_24_bit_terminal_escaped},
};

#[derive(Debug, Clone)]
pub struct FormatterOpts {
    pub lang: String,
    pub style: String,
}

impl Default for FormatterOpts {
    fn default() -> Self {
        Self {
            lang: "txt".to_string(),
            style: "base16-ocean.dark".to_string(),
        }
    }
}

pub fn format_source_code(code: &str, options: FormatterOpts) -> String {
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let theme_set = ThemeSet::load_defaults();

    let syntax = syntax_set
        .find_syntax_by_token(&options.lang)
        .or_else(|| syntax_set.find_syntax_by_extension(&options.lang))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

    let theme = find_theme(&theme_set, &options.style);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut result = String::new();

    for line in LinesWithEndings::from(code) {
        match highlighter.highlight_line(line, &syntax_set) {
            Ok(ranges) => result.push_str(&as_24_bit_terminal_escaped(&ranges[..], false)),
            Err(_) => result.push_str(line),
        }
    }

    result.push_str("\x1b[0m");

    result
}

pub fn available_styles() -> Vec<&'static str> {
    vec![
        "base16-ocean.dark",
        "base16-eighties.dark",
        "base16-mocha.dark",
        "base16-ocean.light",
        "InspiredGitHub",
        "Solarized (dark)",
        "Solarized (light)",
    ]
}

pub fn available_languages() -> Vec<&'static str> {
    vec![
        "bash",
        "c",
        "cpp",
        "css",
        "go",
        "html",
        "java",
        "javascript",
        "json",
        "markdown",
        "python",
        "ruby",
        "rust",
        "shell",
        "sql",
        "toml",
        "typescript",
        "xml",
        "yaml",
    ]
}

fn find_theme<'a>(theme_set: &'a ThemeSet, requested: &str) -> &'a Theme {
    theme_set
        .themes
        .get(requested)
        .or_else(|| theme_set.themes.get("base16-ocean.dark"))
        .or_else(|| theme_set.themes.values().next())
        .expect("syntect default theme set should not be empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_source_without_losing_text() {
        let code = "fn main() {}\n";
        let highlighted = format_source_code(
            code,
            FormatterOpts {
                lang: "rust".to_string(),
                style: "base16-ocean.dark".to_string(),
            },
        );

        assert!(highlighted.contains("main"));
    }
}

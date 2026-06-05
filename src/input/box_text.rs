use super::colorize::text_length;

#[derive(Debug, Clone)]
pub struct BoxOptions {
    pub max_width: usize,
    pub border_color: Option<String>,
    pub has_tags: bool,
}

impl Default for BoxOptions {
    fn default() -> Self {
        Self {
            max_width: 80,
            border_color: None,
            has_tags: false,
        }
    }
}

pub fn wrap_in_box(text: &str, mut options: BoxOptions) -> String {
    if options.max_width < 10 {
        options.max_width = 10;
    }

    let wrapped_lines = wrap_lines(text, options.max_width, options.has_tags);
    let actual_width = wrapped_lines
        .iter()
        .map(|line| text_length(line, options.has_tags))
        .max()
        .unwrap_or(0);

    let border = Border::new(options.border_color.as_deref(), options.has_tags);
    let mut result = String::new();

    result.push_str(&border.top_left);
    result.push_str(&border.horizontal.repeat(actual_width + 2));
    result.push_str(&border.top_right);
    result.push('\n');

    for line in wrapped_lines {
        let padding = actual_width.saturating_sub(text_length(&line, options.has_tags));
        result.push_str(&border.vertical);
        result.push(' ');
        result.push_str(&line);
        result.push_str(&" ".repeat(padding));
        result.push(' ');
        result.push_str(&border.vertical);
        result.push('\n');
    }

    result.push_str(&border.bottom_left);
    result.push_str(&border.horizontal.repeat(actual_width + 2));
    result.push_str(&border.bottom_right);

    result
}

fn wrap_lines(text: &str, max_width: usize, has_tags: bool) -> Vec<String> {
    text.lines()
        .flat_map(|line| wrap_line(line, max_width, has_tags))
        .collect()
}

fn wrap_line(line: &str, max_width: usize, has_tags: bool) -> Vec<String> {
    if text_length(line, has_tags) <= max_width {
        return vec![line.to_string()];
    }

    let mut result = Vec::new();
    let mut current = String::new();

    for word in line.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };

        if text_length(&candidate, has_tags) <= max_width || current.is_empty() {
            current = candidate;
        } else {
            result.push(current);
            current = word.to_string();
        }
    }

    if !current.is_empty() {
        result.push(current);
    }

    result
}

struct Border {
    top_left: String,
    top_right: String,
    bottom_left: String,
    bottom_right: String,
    horizontal: String,
    vertical: String,
}

impl Border {
    fn new(color: Option<&str>, has_tags: bool) -> Self {
        Self {
            top_left: border_part("╭", color, has_tags),
            top_right: border_part("╮", color, has_tags),
            bottom_left: border_part("╰", color, has_tags),
            bottom_right: border_part("╯", color, has_tags),
            horizontal: border_part("─", color, has_tags),
            vertical: border_part("│", color, has_tags),
        }
    }
}

fn border_part(part: &str, color: Option<&str>, has_tags: bool) -> String {
    match (color, has_tags) {
        (Some(color), true) => format!("<{color}>{part}</{color}>"),
        _ => part.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_text_in_rounded_box() {
        let boxed = wrap_in_box(
            "hello",
            BoxOptions {
                max_width: 20,
                border_color: None,
                has_tags: false,
            },
        );

        assert_eq!(boxed, "╭───────╮\n│ hello │\n╰───────╯");
    }
}

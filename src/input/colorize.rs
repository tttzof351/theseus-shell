use unicode_width::UnicodeWidthStr;

pub fn colorize_nested(text: &str) -> String {
    let mut stack: Vec<String> = Vec::new();
    let mut codes: Vec<&'static str> = Vec::new();
    let mut result = String::new();
    let mut index = 0;

    while index < text.len() {
        let rest = &text[index..];

        if let Some(tag) = read_closing_tag(rest) {
            if stack.last().is_some_and(|name| name == &tag.name) {
                stack.pop();
                codes.pop();

                result.push_str("\x1b[0m");
                for code in &codes {
                    result.push_str(code);
                }
            }
            index += tag.bytes_read;
            continue;
        }

        if let Some(tag) = read_opening_tag(rest) {
            let name = tag.name.to_lowercase();
            if let Some(code) = ansi_code(&name) {
                stack.push(name);
                codes.push(code);
                result.push_str(code);
            }
            index += tag.bytes_read;
            continue;
        }

        let ch = rest.chars().next().expect("non-empty string slice");
        result.push(ch);
        index += ch.len_utf8();
    }

    if !codes.is_empty() {
        result.push_str("\x1b[0m");
    }

    result
}

pub fn colorize_tag(tag: &str, text: &str) -> String {
    let Some(code) = ansi_code(&tag.to_lowercase()) else {
        return text.to_string();
    };

    format!("{code}{text}\x1b[0m")
}

pub fn colorize_tags(tags: &[String], text: &str) -> String {
    let codes = tags
        .iter()
        .filter_map(|tag| ansi_code(tag))
        .collect::<Vec<_>>();
    if codes.is_empty() {
        return text.to_string();
    }

    format!("{}{}\x1b[0m", codes.join(""), text)
}

pub fn is_known_color_tag(tag: &str) -> bool {
    ansi_code(&tag.to_lowercase()).is_some()
}

pub fn strip_tags(text: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;

    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    result
}

pub fn strip_ansi_codes(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }

        result.push(ch);
    }

    result
}

pub fn text_length(text: &str, has_tags: bool) -> usize {
    if has_tags {
        let visible_text = strip_tags(text);
        UnicodeWidthStr::width(strip_ansi_codes(&visible_text).as_str())
    } else {
        UnicodeWidthStr::width(strip_ansi_codes(text).as_str())
    }
}

struct Tag {
    name: String,
    bytes_read: usize,
}

fn read_tag(text: &str) -> Option<Tag> {
    let end = text.find('>')?;
    Some(Tag {
        name: text[..end].to_string(),
        bytes_read: end + 1,
    })
}

fn read_opening_tag(text: &str) -> Option<Tag> {
    let tag = read_tag(text.strip_prefix('<')?)?;
    Some(Tag {
        name: tag.name,
        bytes_read: tag.bytes_read + 1,
    })
}

fn read_closing_tag(text: &str) -> Option<Tag> {
    let tag = read_tag(text.strip_prefix("</")?)?;
    Some(Tag {
        name: tag.name,
        bytes_read: tag.bytes_read + 2,
    })
}

fn ansi_code(name: &str) -> Option<&'static str> {
    match name {
        "black" => Some("\x1b[30m"),
        "red" => Some("\x1b[31m"),
        "green" => Some("\x1b[32m"),
        "yellow" => Some("\x1b[33m"),
        "orange" => Some("\x1b[38;5;208m"),
        "blue" => Some("\x1b[34m"),
        "magenta" => Some("\x1b[35m"),
        "cyan" => Some("\x1b[36m"),
        "white" => Some("\x1b[37m"),
        "bright-black" => Some("\x1b[90m"),
        "bright-red" => Some("\x1b[91m"),
        "bright-green" => Some("\x1b[92m"),
        "bright-yellow" => Some("\x1b[93m"),
        "bright-blue" => Some("\x1b[94m"),
        "bright-magenta" => Some("\x1b[95m"),
        "bright-cyan" => Some("\x1b[96m"),
        "bright-white" => Some("\x1b[97m"),
        "bg-black" => Some("\x1b[40m"),
        "bg-red" => Some("\x1b[41m"),
        "bg-green" => Some("\x1b[42m"),
        "bg-yellow" => Some("\x1b[43m"),
        "bg-blue" => Some("\x1b[44m"),
        "bg-magenta" => Some("\x1b[45m"),
        "bg-cyan" => Some("\x1b[46m"),
        "bg-white" => Some("\x1b[47m"),
        "bold" => Some("\x1b[1m"),
        "dim" => Some("\x1b[2m"),
        "italic" => Some("\x1b[3m"),
        "underline" => Some("\x1b[4m"),
        "blink" => Some("\x1b[5m"),
        "reverse" => Some("\x1b[7m"),
        "strikethrough" => Some("\x1b[9m"),
        "reset" => Some("\x1b[0m"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_for_length() {
        assert_eq!(text_length("<bold>hello</bold>", true), 5);
    }

    #[test]
    fn text_length_uses_terminal_column_width() {
        assert_eq!(text_length("a界b", false), 4);
        assert_eq!(text_length("a🙂b", false), 4);
        assert_eq!(text_length("\x1b[32m界\x1b[0m", false), 2);
    }

    #[test]
    fn handles_nested_tags() {
        assert_eq!(
            colorize_nested("<bold>a<green>b</green>c</bold>"),
            "\x1b[1ma\x1b[32mb\x1b[0m\x1b[1mc\x1b[0m"
        );
    }

    #[test]
    fn handles_orange_tag() {
        assert_eq!(
            colorize_nested("<orange>hello</orange>"),
            "\x1b[38;5;208mhello\x1b[0m"
        );
    }

    #[test]
    fn colorizes_plain_text_without_parsing_tags() {
        assert_eq!(
            colorize_tag("bold", "Vec<String>"),
            "\x1b[1mVec<String>\x1b[0m"
        );
    }
}

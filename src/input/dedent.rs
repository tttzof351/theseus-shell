pub fn dedent(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    let lines: Vec<&str> = text.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indentation_width(line))
        .min();

    let Some(min_indent) = min_indent else {
        return String::new();
    };

    let dedented = lines
        .iter()
        .map(|line| remove_indent(line, min_indent))
        .collect::<Vec<_>>()
        .join("\n");

    dedented.trim().to_string()
}

pub fn dedent_keep_indent(text: impl AsRef<str>, base_indent: usize) -> String {
    let dedented = dedent(text);
    if base_indent == 0 {
        return dedented;
    }

    let indent = " ".repeat(base_indent);
    dedented
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn indentation_width(line: &str) -> usize {
    let mut width = 0;

    for ch in line.chars() {
        match ch {
            '\t' => width += 4,
            ch if ch.is_whitespace() => width += 1,
            _ => break,
        }
    }

    width
}

fn remove_indent(line: &str, indent: usize) -> String {
    if line.trim().is_empty() {
        return String::new();
    }

    let mut removed = 0;
    let mut byte_index = 0;

    for (index, ch) in line.char_indices() {
        if removed >= indent {
            byte_index = index;
            break;
        }

        match ch {
            '\t' => removed += 4,
            ch if ch.is_whitespace() => removed += 1,
            _ => {
                byte_index = index;
                break;
            }
        }

        byte_index = index + ch.len_utf8();
    }

    line[byte_index..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_common_indent() {
        let text = "
            one
                two
            three
        ";

        assert_eq!(dedent(text), "one\n    two\nthree");
    }

    #[test]
    fn accepts_owned_string() {
        let text = "
            one
            two
        "
        .to_string();

        assert_eq!(dedent(text), "one\ntwo");
    }
}

use similar::{ChangeTag, TextDiff};

use crate::input::colorize_tag;

pub(super) fn unified_edit_preview(
    content: &str,
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    if content.match_indices(old_string).take(2).count() != 1 {
        return None;
    }

    let updated = content.replacen(old_string, new_string, 1);
    Some(unified_diff(content, &updated))
}

fn unified_diff(old_content: &str, new_content: &str) -> String {
    const CONTEXT_LINES: usize = 3;

    let text_diff = TextDiff::from_lines(old_content, new_content);
    let mut diff = String::new();

    for group in text_diff.grouped_ops(CONTEXT_LINES) {
        let old_start = group
            .first()
            .map(|op| op.old_range().start + 1)
            .unwrap_or(1);
        let new_start = group
            .first()
            .map(|op| op.new_range().start + 1)
            .unwrap_or(1);
        let old_len = group.iter().map(|op| op.old_range().len()).sum::<usize>();
        let new_len = group.iter().map(|op| op.new_range().len()).sum::<usize>();

        if !diff.is_empty() {
            diff.push('\n');
        }
        diff.push_str(&format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        ));

        for op in group {
            for change in text_diff.iter_changes(&op) {
                diff.push('\n');
                let line = trim_trailing_newline(change.value());
                match change.tag() {
                    ChangeTag::Delete => {
                        diff.push_str(&colorize_tag("red", &format!("-{line}")));
                    }
                    ChangeTag::Insert => {
                        diff.push_str(&colorize_tag("green", &format!("+{line}")));
                    }
                    ChangeTag::Equal => {
                        diff.push(' ');
                        diff.push_str(line);
                    }
                }
            }
        }
    }

    diff
}

fn trim_trailing_newline(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

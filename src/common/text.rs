#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum TruncatePosition {
    Start,
    Middle,
    End,
}

pub(crate) fn truncate_utf8_to_bytes(
    text: &str,
    max_bytes: usize,
    position: TruncatePosition,
) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    if max_bytes == 0 {
        return truncate_marker(text.len());
    }

    match position {
        TruncatePosition::Start => {
            let start = utf8_suffix_start(text, max_bytes);
            let marker = truncate_marker(start);
            format!("{marker}\n{}", &text[start..])
        }
        TruncatePosition::Middle => {
            let prefix_budget = max_bytes / 2;
            let suffix_budget = max_bytes.saturating_sub(prefix_budget);
            let end = utf8_prefix_end(text, prefix_budget);
            let start = utf8_suffix_start(&text[end..], suffix_budget) + end;
            let removed_bytes = start.saturating_sub(end);
            let marker = truncate_marker(removed_bytes);
            format!("{}\n{marker}\n{}", &text[..end], &text[start..])
        }
        TruncatePosition::End => {
            let end = utf8_prefix_end(text, max_bytes);
            let marker = truncate_marker(text.len().saturating_sub(end));
            format!("{}\n{marker}", &text[..end])
        }
    }
}

fn truncate_marker(removed_bytes: usize) -> String {
    format!("[truncated {removed_bytes} bytes]")
}

fn utf8_prefix_end(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }

    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn utf8_suffix_start(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return 0;
    }

    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_short_text_unchanged() {
        assert_eq!(
            truncate_utf8_to_bytes("hello", 5, TruncatePosition::End),
            "hello".to_string()
        );
    }

    #[test]
    fn truncates_end_to_limit() {
        assert_eq!(
            truncate_utf8_to_bytes("hello world", 5, TruncatePosition::End),
            "hello\n[truncated 6 bytes]".to_string()
        );
    }

    #[test]
    fn truncates_end_without_splitting_utf8_character() {
        assert_eq!(
            truncate_utf8_to_bytes("abЖcd", 3, TruncatePosition::End),
            "ab\n[truncated 4 bytes]".to_string()
        );
    }

    #[test]
    fn truncates_start_to_limit() {
        assert_eq!(
            truncate_utf8_to_bytes("hello world", 5, TruncatePosition::Start),
            "[truncated 6 bytes]\nworld".to_string()
        );
    }

    #[test]
    fn truncates_start_without_splitting_utf8_character() {
        assert_eq!(
            truncate_utf8_to_bytes("abЖcd", 3, TruncatePosition::Start),
            "[truncated 4 bytes]\ncd".to_string()
        );
    }

    #[test]
    fn truncates_middle_to_limit() {
        assert_eq!(
            truncate_utf8_to_bytes("hello world", 6, TruncatePosition::Middle),
            "hel\n[truncated 5 bytes]\nrld".to_string()
        );
    }

    #[test]
    fn zero_limit_keeps_only_marker() {
        assert_eq!(
            truncate_utf8_to_bytes("hello", 0, TruncatePosition::End),
            "[truncated 5 bytes]".to_string()
        );
    }
}

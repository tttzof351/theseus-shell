pub fn preprocess_markdown(text: &str) -> String {
    add_table_outer_rules(text)
}

fn add_table_outer_rules(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut index = 0;
    let mut in_fenced_code = false;

    while index < lines.len() {
        let line = lines[index];
        if is_fence_line(line) {
            in_fenced_code = !in_fenced_code;
            result.push(line.to_string());
            index += 1;
            continue;
        }

        if in_fenced_code || is_indented_code_line(line) {
            result.push(line.to_string());
            index += 1;
            continue;
        }

        if is_table_rule_line(line)
            && index + 2 < lines.len()
            && is_table_row_line(lines[index + 1])
            && is_table_rule_line(lines[index + 2])
        {
            let rule = lines[index + 2];
            index = push_table_block(&lines, index, Some(line), rule, &mut result);
            continue;
        }

        if is_table_row_line(line)
            && index + 1 < lines.len()
            && is_table_rule_line(lines[index + 1])
        {
            let rule = lines[index + 1];
            result.push(normalized_table_rule(rule));
            index = push_table_block(&lines, index, None, rule, &mut result);
            continue;
        }

        result.push(line.to_string());
        index += 1;
    }

    result.join("\n")
}

fn push_table_block(
    lines: &[&str],
    mut index: usize,
    top_rule: Option<&str>,
    fallback_rule: &str,
    result: &mut Vec<String>,
) -> usize {
    let mut last_line_was_rule = false;

    while index < lines.len() && is_table_line(lines[index]) {
        result.push(lines[index].to_string());
        last_line_was_rule = is_table_rule_line(lines[index]);
        index += 1;
    }

    if !last_line_was_rule {
        result.push(normalized_table_rule(top_rule.unwrap_or(fallback_rule)));
    }

    index
}

fn normalized_table_rule(line: &str) -> String {
    let column_count = line
        .split('|')
        .filter(|cell| !cell.trim().is_empty())
        .count();
    let cells = std::iter::repeat_n("---", column_count.max(1))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("| {cells} |")
}

fn is_table_line(line: &str) -> bool {
    is_table_row_line(line) || is_table_rule_line(line)
}

fn is_table_row_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|') && !is_table_rule_line(trimmed)
}

fn is_table_rule_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return false;
    }

    trimmed
        .trim_matches('|')
        .split('|')
        .all(|cell| is_table_rule_cell(cell.trim()))
}

fn is_table_rule_cell(cell: &str) -> bool {
    let marker = cell.trim_matches(':');
    marker.len() >= 3 && marker.chars().all(|ch| ch == '-')
}

fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn is_indented_code_line(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_table_outer_rules() {
        let markdown = "| Module | Purpose |\n| --- | --- |\n| agent | LLM |\n";

        assert_eq!(
            preprocess_markdown(markdown),
            "| --- | --- |\n| Module | Purpose |\n| --- | --- |\n| agent | LLM |\n| --- | --- |"
        );
    }

    #[test]
    fn keeps_existing_table_outer_rules() {
        let markdown =
            "| --- | --- |\n| Metric | Value |\n| --- | ---: |\n| messages | 3 |\n| --- | ---: |\n";

        assert_eq!(preprocess_markdown(markdown), markdown.trim_end());
    }
}

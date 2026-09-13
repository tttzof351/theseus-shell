//! Resolve reference links for the terminal adapter without editing the source.
use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
use std::{io, ops::Range};

pub(super) fn resolve(
    source: &str,
    visible_from: usize,
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<String> {
    check()?;
    if !source.contains('[') {
        return Ok(source[visible_from..].to_owned());
    }
    let parser = Parser::new(source);
    let mut edits = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| (definition.span.clone(), None))
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return Ok(source[visible_from..].to_owned());
    }
    struct Link {
        span: Range<usize>,
        label: Option<Range<usize>>,
        destination: String,
    }
    let mut link: Option<Link> = None;
    let mut depth = 0usize;
    let mut covered_end = 0;
    for (event, range) in parser.into_offset_iter() {
        check()?;
        // Reference definitions have no events. Root gaps also contain shadowed
        // definitions, which aren't retained in Parser's first-definition map.
        if depth == 0 {
            definition_gap(source, covered_end..range.start, &mut edits);
            covered_end = covered_end.max(range.end);
        }
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        match event {
            Event::Start(Tag::Link {
                link_type: LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut,
                dest_url,
                ..
            }) => {
                link = Some(Link {
                    span: range,
                    label: None,
                    destination: terminal_destination(&dest_url),
                });
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = link.take() {
                    // A link begun before clear must not resurrect its hidden
                    // label just because a later delta completes its syntax.
                    if link.span.start >= visible_from {
                        let label = link.label.map(|range| &source[range]).unwrap_or("");
                        edits.push((link.span, Some(format!("{label} ({})", link.destination))));
                    }
                }
            }
            _ => {
                if let Some(link) = &mut link {
                    link.label = Some(match link.label.take() {
                        Some(previous) => {
                            previous.start.min(range.start)..previous.end.max(range.end)
                        }
                        None => range,
                    });
                }
            }
        }
    }
    definition_gap(source, covered_end..source.len(), &mut edits);
    edits.sort_by_key(|(range, _)| range.start);
    let mut result = String::new();
    let mut cursor = visible_from;
    for (range, replacement) in edits {
        check()?;
        if range.end <= cursor {
            continue;
        }
        let start = range.start.max(cursor);
        result.push_str(&source[cursor..start]);
        match replacement {
            Some(text) => result.push_str(&text),
            // Retain line boundaries, including for definitions hidden by clear.
            None => result.extend(source[start..range.end].chars().filter(|ch| *ch == '\n')),
        }
        cursor = range.end;
    }
    result.push_str(&source[cursor..]);
    Ok(result)
}

fn definition_gap(
    source: &str,
    range: Range<usize>,
    edits: &mut Vec<(Range<usize>, Option<String>)>,
) {
    let gap = &source[range.clone()];
    if gap.chars().any(|ch| !ch.is_whitespace()) {
        // Keep indentation belonging to the following visible block.
        let end = gap
            .rfind('\n')
            .map(|offset| range.start + offset + 1)
            .unwrap_or(range.end);
        edits.push((range.start..end, None));
    }
}

fn terminal_destination(destination: &str) -> String {
    let mut result = String::new();
    for ch in destination.chars() {
        if ch.is_control() || ch.is_whitespace() || matches!(ch, '`' | '*' | '~' | '|') {
            for byte in ch.to_string().as_bytes() {
                use std::fmt::Write;
                write!(result, "%{byte:02X}").unwrap();
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_late_case_folded_collapsed_and_shortcut_links_with_styled_labels() {
        let source = "[**Guide**][DOC]\n\n[doc][] and [doc]\n\n[doc]: https://example.test/guide \"Title\"\n";
        let rendered = resolve(source, 0, &|| Ok(())).unwrap();
        assert!(
            rendered.contains("**Guide** (https://example.test/guide)"),
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("doc (https://example.test/guide)").count(),
            2,
            "{rendered}"
        );
        assert!(!rendered.contains("[doc]:"), "{rendered}");
    }

    #[test]
    fn code_and_escaped_links_stay_literal_and_hidden_definitions_still_resolve_new_text() {
        let source = "[doc]: https://example.test/guide\n\nHIDDEN\n\n[NEW][doc]\n\n`[CODE][doc]`\n\n```text\n[FENCE][doc]\n```\n\n\\[ESCAPED][doc]\n";
        let rendered = resolve(source, source.find("[NEW]").unwrap(), &|| Ok(())).unwrap();
        assert!(
            rendered.contains("NEW (https://example.test/guide)"),
            "{rendered}"
        );
        assert!(rendered.contains("`[CODE][doc]`") && rendered.contains("[FENCE][doc]"));
        assert!(!rendered.contains("HIDDEN"));
        assert!(!rendered.contains("ESCAPED ("));
    }

    #[test]
    fn first_definition_wins_and_shadowed_definitions_do_not_become_output() {
        let source = "[Guide][doc]\n\n[doc]: https://first.test\n[DOC]: https://second.test\n  \"Second title\"\n\n * CHILD\n";
        let rendered = resolve(source, 0, &|| Ok(())).unwrap();
        assert!(
            rendered.contains("Guide (https://first.test)"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("second.test") && !rendered.contains("Second title"),
            "{rendered}"
        );
        assert!(
            rendered.contains(" * CHILD"),
            "following indentation changed: {rendered}"
        );
    }

    #[test]
    fn clear_inside_a_link_does_not_restore_its_hidden_label() {
        let source = "[HIDDEN][doc]\n\n[doc]: https://example.test/guide\n\nVISIBLE";
        let rendered = resolve(source, "[HIDDEN][do".len(), &|| Ok(())).unwrap();
        assert!(!rendered.contains("HIDDEN"));
        assert!(rendered.contains("VISIBLE"));
    }
}

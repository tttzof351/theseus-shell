//! Preserve the provenance of displayed Markdown characters through termimad.
//!
//! Formatting writes borrowed source slices through `fmt::Write`; style escapes,
//! padding and table borders are separate writes. Keeping those spans avoids
//! guessing which occurrence of a repeated word a formatted cell came from.

use crate::terminal_renderer::managed::SourceCharacter;
use std::{fmt, num::NonZeroUsize, ops::Range};

struct Span {
    rendered: Range<usize>,
    source_start: usize,
}

pub(super) struct SourceWriter<'a> {
    source: &'a str,
    pub text: String,
    spans: Vec<Span>,
}

impl<'a> SourceWriter<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            text: String::new(),
            spans: Vec::new(),
        }
    }

    pub fn character_rows(&self) -> Vec<Vec<SourceCharacter>> {
        struct Collector<'a> {
            spans: &'a [Span],
            next_span: usize,
            byte_end: usize,
            rows: Vec<Vec<SourceCharacter>>,
            line: Vec<SourceCharacter>,
        }
        impl Collector<'_> {
            fn character(&mut self) {
                while self
                    .spans
                    .get(self.next_span)
                    .is_some_and(|s| s.rendered.end < self.byte_end)
                {
                    self.next_span += 1;
                }
                let origin = self.spans.get(self.next_span).and_then(|s| {
                    (s.rendered.start < self.byte_end && self.byte_end <= s.rendered.end)
                        .then(|| {
                            NonZeroUsize::new(s.source_start + self.byte_end - s.rendered.start)
                        })
                        .flatten()
                });
                self.line.push(origin);
            }
        }
        impl vte::Perform for Collector<'_> {
            fn print(&mut self, _: char) {
                self.character();
            }
            fn execute(&mut self, byte: u8) {
                match byte {
                    b'\t' => self.character(),
                    b'\n' => self.rows.push(std::mem::take(&mut self.line)),
                    _ => {}
                }
            }
        }
        let mut parser = vte::Parser::new();
        let mut collector = Collector {
            spans: &self.spans,
            next_span: 0,
            byte_end: 0,
            rows: Vec::new(),
            line: Vec::new(),
        };
        for byte in self.text.as_bytes() {
            collector.byte_end += 1;
            parser.advance(&mut collector, std::slice::from_ref(byte));
        }
        if !collector.line.is_empty() {
            collector.rows.push(collector.line);
        }
        collector.rows
    }
}

impl fmt::Write for SourceWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let source_start = (text.as_ptr() as usize).checked_sub(self.source.as_ptr() as usize);
        if let Some(source_start) = source_start
            .filter(|start| *start <= self.source.len() && text.len() <= self.source.len() - *start)
            && !text.is_empty()
        {
            self.spans.push(Span {
                rendered: self.text.len()..self.text.len() + text.len(),
                source_start,
            });
        }
        self.text.push_str(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, fmt::Write};

    #[test]
    fn repeated_words_and_table_cells_keep_exact_source_positions_across_widths() {
        let source = "**same** same Привет 界\n\n| same repeated words | second column |\n|---|---|\n| repeated same repeated same | same words repeated same |\n";
        let expected = source
            .char_indices()
            .filter(|(_, c)| c.is_alphanumeric())
            .map(|(i, c)| i + c.len_utf8())
            .collect::<BTreeSet<_>>();
        for width in [24, 40, 80] {
            let skin = termimad::MadSkin::default();
            let mut writer = SourceWriter::new(source);
            write!(&mut writer, "{}", skin.text(source, Some(width))).unwrap();
            let rows = writer.character_rows();
            let rendered = crate::application::ansi::ansi_render_lines(&writer.text);
            assert_eq!(rows.len(), rendered.len());
            let mut actual = BTreeSet::new();
            for (origins, line) in rows.iter().zip(&rendered) {
                assert_eq!(origins.len(), line.text.chars().count());
                for (origin, c) in origins.iter().zip(line.text.chars()) {
                    if c.is_alphanumeric() {
                        let end = origin.expect("source character became anonymous").get();
                        assert_eq!(source[..end].chars().last(), Some(c));
                        assert!(
                            actual.insert(end),
                            "same source character rendered twice at {end}"
                        );
                    }
                }
            }
            assert_eq!(actual, expected, "width {width}");
        }
    }
}

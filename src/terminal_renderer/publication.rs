//! Character-level accounting for a partially published stable source group.

use super::{
    CachedLogicalLayout, PhysicalCell, PhysicalRow,
    managed::{PublicationUnit, RowIdentity},
};
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Origin {
    Source(usize),
    Text(usize, usize),
    Prefix(usize, usize),
    Empty(usize, usize),
}

#[derive(Default)]
struct EmittedOrigins {
    // Source identities are byte offsets in one publication group's source.
    // Offset the bitmap so a paragraph late in a document doesn't allocate a
    // bit for every preceding byte. A table/reflow can visit offsets backwards.
    first_word: usize,
    source_words: VecDeque<u64>,
    other: HashSet<Origin>,
}

impl EmittedOrigins {
    fn contains(&self, origin: &Origin) -> bool {
        match *origin {
            Origin::Source(offset) => (offset / 64)
                .checked_sub(self.first_word)
                .and_then(|index| self.source_words.get(index))
                .is_some_and(|word| word & (1 << (offset % 64)) != 0),
            _ => self.other.contains(origin),
        }
    }

    fn insert(&mut self, origin: Origin) {
        if let Origin::Source(offset) = origin {
            let word = offset / 64;
            if self.source_words.is_empty() {
                self.first_word = word;
            } else if word < self.first_word {
                let extra = self.first_word - word;
                self.source_words.reserve(extra);
                for _ in 0..extra {
                    self.source_words.push_front(0);
                }
                self.first_word = word;
            }
            let index = word - self.first_word;
            if index >= self.source_words.len() {
                self.source_words.resize(index + 1, 0);
            }
            self.source_words[index] |= 1 << (offset % 64);
        } else {
            self.other.insert(origin);
        }
    }
}

pub(super) struct PartialPublication {
    pub id: RowIdentity,
    pub cursor: usize,
    width: usize,
    emitted: EmittedOrigins,
}

pub(super) struct RowPublication {
    pub row: PhysicalRow,
    origins: Vec<Origin>,
}

impl PartialPublication {
    pub fn new(id: RowIdentity, width: usize) -> Self {
        Self {
            id,
            cursor: 0,
            width,
            emitted: EmittedOrigins::default(),
        }
    }

    pub fn resize(&mut self, width: usize) {
        if self.width != width {
            self.width = width;
            self.cursor = 0;
        }
    }

    pub fn commit(&mut self, row: &RowPublication) {
        for origin in &row.origins {
            self.emitted.insert(*origin);
        }
    }

    pub fn row(
        &self,
        layout: &CachedLogicalLayout,
        row: usize,
        line: usize,
        unit: &PublicationUnit,
    ) -> Option<RowPublication> {
        let physical = &layout.rows[row];
        let provenance = unit.origins.as_ref().and_then(|lines| lines.get(line));
        let mapped =
            provenance.is_some_and(|origins| origins.characters.iter().any(Option::is_some));
        let preserve_columns = provenance.is_some_and(|origins| origins.preserve_columns);
        let mut next = PhysicalRow::empty(physical.cells.len());
        let mut column = 0;
        let mut character = layout.row_text_ranges[row].start;
        let mut prefix_character = 0;
        let mut keys = Vec::new();
        let mut fresh = false;
        for (old_column, cell) in physical.cells.iter().enumerate() {
            let PhysicalCell::Glyph { text, width, style } = cell else {
                continue;
            };
            let prefix = row == 0 && old_column < layout.prefix_width;
            let count = if !prefix && layout.text_characters.get(character) == Some(&'\t') {
                1
            } else {
                text.chars().count()
            };
            let key_start = keys.len();
            let mut glyph_fresh = false;
            for offset in 0..count {
                let key = if prefix {
                    Some(Origin::Prefix(line, prefix_character + offset))
                } else if mapped {
                    provenance
                        .and_then(|p| p.characters.get(character + offset))
                        .copied()
                        .flatten()
                        .map(|n| Origin::Source(n.get()))
                } else {
                    Some(Origin::Text(line, character + offset))
                };
                if let Some(key) = key {
                    glyph_fresh |= !self.emitted.contains(&key);
                    keys.push(key);
                }
            }
            if prefix {
                prefix_character += count;
            } else {
                character += count;
            }
            fresh |= glyph_fresh;
            let consumed = keys.len() > key_start && !glyph_fresh;
            if preserve_columns {
                column = old_column;
            }
            if !consumed {
                next.cells[column] = PhysicalCell::Glyph {
                    text: text.clone(),
                    width: *width,
                    style: *style,
                };
                for cell in &mut next.cells[column + 1..column + width] {
                    *cell = PhysicalCell::Continuation {
                        leading_column: column,
                    };
                }
                column += width;
            }
        }
        if keys.is_empty() {
            // Empty lines/rules still have a once-only publication identity.
            let line_id = provenance
                .and_then(|p| p.characters.iter().flatten().next())
                .map(|n| n.get())
                .unwrap_or(line);
            let key = Origin::Empty(line_id, row);
            fresh = !self.emitted.contains(&key);
            keys.push(key);
        }
        fresh.then_some(RowPublication {
            row: next,
            origins: keys,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordered_source_offsets_match_a_set_without_allocating_the_document_prefix() {
        let mut emitted = EmittedOrigins::default();
        let mut expected = HashSet::new();
        let base = 1_000_000_000;
        // Word boundaries, growth in both directions, and duplicate origins.
        let mut offsets = vec![65, 63, 64, 127, 128, 2048, 0, 1, 4096, 63];
        let mut seed = 17_u32;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            offsets.push(seed as usize % 4097);
        }
        for offset in offsets {
            let origin = Origin::Source(base + offset);
            assert_eq!(emitted.contains(&origin), expected.contains(&origin));
            emitted.insert(origin);
            expected.insert(origin);
        }
        for offset in base - 1..=base + 4097 {
            let origin = Origin::Source(offset);
            assert_eq!(
                emitted.contains(&origin),
                expected.contains(&origin),
                "{offset}"
            );
        }
        assert!(
            emitted.source_words.len() <= 65,
            "bitmap retained the document prefix"
        );
        assert!(emitted.other.is_empty());
    }

    #[test]
    fn source_text_prefix_and_empty_row_identities_do_not_alias() {
        let mut emitted = EmittedOrigins::default();
        let origins = [
            Origin::Source(42),
            Origin::Text(0, 42),
            Origin::Text(1, 42),
            Origin::Prefix(0, 42),
            Origin::Prefix(1, 42),
            Origin::Empty(0, 42),
            Origin::Empty(1, 42),
        ];
        for (index, origin) in origins.iter().enumerate() {
            assert!(!emitted.contains(origin));
            emitted.insert(*origin);
            for old in &origins[..=index] {
                assert!(emitted.contains(old));
            }
        }
    }
}

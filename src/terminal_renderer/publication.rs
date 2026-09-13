//! Character-level accounting for a partially published stable source group.

use super::{
    CachedLogicalLayout, PhysicalCell, PhysicalRow,
    managed::{PublicationUnit, RowIdentity},
};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Origin {
    Source(usize),
    Text(usize, usize),
    Prefix(usize, usize),
    Empty(usize, usize),
}

pub(super) struct PartialPublication {
    pub id: RowIdentity,
    pub cursor: usize,
    width: usize,
    emitted: HashSet<Origin>,
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
            emitted: HashSet::new(),
        }
    }

    pub fn resize(&mut self, width: usize) {
        if self.width != width {
            self.width = width;
            self.cursor = 0;
        }
    }

    pub fn commit(&mut self, row: &RowPublication) {
        self.emitted.extend(row.origins.iter().copied());
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

//! Character-indexed style runs. Uniform output costs one run, not one style per scalar.
use super::CellStyle;
use std::ops::{Index, Range};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellStyles {
    runs: Vec<(usize, CellStyle)>, // exclusive character end, style
}

impl CellStyles {
    pub fn plain(length: usize) -> Self {
        let mut styles = Self::default();
        styles.push_run(length, CellStyle::default());
        styles
    }

    pub fn len(&self) -> usize {
        self.runs.last().map_or(0, |run| run.0)
    }
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<&CellStyle> {
        self.runs
            .get(self.runs.partition_point(|run| run.0 <= index))
            .map(|run| &run.1)
    }
    pub fn last(&self) -> Option<&CellStyle> {
        self.runs.last().map(|run| &run.1)
    }
    pub fn iter(&self) -> impl Iterator<Item = &CellStyle> {
        let mut start = 0;
        self.runs.iter().flat_map(move |(end, style)| {
            let count = end - start;
            start = *end;
            std::iter::repeat_n(style, count)
        })
    }
    pub fn map(&mut self, mut update: impl FnMut(&mut CellStyle)) {
        let old = std::mem::take(&mut self.runs);
        for (end, mut style) in old {
            update(&mut style);
            self.push_run(end, style);
        }
    }
    pub fn set_range(&mut self, range: Range<usize>, style: CellStyle) {
        let end = range.end.min(self.len());
        let start = range.start.min(end);
        if start == end {
            return;
        }
        let old = std::mem::take(&mut self.runs);
        let mut position = 0;
        for (run_end, old_style) in old {
            if position < start {
                self.push_run(run_end.min(start), old_style);
            }
            if run_end > start && position < end {
                self.push_run(run_end.min(end), style);
            }
            if run_end > end {
                self.push_run(run_end, old_style);
            }
            position = run_end;
        }
    }
    pub fn slice(&self, range: Range<usize>) -> Self {
        let mut result = Self::default();
        let end = range.end.min(self.len());
        let first = self.runs.partition_point(|run| run.0 <= range.start);
        for &(run_end, style) in &self.runs[first..] {
            if run_end > range.start {
                result.push_run(run_end.min(end).saturating_sub(range.start), style);
            }
            if run_end >= end {
                break;
            }
        }
        result
    }
    fn push_run(&mut self, end: usize, style: CellStyle) {
        if end <= self.len() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.1 == style
        {
            last.0 = end;
        } else {
            self.runs.push((end, style));
        }
    }
}

impl From<Vec<CellStyle>> for CellStyles {
    fn from(styles: Vec<CellStyle>) -> Self {
        let mut result = Self::default();
        for (index, style) in styles.into_iter().enumerate() {
            result.push_run(index + 1, style);
        }
        result
    }
}
impl Index<usize> for CellStyles {
    type Output = CellStyle;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("style character index")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_slices_and_mapping_match_dense_styles() {
        let mut dense = vec![CellStyle::default(); 100];
        let mut compact = CellStyles::plain(100);
        for (start, end) in [(5, 90), (0, 2), (30, 40), (20, 100), (0, 100), (99, 120)] {
            let style = CellStyle {
                bold: start % 2 == 0,
                italic: end % 3 == 0,
                ..Default::default()
            };
            for item in dense.iter_mut().take(end).skip(start) {
                *item = style;
            }
            compact.set_range(start..end, style);
            assert_eq!(compact.iter().copied().collect::<Vec<_>>(), dense);
            assert_eq!(
                compact.slice(13..71).iter().copied().collect::<Vec<_>>(),
                dense[13..71]
            );
        }
        compact.map(|style| style.bold = true);
        for style in &mut dense {
            style.bold = true;
        }
        assert_eq!(compact, CellStyles::from(dense));
    }
    #[test]
    fn uniform_style_storage_does_not_scale_with_line_length() {
        let styles = CellStyles::plain(2_000_000);
        assert_eq!(styles.runs.len(), 1);
        assert_eq!(styles.len(), 2_000_000);
    }
}

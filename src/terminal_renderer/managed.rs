//! A mutable viewport must never scroll its previous revisions into terminal history.
//! Only complete logical lines in the stable document prefix may be published.

use super::publication::PartialPublication;
use super::*;
use std::{num::NonZeroUsize, sync::Arc};

pub(crate) type SourceCharacter = Option<NonZeroUsize>;

#[derive(Debug, Clone)]
pub(crate) struct LineOrigins {
    pub characters: Arc<[SourceCharacter]>,
    pub preserve_columns: bool,
}

// Publication shares the frame with input/status. A completed multi-thousand
// line answer must not spend an entire frame replaying its native history.
const PUBLICATION_ROWS_PER_FRAME: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RowIdentity {
    pub block: u64,
    pub group: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicationAnchor {
    pub id: RowIdentity,
    pub characters: usize,
}

/// Native publication performed while an external program owned the terminal.
pub(crate) struct ExternalPublication {
    pub scrolled: usize,
    pub cleared: bool,
    pub source: Option<PublicationAnchor>,
}

#[derive(Debug, Clone)]
pub(crate) struct PublicationUnit {
    pub id: RowIdentity,
    /// Exclusive end in the current width's logical RenderLines.
    pub end: usize,
    pub stable: bool,
    pub origins: Option<Arc<[LineOrigins]>>,
}

#[derive(Default)]
pub(crate) struct ManagedRenderer {
    layout: IndexedPhysicalLayout,
    previous: Option<PhysicalTerminal>,
    published: Option<RowIdentity>,
    published_floor: Option<RowIdentity>,
    view_top: Option<usize>,
    last_viewport_top: usize,
    pending_scroll: isize,
    scroll_anchor: Option<(RowIdentity, usize)>,
    publication_pending: bool,
    last_publication: Vec<PublicationUnit>,
    partial: Option<PartialPublication>,
}

impl ManagedRenderer {
    pub(crate) fn needs_reflow(&self, width: usize) -> bool {
        self.layout.width != width
    }

    /// Keep the last document layout while its new width is prepared elsewhere.
    /// Only viewport rows and the editor/status footer are touched here; no
    /// document reflow or native publication is allowed in this temporary frame.
    pub(crate) fn render_pending_resize(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        publication: &[PublicationUnit],
        footer_start: usize,
        size: TerminalSize,
    ) -> io::Result<()> {
        let mut footer = VirtualScreen::from_render_lines(
            (footer_start..screen.lines.len())
                .map(|index| RenderLine {
                    prefix: screen.prefixes[index].clone(),
                    text: screen.lines[index].clone(),
                    prefix_styles: screen.prefix_styles[index].clone(),
                    styles: screen.line_styles[index].clone(),
                })
                .collect(),
            VirtualCursor {
                line: screen.cursor.line.saturating_sub(footer_start),
                char_offset: screen.cursor.char_offset,
            },
            screen.cursor_visible,
        );
        if footer.lines.is_empty() {
            footer.push_render_line(&RenderLine::plain(""));
            footer.cursor = VirtualCursor {
                line: 0,
                char_offset: 0,
            };
        }
        let mut footer_layout = IndexedPhysicalLayout::default();
        let footer_frame = footer_layout.layout(&footer, size);
        let footer_height = footer_layout.heights.total().min(size.height);
        let output_height = size.height - footer_height;
        let document_end = self
            .layout
            .heights
            .prefix_sum(footer_start.min(self.layout.logical_lines.len()));
        let following_top = document_end.saturating_sub(output_height);
        let mut top = self.view_top.unwrap_or(following_top);
        if self.view_top.is_some() {
            top = top
                .saturating_add_signed(-self.pending_scroll)
                .min(following_top);
            if self.pending_scroll < 0 && top == following_top {
                self.follow_output();
            }
            self.pending_scroll = 0;
        }
        if self.view_top.is_none() {
            let floor = publication
                .iter()
                .take_while(|unit| self.published_floor.is_some_and(|id| unit.id <= id))
                .last()
                .map(|unit| unit.end)
                .unwrap_or(0);
            top = top.max(self.layout.heights.prefix_sum(floor));
        } else {
            self.view_top = Some(top);
            self.scroll_anchor = publication.iter().enumerate().find_map(|(index, unit)| {
                if self.layout.heights.prefix_sum(unit.end) <= top {
                    return None;
                }
                let first = index
                    .checked_sub(1)
                    .map(|i| publication[i].end)
                    .unwrap_or(0);
                Some((
                    unit.id,
                    top.saturating_sub(self.layout.heights.prefix_sum(first)),
                ))
            });
        }
        self.last_viewport_top = top;
        let rows = (0..output_height)
            .map(|row| {
                let absolute = top + row;
                if absolute >= document_end {
                    return PhysicalRow::empty(size.width);
                }
                let Some((line, row)) = self.layout.heights.line_containing_row(absolute) else {
                    return PhysicalRow::empty(size.width);
                };
                if self.view_top.is_none()
                    && let Some(partial) = &self.partial
                    && let Some(index) = publication.iter().position(|unit| unit.id == partial.id)
                {
                    let unit = &publication[index];
                    let first = index
                        .checked_sub(1)
                        .map(|i| publication[i].end)
                        .unwrap_or(0);
                    if first <= line && line < unit.end {
                        return partial
                            .row(&self.layout.logical_lines[line], row, line - first, unit)
                            .map(|row| fit_row(&row.row, size.width))
                            .unwrap_or_else(|| PhysicalRow::empty(size.width));
                    }
                }
                fit_row(&self.layout.logical_lines[line].rows[row], size.width)
            })
            .chain(footer_frame.rows.into_iter().take(footer_height))
            .collect();
        let next = PhysicalTerminal {
            size,
            rows,
            cursor: PhysicalCursor {
                position: PhysicalPosition {
                    row: output_height + footer_frame.cursor.position.row,
                    column: footer_frame.cursor.position.column,
                },
                visible: footer_frame.cursor.visible,
            },
            viewport_top: 0,
        };
        apply_diff(
            output,
            &diff_physical_terminal(self.previous.as_ref(), &next),
        )?;
        output.flush()?;
        self.previous = Some(next);
        Ok(())
    }

    pub(crate) fn install_layout(&mut self, layout: super::IndexedPhysicalLayout) {
        self.layout = layout;
    }
    pub(crate) fn publication_pending(&self) -> bool {
        self.publication_pending
    }

    pub(crate) fn scroll_back(&mut self, rows: usize) {
        self.view_top.get_or_insert(self.last_viewport_top);
        self.pending_scroll = self.pending_scroll.saturating_add(rows as isize);
    }

    pub(crate) fn scroll_forward(&mut self, rows: usize) {
        self.view_top.get_or_insert(self.last_viewport_top);
        self.pending_scroll = self.pending_scroll.saturating_sub(rows as isize);
    }

    pub(crate) fn follow_output(&mut self) {
        self.view_top = None;
        self.scroll_anchor = None;
        self.pending_scroll = 0;
    }

    /// Forget a cleared document. Terminal scrollback itself is not cleared.
    pub(crate) fn clear_document(&mut self) {
        self.published = None;
        self.published_floor = None;
        self.follow_output();
        self.previous = None;
        self.layout = IndexedPhysicalLayout::default();
        self.last_viewport_top = 0;
        self.publication_pending = false;
        self.last_publication.clear();
        self.partial = None;
    }

    /// Adopt only rows actually scrolled out by the terminal lease. Visible
    /// shell output remains eligible for publication by a later managed frame.
    pub(crate) fn adopt_external_output(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        publication: &[PublicationUnit],
        size: TerminalSize,
        external: ExternalPublication,
    ) -> io::Result<()> {
        let ExternalPublication {
            scrolled,
            cleared,
            source,
        } = external;
        if cleared {
            self.clear_document();
        }
        if let Some(anchor) = source {
            self.layout.layout(screen, size);
            let index = publication
                .iter()
                .position(|unit| unit.id == anchor.id)
                .ok_or_else(|| {
                    io::Error::other("shell publication anchor is missing from the document")
                })?;
            let first = index
                .checked_sub(1)
                .map(|i| publication[i].end)
                .unwrap_or(0);
            let end = publication[index].end;
            debug_assert_eq!(
                first + 1,
                end,
                "shell source groups contain one logical line"
            );
            let line = &self.layout.logical_lines[first];
            let offset = line
                .text
                .char_indices()
                .nth(anchor.characters)
                .map(|(byte, _)| byte)
                .unwrap_or(line.text.len());
            if offset < line.text.len() {
                // The byte anchor survives a resize inside the PTY lease. Only
                // the unscrolled source suffix is published at the current width.
                let tail = CachedLogicalLayout::build(
                    "",
                    &line.text[offset..],
                    &[],
                    &line.styles[anchor.characters.min(line.styles.len())..],
                    None,
                    size.width,
                );
                queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
                for row in &tail.rows {
                    publish_row(output, row)?;
                }
                output.flush()?;
            }
            self.published = Some(anchor.id);
            self.partial = None;
            self.published_floor = self.published;
            self.previous = None;
            self.follow_output();
            return Ok(());
        }
        if scrolled == 0 {
            self.previous = None;
            self.follow_output();
            return Ok(());
        }
        // With no shell source in native history, the scrolled rows belong to
        // the submission frame. Use that frame's layout, not a reflow at the
        // post-lease width, to identify its already displayed source groups.
        let publication = if cleared {
            publication
        } else {
            &self.last_publication
        };
        if cleared {
            self.layout.layout(screen, size);
        }
        let mut native_end = self.last_viewport_top + scrolled;
        // Raw PTY scrolling can bisect a wrapped source group. Complete only
        // its unscrolled suffix at the old width, then keep the normal stable
        // group watermark. Replaying the group would duplicate its first rows;
        // marking it complete without this suffix would lose the remaining rows.
        let mut start = 0;
        for unit in publication.iter().take_while(|unit| unit.stable) {
            let end = self.layout.heights.prefix_sum(unit.end);
            if start < native_end && native_end < end {
                queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
                for absolute in native_end..end {
                    let (line, row) = self
                        .layout
                        .heights
                        .line_containing_row(absolute)
                        .expect("publication row");
                    publish_row_at_width(
                        output,
                        &self.layout.logical_lines[line].rows[row],
                        size.width,
                    )?;
                }
                output.flush()?;
                native_end = end;
                break;
            }
            start = end;
        }
        self.published = publication
            .iter()
            .take_while(|unit| {
                unit.stable && self.layout.heights.prefix_sum(unit.end) <= native_end
            })
            .last()
            .map(|unit| unit.id);
        self.published_floor = self.published;
        self.partial = None;
        self.previous = None;
        self.follow_output();
        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        publication: &[PublicationUnit],
        footer_start: usize,
        pin_footer: bool,
        size: TerminalSize,
    ) -> io::Result<()> {
        let mut next = self.layout.layout(screen, size);
        let absolute_cursor_row = next.cursor.position.row + next.viewport_top;
        let footer_row = self.layout.heights.prefix_sum(footer_start);
        let pinned = pin_footer || self.view_top.is_some();
        let footer_height = if pinned {
            (self.layout.heights.total() - footer_row).min(size.height)
        } else {
            0
        };
        let output_height = size.height - footer_height;
        let footer_clip = absolute_cursor_row
            .saturating_sub(footer_row)
            .saturating_add(1)
            .saturating_sub(footer_height);
        let following_top = if pinned {
            footer_row.saturating_sub(output_height)
        } else {
            next.viewport_top
        };
        let all_units = publication;
        let publication = &publication[..publication.partition_point(|unit| unit.stable)];
        let published_units =
            publication.partition_point(|unit| self.published.is_some_and(|id| unit.id <= id));
        let mut viewport_top = self.view_top.unwrap_or(following_top);
        if self.view_top.is_some() {
            if let Some((anchor, offset)) = self.scroll_anchor
                && let Some(index) = all_units.iter().position(|unit| unit.id >= anchor)
            {
                let start_line = index
                    .checked_sub(1)
                    .map(|previous| all_units[previous].end)
                    .unwrap_or(0);
                let start = self.layout.heights.prefix_sum(start_line);
                let end = self.layout.heights.prefix_sum(all_units[index].end);
                viewport_top = start + offset.min(end.saturating_sub(start + 1));
            }
            viewport_top = viewport_top
                .saturating_add_signed(-self.pending_scroll)
                .min(following_top);
            if self.pending_scroll < 0 && viewport_top == following_top {
                self.follow_output();
            }
            self.pending_scroll = 0;
        }
        if self.view_top.is_none() {
            // Resizing to a taller/wider viewport must not copy native history
            // back onto the live screen. Published groups keep their identities.
            let floor_lines = publication
                .iter()
                .take_while(|unit| self.published_floor.is_some_and(|id| unit.id <= id))
                .last()
                .map(|unit| unit.end)
                .unwrap_or(0);
            viewport_top = viewport_top.max(self.layout.heights.prefix_sum(floor_lines));
        }
        let mut budget = PUBLICATION_ROWS_PER_FRAME;
        let mut wrote = false;
        self.publication_pending = false;
        if self.view_top.is_none() {
            for index in published_units..publication.len() {
                let unit = &publication[index];
                let first = index
                    .checked_sub(1)
                    .map(|i| publication[i].end)
                    .unwrap_or(0);
                let start = self.layout.heights.prefix_sum(first);
                let end = self.layout.heights.prefix_sum(unit.end);
                if start >= viewport_top {
                    break;
                }
                let partial = self
                    .partial
                    .get_or_insert_with(|| PartialPublication::new(unit.id, size.width));
                debug_assert_eq!(partial.id, unit.id);
                partial.resize(size.width);
                let limit = viewport_top.min(end).saturating_sub(start);
                while partial.cursor < limit {
                    if budget == 0 {
                        self.publication_pending = true;
                        break;
                    }
                    let (line, row) = self
                        .layout
                        .heights
                        .line_containing_row(start + partial.cursor)
                        .expect("publication row");
                    if let Some(next) =
                        partial.row(&self.layout.logical_lines[line], row, line - first, unit)
                    {
                        if !wrote {
                            queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
                            wrote = true;
                        }
                        publish_row(output, &next.row)?;
                        partial.commit(&next);
                        budget -= 1;
                    }
                    partial.cursor += 1;
                }
                if partial.cursor < end - start {
                    break;
                }
                self.published = Some(unit.id);
                self.published_floor = self.published;
                self.partial = None;
            }
        }
        if wrote {
            self.previous = None;
        }

        self.last_viewport_top = viewport_top;
        if self.view_top.is_some() {
            self.view_top = Some(viewport_top);
            self.scroll_anchor = all_units.iter().enumerate().find_map(|(index, unit)| {
                let end = self.layout.heights.prefix_sum(unit.end);
                if end <= viewport_top {
                    return None;
                }
                let start_line = index
                    .checked_sub(1)
                    .map(|previous| all_units[previous].end)
                    .unwrap_or(0);
                Some((
                    unit.id,
                    viewport_top.saturating_sub(self.layout.heights.prefix_sum(start_line)),
                ))
            });
        }
        next.rows = (0..size.height)
            .map(|row| {
                let absolute = if pinned && row >= output_height {
                    Some(footer_row + footer_clip + row - output_height)
                } else {
                    let absolute = viewport_top + row;
                    (!pinned || absolute < footer_row).then_some(absolute)
                };
                absolute
                    .and_then(|absolute| self.layout.heights.line_containing_row(absolute))
                    .map(|(line, row)| {
                        if self.view_top.is_none()
                            && let Some(partial) = &self.partial
                            && let Some(index) = publication.iter().position(|u| u.id == partial.id)
                        {
                            let unit = &publication[index];
                            let first = index
                                .checked_sub(1)
                                .map(|i| publication[i].end)
                                .unwrap_or(0);
                            if first <= line && line < unit.end {
                                return partial
                                    .row(&self.layout.logical_lines[line], row, line - first, unit)
                                    .map(|r| r.row)
                                    .unwrap_or_else(|| PhysicalRow::empty(size.width));
                            }
                        }
                        self.layout.logical_lines[line].rows[row].clone()
                    })
                    .unwrap_or_else(|| PhysicalRow::empty(size.width))
            })
            .collect();
        next.cursor.position.row = if pinned {
            output_height + absolute_cursor_row.saturating_sub(footer_row + footer_clip)
        } else {
            absolute_cursor_row.saturating_sub(viewport_top)
        };
        // Physical diff is strictly in-place. Publishing above is the only place
        // allowed to scroll the actual terminal; preview growth/shrink is a repaint.
        next.viewport_top = 0;
        let diff = diff_physical_terminal(self.previous.as_ref(), &next);
        apply_diff(output, &diff)?;
        output.flush()?;
        self.previous = Some(next);
        self.last_publication = publication.to_vec();
        Ok(())
    }
}

/// Crop/pad cached cells without wrapping or leaving half of a wide character.
fn fit_row(row: &PhysicalRow, width: usize) -> PhysicalRow {
    let mut fitted = PhysicalRow::empty(width);
    for (column, cell) in row.cells.iter().enumerate().take(width) {
        if let PhysicalCell::Glyph {
            width: glyph_width, ..
        } = cell
            && column + glyph_width <= width
        {
            fitted.cells[column] = cell.clone();
            for offset in 1..*glyph_width {
                fitted.cells[column + offset] = PhysicalCell::Continuation {
                    leading_column: column,
                };
            }
        }
    }
    fitted
}

fn publish_row(output: &mut impl Write, row: &PhysicalRow) -> io::Result<()> {
    queue!(
        output,
        MoveTo(0, 0),
        Print("\x1b[0m"),
        Print(row_terminal_text(row, 0, row.cells.len())),
        Print("\x1b[0m"),
        ScrollUp(1)
    )
}

fn publish_row_at_width(
    output: &mut impl Write,
    row: &PhysicalRow,
    width: usize,
) -> io::Result<()> {
    if row.cells.len() <= width {
        return publish_row(output, row);
    }
    let last = row
        .cells
        .iter()
        .rposition(|cell| !matches!(cell, PhysicalCell::Empty))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut text = String::new();
    let mut styles = Vec::new();
    for cell in &row.cells[..last] {
        match cell {
            PhysicalCell::Glyph {
                text: glyph, style, ..
            } => {
                text.push_str(glyph);
                styles.extend(std::iter::repeat_n(*style, glyph.chars().count()));
            }
            PhysicalCell::Empty => {
                text.push(' ');
                styles.push(CellStyle::default());
            }
            PhysicalCell::Continuation { .. } => {}
        }
    }
    let layout = CachedLogicalLayout::build("", &text, &[], &styles, None, width);
    for row in &layout.rows {
        publish_row(output, row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_resize_keeps_document_cache_and_publication_while_editor_reflows() {
        let mut renderer = ManagedRenderer::default();
        let mut terminal = vt100::Parser::new(8, 30, 2000);
        let mut scene = screen(vec![
            RenderLine::plain(
                (0..1200)
                    .map(|i| format!("ITEM_{i:04} "))
                    .collect::<String>(),
            ),
            RenderLine::new("user> ", "CURRENT_DRAFT"),
        ]);
        scene.cursor.char_offset = "CURRENT_DRAFT".len();
        let units = publication(1);
        let mut bytes = Vec::new();
        renderer
            .render(
                &mut bytes,
                &scene,
                &units,
                1,
                true,
                TerminalSize::new(30, 8),
            )
            .unwrap();
        terminal.process(&bytes);
        assert!(renderer.publication_pending());
        let cache = renderer.layout.logical_lines.clone();
        let published = renderer.published;
        let partial_cursor = renderer.partial.as_ref().unwrap().cursor;
        let native_rows = history(&mut terminal).len();
        for width in [12, 80, 20] {
            terminal.screen_mut().set_size(8, width);
            bytes.clear();
            renderer
                .render_pending_resize(&mut bytes, &scene, &units, 1, TerminalSize::new(width, 8))
                .unwrap();
            terminal.process(&bytes);
            assert_eq!(renderer.layout.width, 30);
            assert_eq!(
                renderer.layout.logical_lines, cache,
                "document was reflowed in the UI"
            );
            assert_eq!(renderer.published, published);
            assert_eq!(renderer.partial.as_ref().unwrap().cursor, partial_cursor);
            assert_eq!(
                history(&mut terminal).len(),
                native_rows,
                "temporary frame scrolled"
            );
            let visible = terminal
                .screen()
                .contents()
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            assert!(
                visible.contains("CURRENT_DRAFT"),
                "width {width}: {visible}"
            );
            assert!(terminal.screen().cursor_position().1 < width);
        }
        let following = renderer.last_viewport_top;
        renderer.scroll_back(5);
        renderer
            .render_pending_resize(&mut Vec::new(), &scene, &units, 1, TerminalSize::new(20, 8))
            .unwrap();
        assert_eq!(renderer.last_viewport_top, following - 5);
        renderer.scroll_forward(5);
        renderer
            .render_pending_resize(&mut Vec::new(), &scene, &units, 1, TerminalSize::new(20, 8))
            .unwrap();
        assert!(renderer.view_top.is_none());
        let mut prepared = IndexedPhysicalLayout::default();
        prepared.layout(&scene, TerminalSize::new(20, 8));
        renderer.install_layout(prepared);
        // The two browse frames above used a discard writer. Force a fresh VT
        // frame now, as the physical fixture didn't observe those frames.
        renderer.previous = None;
        for _ in 0..20 {
            bytes.clear();
            renderer
                .render(
                    &mut bytes,
                    &scene,
                    &units,
                    1,
                    true,
                    TerminalSize::new(20, 8),
                )
                .unwrap();
            terminal.process(&bytes);
            if !renderer.publication_pending() {
                break;
            }
        }
        assert!(!renderer.publication_pending());
        let mut rows = history(&mut terminal);
        rows.extend(terminal.screen().rows(0, 20));
        let text = rows
            .join("")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        for i in 0..1200 {
            let marker = format!("ITEM_{i:04}");
            assert_eq!(
                text.matches(&marker).count(),
                1,
                "{marker} lost or republished"
            );
        }
    }

    #[test]
    fn pending_resize_crops_whole_wide_glyphs_and_preserves_styles() {
        let style = CellStyle {
            bold: true,
            ..CellStyle::default()
        };
        let cached = CachedLogicalLayout::build("", "ab界z", &[], &[style; 4], None, 5);
        let narrow = fit_row(&cached.rows[0], 3);
        assert_eq!(narrow.cells[2], PhysicalCell::Empty);
        let exact = fit_row(&cached.rows[0], 4);
        assert!(
            matches!(&exact.cells[2], PhysicalCell::Glyph { text, width: 2, style: s } if text == "界" && *s == style)
        );
        assert_eq!(
            exact.cells[3],
            PhysicalCell::Continuation { leading_column: 2 }
        );
        let wide = fit_row(&cached.rows[0], 8);
        assert_eq!(&wide.cells[..5], cached.rows[0].cells.as_slice());
        assert!(
            wide.cells[5..]
                .iter()
                .all(|cell| *cell == PhysicalCell::Empty)
        );
    }

    #[test]
    fn one_large_wrapped_group_obeys_frame_budget_and_keeps_its_tail_visible() {
        let mut renderer = ManagedRenderer::default();
        let mut size = TerminalSize::new(30, 8);
        let mut terminal = vt100::Parser::new(8, 30, 2000);
        let text = (0..1200)
            .map(|i| format!("ITEM_{i:04} "))
            .collect::<String>();
        let scene = screen(vec![RenderLine::plain(text), RenderLine::plain("user>")]);
        let units = vec![PublicationUnit {
            id: RowIdentity { block: 0, group: 0 },
            end: 1,
            stable: true,
            origins: None,
        }];
        let mut previous = 0;
        for frame in 0..20 {
            if frame == 1 {
                size = TerminalSize::new(23, 8);
                terminal.screen_mut().set_size(8, 23);
            }
            let mut bytes = Vec::new();
            renderer
                .render(&mut bytes, &scene, &units, 1, false, size)
                .unwrap();
            terminal.process(&bytes);
            if frame == 0 {
                assert!(
                    terminal.screen().contents().contains("ITEM_1199"),
                    "completed tail disappeared"
                );
            }
            terminal.screen_mut().set_scrollback(2000);
            let native = terminal.screen().scrollback();
            assert!(
                native - previous <= PUBLICATION_ROWS_PER_FRAME,
                "frame published {} rows",
                native - previous
            );
            previous = native;
            terminal.screen_mut().set_scrollback(0);
            if !renderer.publication_pending() {
                break;
            }
            assert!(frame < 19, "publication did not converge");
        }
        let mut rows = history(&mut terminal);
        rows.extend(terminal.screen().rows(0, 23));
        let text = rows
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        for i in 0..1200 {
            let marker = format!("ITEM_{i:04}");
            assert_eq!(text.matches(&marker).count(), 1, "{marker}");
        }
    }

    #[test]
    fn partially_published_group_survives_shell_handoff_after_reflow() {
        let mut renderer = ManagedRenderer::default();
        let mut terminal = vt100::Parser::new(8, 30, 2000);
        let mut lines = vec![
            RenderLine::plain(
                (0..1200)
                    .map(|i| format!("ITEM_{i:04} "))
                    .collect::<String>(),
            ),
            RenderLine::plain("command> true"),
        ];
        for width in [30, 23] {
            terminal.screen_mut().set_size(8, width);
            for frame in 0..20 {
                let mut bytes = Vec::new();
                renderer
                    .render(
                        &mut bytes,
                        &screen(lines.clone()),
                        &publication(2),
                        2,
                        false,
                        TerminalSize::new(width, 8),
                    )
                    .unwrap();
                terminal.process(&bytes);
                if !renderer.publication_pending() {
                    break;
                }
                assert!(frame < 19, "publication did not converge");
            }
        }
        assert!(
            renderer.partial.is_some(),
            "fixture must bisect a source group"
        );
        terminal.screen_mut().set_scrollback(2000);
        let before = terminal.screen().scrollback();
        terminal.screen_mut().set_scrollback(0);
        terminal.process(b"\r\n\r\n");
        terminal.screen_mut().set_scrollback(2000);
        let scrolled = terminal.screen().scrollback() - before;
        terminal.screen_mut().set_scrollback(0);
        assert!(scrolled > 0);
        terminal.screen_mut().set_size(8, 45);
        let size = TerminalSize::new(45, 8);
        lines.push(RenderLine::plain("user>"));
        let mut bytes = Vec::new();
        renderer
            .adopt_external_output(
                &mut bytes,
                &screen(lines.clone()),
                &publication(2),
                size,
                ExternalPublication {
                    scrolled,
                    cleared: false,
                    source: None,
                },
            )
            .unwrap();
        terminal.process(&bytes);
        lines.pop();
        lines.extend((0..20).map(|i| RenderLine::plain(format!("NEXT_{i:02}"))));
        let stable = lines.len();
        lines.push(RenderLine::plain("user>"));
        bytes.clear();
        renderer
            .render(
                &mut bytes,
                &screen(lines),
                &publication(stable),
                stable,
                false,
                size,
            )
            .unwrap();
        terminal.process(&bytes);
        let mut rows = history(&mut terminal);
        rows.extend(terminal.screen().rows(0, 45));
        let text = rows
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        for index in 0..1200 {
            let marker = format!("ITEM_{index:04}");
            assert_eq!(text.matches(&marker).count(), 1, "{marker}");
        }
    }

    #[test]
    fn old_frame_group_boundary_survives_resize_before_shell_source_scrolls() {
        let mut renderer = ManagedRenderer::default();
        let old_size = TerminalSize::new(30, 6);
        let mut terminal = vt100::Parser::new(6, 30, 2000);
        let mut lines = (0..10)
            .map(|i| RenderLine::plain(format!("PRE_{i:02} {} END_{i:02}", "a".repeat(35))))
            .collect::<Vec<_>>();
        lines.push(RenderLine::plain("command>"));
        let mut bytes = Vec::new();
        renderer
            .render(
                &mut bytes,
                &screen(lines.clone()),
                &publication(11),
                11,
                false,
                old_size,
            )
            .unwrap();
        terminal.process(&bytes);
        terminal.screen_mut().set_scrollback(2000);
        let before_shell = terminal.screen().scrollback();
        terminal.screen_mut().set_scrollback(0);
        terminal.process(b"\r\n\r\n");
        terminal.screen_mut().set_scrollback(2000);
        let scrolled = terminal.screen().scrollback() - before_shell;
        terminal.screen_mut().set_scrollback(0);
        terminal.screen_mut().set_size(6, 90);
        let size = TerminalSize::new(90, 6);
        lines.push(RenderLine::plain(""));
        lines.push(RenderLine::plain("user>"));
        let scene = screen(lines.clone());
        bytes.clear();
        renderer
            .adopt_external_output(
                &mut bytes,
                &scene,
                &publication(12),
                size,
                ExternalPublication {
                    scrolled,
                    cleared: false,
                    source: None,
                },
            )
            .unwrap();
        terminal.process(&bytes);
        lines.pop();
        lines.extend((0..20).map(|i| RenderLine::plain(format!("NEXT_{i:02}"))));
        let stable = lines.len();
        lines.push(RenderLine::plain("user>"));
        bytes.clear();
        renderer
            .render(
                &mut bytes,
                &screen(lines),
                &publication(stable),
                stable,
                false,
                size,
            )
            .unwrap();
        terminal.process(&bytes);
        let mut rows = history(&mut terminal);
        rows.extend(terminal.screen().rows(0, 90));
        let rendered = rows.join("\n");
        for i in 0..10 {
            for prefix in ["PRE", "END"] {
                let marker = format!("{prefix}_{i:02}");
                assert_eq!(
                    rendered.matches(&marker).count(),
                    1,
                    "{marker}:\n{rendered}"
                );
            }
        }
    }

    #[test]
    fn large_stable_history_publishes_in_bounded_frames_without_missing_rows() {
        let mut renderer = ManagedRenderer::default();
        let size = TerminalSize::new(30, 8);
        let mut terminal = vt100::Parser::new(8, 30, 2000);
        let mut lines = (0..600)
            .map(|i| RenderLine::plain(format!("ROW_{i:04}")))
            .collect::<Vec<_>>();
        lines.push(RenderLine::plain("user> "));
        let screen = screen(lines);
        let publication = publication(600);
        let mut previous_scroll = 0;
        for frame in 0..10 {
            let mut bytes = Vec::new();
            renderer
                .render(&mut bytes, &screen, &publication, 600, false, size)
                .unwrap();
            terminal.process(&bytes);
            terminal.screen_mut().set_scrollback(2000);
            let scrolled = terminal.screen().scrollback();
            assert!(scrolled - previous_scroll <= PUBLICATION_ROWS_PER_FRAME);
            previous_scroll = scrolled;
            terminal.screen_mut().set_scrollback(0);
            assert!(terminal.screen().contents().contains("user>"));
            if !renderer.publication_pending() {
                break;
            }
            assert!(frame < 9, "publication did not converge");
        }
        let mut rows = history(&mut terminal);
        rows.extend(terminal.screen().rows(0, 30));
        for i in 0..600 {
            let marker = format!("ROW_{i:04}");
            assert_eq!(
                rows.iter().filter(|row| row.trim() == marker).count(),
                1,
                "{marker}"
            );
        }
    }

    fn publication(count: usize) -> Vec<PublicationUnit> {
        (0..count)
            .map(|index| PublicationUnit {
                id: RowIdentity {
                    block: 0,
                    group: index,
                },
                end: index + 1,
                stable: true,
                origins: None,
            })
            .collect()
    }

    fn screen(lines: Vec<RenderLine>) -> VirtualScreen {
        let cursor = VirtualCursor {
            line: lines.len() - 1,
            char_offset: 0,
        };
        VirtualScreen::from_render_lines(lines, cursor, true)
    }

    fn history(parser: &mut vt100::Parser) -> Vec<String> {
        parser.screen_mut().set_scrollback(1000);
        let count = parser.screen().scrollback();
        let rows = parser.screen().size().0 as usize;
        let mut result = Vec::new();
        for offset in (1..=count).rev().step_by(rows) {
            parser.screen_mut().set_scrollback(offset);
            // Native rows keep the width at which they were printed. Reading
            // only today's width would hide the suffix of older wider rows.
            result.extend(parser.screen().rows(0, u16::MAX).take(rows.min(offset)));
        }
        parser.screen_mut().set_scrollback(0);
        result
    }

    #[test]
    fn mutable_preview_never_enters_native_history_and_stable_lines_publish_once() {
        let mut renderer = ManagedRenderer::default();
        let size = TerminalSize::new(30, 6);
        let mut parser = vt100::Parser::new(6, 30, 1000);
        let mut bytes = Vec::new();
        for count in [20, 8, 25, 3] {
            let mut lines = (0..count)
                .map(|i| RenderLine::plain(format!("DRAFT_{i}")))
                .collect::<Vec<_>>();
            lines.push(RenderLine::plain("user> "));
            renderer
                .render(&mut bytes, &screen(lines), &[], 0, false, size)
                .unwrap();
            parser.process(&bytes);
            bytes.clear();
            assert!(history(&mut parser).is_empty());
        }
        let mut lines = (0..18)
            .map(|i| RenderLine::plain(format!("FINAL_{i:02}")))
            .collect::<Vec<_>>();
        lines.push(RenderLine::plain("user> "));
        let final_screen = screen(lines);
        renderer
            .render(&mut bytes, &final_screen, &publication(18), 18, false, size)
            .unwrap();
        parser.process(&bytes);
        bytes.clear();
        renderer
            .render(&mut bytes, &final_screen, &publication(18), 18, false, size)
            .unwrap();
        parser.process(&bytes);
        let mut all = history(&mut parser);
        all.extend(parser.screen().rows(0, 30));
        for i in 0..18 {
            assert_eq!(
                all.iter()
                    .filter(|line| line.trim() == format!("FINAL_{i:02}"))
                    .count(),
                1,
                "{all:?}"
            );
        }
        assert!(!all.iter().any(|line| line.contains("DRAFT_")));
    }

    #[test]
    fn browsing_and_resize_do_not_republish_stable_prefix() {
        let mut renderer = ManagedRenderer::default();
        let mut lines = (0..30)
            .map(|i| RenderLine::plain(format!("line {i}")))
            .collect::<Vec<_>>();
        lines.push(RenderLine::plain("user>"));
        let screen = screen(lines);
        let mut bytes = Vec::new();
        renderer
            .render(
                &mut bytes,
                &screen,
                &publication(30),
                30,
                false,
                TerminalSize::new(30, 8),
            )
            .unwrap();
        let published = renderer.published;
        bytes.clear();
        renderer.scroll_back(10);
        renderer
            .render(
                &mut bytes,
                &screen,
                &publication(30),
                30,
                false,
                TerminalSize::new(20, 10),
            )
            .unwrap();
        assert_eq!(renderer.published, published);
        assert!(!bytes.windows(4).any(|window| window == b"\x1b[1S"));
        renderer.follow_output();
        renderer
            .render(
                &mut Vec::new(),
                &screen,
                &publication(30),
                30,
                false,
                TerminalSize::new(20, 10),
            )
            .unwrap();
        assert_eq!(renderer.published, published);
    }
}

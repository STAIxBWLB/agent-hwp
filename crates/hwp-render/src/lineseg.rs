//! Synthesize line layout (PARA_LINE_SEG) for authored documents originating from Markdown/HWPX.
//!
//! Hangul 5.1.x renders body text at zero height without line-layout caches,
//! producing black bars, blank content or corruption. One-line-per-paragraph synthesis
//! causes overlap and collapsed heights in multiline paragraphs and long table cells.
//! Measure character widths with font shaping (shape_range), wrap greedily at body width,
//! and generate one PARA_LINE_SEG per line. Body v_pos is page-relative; cell v_pos is cumulative.
//!
//! Accurate wrapping requires the same font as Hangul (HCR Batang).

use hwp_model::{Control, Document, LineSeg, Paragraph, Table};

use crate::fonts::FontStore;
use crate::issues::RenderIssueAccumulator;
use crate::layout::TAB_INTERVAL_PT;
use crate::shape::{InlineItem, glyph_source_sequences, shape_range, source_has_no_break_space};

/// Contexts excluded from the plain-body section-start spacing rule.
/// Hangul 12.30.0 saved a paragraph-anchored floating table host with no added space-before
/// (#570). Pictures, GSOs and unknown placement remain conservative exclusions, not measured claims.
pub(crate) fn hosts_floating_body_object(para: &Paragraph) -> bool {
    para.controls.iter().any(|control| match control {
        Control::Table(table) => table.placement.as_ref().is_none_or(|p| !p.treat_as_char),
        Control::Picture(picture) => !picture.treat_as_char,
        Control::Generic(generic) if generic.ctrl_id == *b"gso " => {
            crate::gso::parse_gso_box(&generic.data).is_none_or(|b| !b.treat_as_char())
        }
        _ => false,
    })
}

/// Fixed vertical table-block padding in HWPUNIT, added by genuine Hangul to the whole table.
///
/// Measurements of the genuine first-paragraph sample (5.1.1.0) and work_report.hwp (5.0.2.4)
/// both yield table advance minus summed row heights = 566 (2.0mm is 566.93 HWPUNIT).
/// It stays constant regardless of table top/bottom margins (141 each) or cell side margins,
/// so treat it as table-specific outer padding added once, separately from row heights.
const TABLE_BLOCK_PADDING: i32 = 566;

/// One page's body box in HWPUNIT: the paper less its top and bottom margins and its header and
/// footer bands. Both paginating paths read the sums from here, so the cache and the renderer
/// cannot drift on where the body ends (#423).
pub(crate) struct BodyBox {
    /// Paper top edge to body top edge: the top margin plus the header band.
    pub(crate) top_hu: i32,
    /// Bottom margin plus footer band: subtract this from the paper height to reach the body
    /// bottom.
    pub(crate) bottom_hu: i32,
    /// Body height, at least one HWPUNIT.
    pub(crate) height_hu: i32,
}

/// `paper_height_hu` is the paper extent the caller paginates on: the renderer passes the
/// landscape-resolved height, the body cache its page height.
pub(crate) fn body_box(page: &hwp_model::PageDef, paper_height_hu: i32) -> BodyBox {
    let top_hu = page.margin_top.0 + page.margin_header.0;
    let bottom_hu = page.margin_bottom.0 + page.margin_footer.0;
    BodyBox {
        top_hu,
        bottom_hu,
        height_hu: paper_height_hu
            .saturating_sub(top_hu)
            .saturating_sub(bottom_hu)
            .max(1),
    }
}

/// Synthesize line layout for the whole authored document, including body and table-cell paragraphs.
/// Native HWP5 sources keep the base cursor path, so native edits do not mix
/// cacheless fragment anchors/page-start flags with restored genuine caches. The base path
/// still restarts its cursor at an explicit page break, as genuine caches do.
/// To match Hangul's line breaks, store must have HCR Batang loaded.
///
/// A floating-table section reads its anchors from the prepass. If that pass did not finish
/// (a page or display-item budget stop, or a paragraph it never visited), the section is laid
/// out by the base cursor path instead and reports `anchor_prepass_incomplete`: default zero
/// anchors are never published as measured positions (#426).
pub fn synthesize_linesegs(
    doc: &mut Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
) {
    synthesize_linesegs_inner(doc, store, warnings, None, &BodyFlow::default(), None);
}

/// One section's body flow continues at a caller-supplied cursor instead of restarting at the
/// section top: an edited HWP5 document keeps the saved caches of the paragraphs before the edit,
/// so the re-synthesized suffix has to start where that prefix ends (#462).
#[derive(Clone, Copy, Debug)]
pub struct BodySeed {
    pub section: usize,
    /// Body paragraph index the cursor applies to.
    pub paragraph: usize,
    /// Page-relative cursor the paragraph's flow starts from, in HWPUNIT: the kept prefix's bottom.
    /// The paragraph's own space-before is added by the walk, as for any other paragraph.
    pub v_pos_hu: i32,
}

/// The cursor the walk below carries past one body paragraph, in HWPUNIT: the paragraph's entry
/// position plus the tables it anchors when it has any (text cannot follow a table at the line
/// cursor, which is what Hangul flags as corruption), otherwise `line_cursor`, the position
/// `compute_linesegs` left the caller at; plus the paragraph's space-after.
///
/// `line_cursor` is passed in rather than re-derived from the committed segs on purpose: the
/// serialized `line_spacing` is `max(advance - base, 0)`, so `v_pos + line_height + line_spacing`
/// reads back `max(base, advance)`. With FIXED spacing below the base line, or FIXED 0, that is not
/// the advance the synthesizer committed, and re-deriving it would change unseeded synthesis
/// (HWPX/markdown -> HWP) that this rule is not allowed to touch (#509).
fn body_cursor(entry_v_pos: i32, line_cursor: i32, table_total: i32, space_after: i32) -> i32 {
    let bottom = if table_total > 0 {
        entry_v_pos.saturating_add(table_total)
    } else {
        line_cursor
    };
    bottom.saturating_add(space_after)
}

/// The anchor line of a paragraph that holds nothing but one inline (treat-as-char) table, as
/// Hangul saves it (#644, rule B11): the line is as tall as the table object plus its outer top and
/// bottom margins, and keeps the paragraph's own line spacing, so the next paragraph starts at
/// `v_pos + line_height + line_spacing` plus the paragraph spacing. Measured over 315 genuine
/// table-only anchor paragraphs: the advance held in 315 of 315, the height in 300 (the rest are
/// large multi-row tables whose saved line is taller than the object; unexplained). Returns the
/// object block height, or `None` when the paragraph is not that shape.
pub(crate) fn inline_table_line_height(para: &Paragraph) -> Option<i32> {
    let mut tables = para.controls.iter().filter_map(|c| match c {
        Control::Table(t) => Some(t),
        _ => None,
    });
    let table = tables.next()?;
    if tables.next().is_some()
        || para.chars.iter().any(|c| match c {
            hwp_model::HwpChar::Text(_) => true,
            // A line break or tab makes the paragraph more than one table line, which
            // synthesis (one saved line) and layout must both leave to the ordinary path.
            hwp_model::HwpChar::CharCtrl(code) => *code == hwp_model::ctrl_char::LINE_BREAK,
            hwp_model::HwpChar::InlineCtrl { code, .. } => *code == hwp_model::ctrl_char::TAB,
            _ => false,
        })
        || para.controls.iter().any(|c| {
            !matches!(c, Control::Table(_) | Control::SectionDef(_))
                && !matches!(&c.ctrl_id(), b"cold" | b"pgnp" | b"nwno")
        })
    {
        return None;
    }
    let (height, top, bottom) = inline_table_object_extent(table)?;
    (height > 0).then(|| height.saturating_add(top).saturating_add(bottom))
}

/// The anchor line's spacing when the table's "affect line spacing" bit (object attr bit 2) is
/// set: Hangul then applies the paragraph's percentage line spacing to the table line's own
/// height rather than the font's (the Hangul re-save of the #620 T4 strip: block 2268 at 160%
/// saved `line_spacing` 1360 = 2268 * 60 / 100). With the bit clear, the anchor uses the
/// font-based spacing. The B11 corpus had 315 HWP5 and 451 HWPX inline tables with the bit
/// clear. Both inline tables in each public parity source have it set. `None` leaves the
/// synthesized spacing alone (bit clear, or not a percentage mode).
fn inline_table_affected_spacing(doc: &Document, para: &Paragraph, block: i32) -> Option<i32> {
    let table = para.controls.iter().find_map(|c| match c {
        Control::Table(t) => Some(t),
        _ => None,
    })?;
    let affects = match &table.placement {
        Some(placement) => placement.affect_line_spacing,
        None => table
            .common_data
            .get(0..4)
            .is_some_and(|b| u32::from_le_bytes(b.try_into().unwrap()) & 0b100 != 0),
    };
    if !affects {
        return None;
    }
    let kind = doc
        .header
        .para_shapes
        .get(para.para_shape.0 as usize)
        .map_or(0, |p| p.line_spacing_type);
    (kind == 0).then(|| {
        paragraph_advance(doc, para, block)
            .saturating_sub(block)
            .max(0)
    })
}

/// Height plus line spacing of a table-only inline anchor line (rule B11), as both the synthesized
/// cache and the cacheless layout cursor advance it. `None` when the paragraph is not that shape.
pub(crate) fn inline_table_line_advance(doc: &Document, para: &Paragraph) -> Option<i32> {
    let block = inline_table_line_height(para)?;
    let spacing = inline_table_affected_spacing(doc, para, block).unwrap_or_else(|| {
        let metrics = paragraph_metrics(doc, para);
        metrics.advance.saturating_sub(metrics.height).max(0)
    });
    Some(block.saturating_add(spacing))
}

/// `(height, outer top, outer bottom)` of an inline table's object box: the placement for
/// HWPX/authored tables, the object-common payload (HWP 5.0 §4.3.9 table 69: attr, vertical and
/// horizontal offset, width, height, z-order, outer margins left/right/top/bottom) for HWP5 ones.
fn inline_table_object_extent(table: &Table) -> Option<(i32, i32, i32)> {
    if let Some(placement) = &table.placement {
        if !placement.treat_as_char {
            return None;
        }
        let height = if placement.height > 0 {
            placement.height
        } else {
            // The HWP5 writer's grid rule (`table_grid_totals`): the per-row maximum over
            // every single-row cell, summed, so a merged column-0 cell cannot under-count.
            let mut rows = vec![0i32; usize::from(table.rows.max(1))];
            for c in table.cells.iter().filter(|c| c.row_span == 1) {
                if let Some(h) = rows.get_mut(usize::from(c.row)) {
                    *h = (*h).max(c.height.0);
                }
            }
            rows.iter().fold(0i32, |sum, h| sum.saturating_add(*h))
        };
        return Some((
            height,
            i32::from(placement.out_margins[2]),
            i32::from(placement.out_margins[3]),
        ));
    }
    let data = &table.common_data;
    let u32_at = |o: usize| {
        data.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let u16_at = |o: usize| {
        data.get(o..o + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    };
    if u32_at(0)? & 1 == 0 {
        return None;
    }
    Some((
        u32_at(16)? as i32,
        i32::from(u16_at(28)?),
        i32::from(u16_at(30)?),
    ))
}

/// [`body_cursor`] measured from a paragraph's saved record, for a caller continuing a flow from
/// kept caches: the saved first line stands for the entry position, and the tables are measured
/// from their saved nested caches, which is what the renderer draws for that kept paragraph.
///
/// A saved record carries no separate advance field, so the line cursor is reconstructed as
/// `v_pos + line_height + line_spacing` of the last line. That is the only reading a saved cache
/// offers, and it is exact for every mode but FIXED spacing below the base line, which a genuine
/// record cannot express without also losing the committed position (#509).
///
/// This is the body reading: a table-only inline anchor follows rule B11 (#644). Nested paragraph
/// lists use [`saved_list_cursor`], since B11 was measured on body anchors only (#410).
pub fn saved_body_cursor(paragraph: &Paragraph, space_after: i32) -> Option<i32> {
    saved_cursor(paragraph, space_after, true)
}

/// [`saved_body_cursor`] for a kept paragraph inside a nested list (a cell, caption or text
/// box): the pre-#644 reading, with no B11 line advance.
fn saved_list_cursor(paragraph: &Paragraph, space_after: i32) -> Option<i32> {
    saved_cursor(paragraph, space_after, false)
}

fn saved_cursor(paragraph: &Paragraph, space_after: i32, body: bool) -> Option<i32> {
    let entry = paragraph.line_segs.first()?;
    let table_total = paragraph
        .controls
        .iter()
        .filter_map(|control| match control {
            Control::Table(table) => Some(table_height(table)),
            _ => None,
        })
        .fold(0i32, |total, height| total.saturating_add(height));
    let last = paragraph.line_segs.last()?;
    let line_cursor = last
        .v_pos
        .saturating_add(last.line_height)
        .saturating_add(last.line_spacing);
    // A kept table anchor keeps the table inside its own line block: genuine records store the
    // extent in the anchor line's height, which can end below the summed cell heights (a 4412 HU
    // table under a 5100 HU line in the #462 fixture). Resuming at the smaller of the two would
    // start the next paragraph inside the line the output keeps (#509).
    let line_bottom = last.v_pos.saturating_add(last.line_height);
    // A table-only inline anchor (rule B11, #644) advances by its line height plus line spacing;
    // the measured block counts only when the cell grid outgrew the object (an edited cell),
    // exactly as the synthesizer decides it.
    if body && let Some(block) = inline_table_line_height(paragraph) {
        let grid = table_total.saturating_sub(TABLE_BLOCK_PADDING);
        let table_total = if grid > block { table_total } else { 0 };
        // Floor at the block: an anchor ahwp saved before #644 kept the font line height, and
        // the next paragraph must still clear its table. A no-op on genuine caches, whose line
        // is at least the block (315 of 315).
        return Some(body_cursor(
            entry.v_pos,
            line_cursor,
            table_total
                .max(block)
                .max(line_cursor.saturating_sub(entry.v_pos)),
            space_after,
        ));
    }
    Some(
        body_cursor(entry.v_pos, line_cursor, table_total, space_after)
            .max(line_bottom.saturating_add(space_after)),
    )
}

/// [`synthesize_linesegs`] with body-flow seeds, so an edited document can re-synthesize only the
/// paragraphs after an edit while keeping the genuine caches before it (#462). A seeded paragraph
/// starts at its seed; an explicit page break in that paragraph still restarts the page.
pub fn synthesize_linesegs_seeded(
    doc: &mut Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
    flow: &BodyFlow<'_>,
) {
    synthesize_linesegs_inner(doc, store, warnings, None, flow, None);
}

/// Immutable source match for one edited body paragraph. Moved/new paragraphs have no
/// same-position source cache and must establish a new flow (#540 G3).
#[derive(Clone, Copy, Debug)]
pub struct NativeBodyCache<'a> {
    pub original: Option<&'a Paragraph>,
    pub unchanged: bool,
    /// Position-independent unchanged source candidate, used only to recognise a floating host.
    /// This never permits a shifted paragraph to enter the #585 verbatim-keep path.
    pub floating_original: Option<&'a Paragraph>,
    /// First stored position of this source paragraph's successor, even if the edited successor
    /// is new or changed. This is a source offset, not a floating-table pagination formula (#590).
    pub successor_v_pos: Option<i32>,
}

fn native_floating_host<'a>(source: NativeBodyCache<'a>) -> Option<&'a Paragraph> {
    source
        .floating_original
        .or_else(|| source.original.filter(|_| source.unchanged))
        .filter(|p| {
            !p.line_segs.is_empty()
                && p.line_segs.iter().all(|line| line.seg_width == 0)
                && p.controls.iter().any(|c| match c {
                    Control::Table(t) => t.placement.as_ref().is_some_and(|p| !p.treat_as_char),
                    _ => false,
                })
        })
}

/// A host delta belongs to one source page/column. Any boundary or overflowing line makes
/// this whole paragraph use ordinary synthesis from the pending body cursor instead (#590 F1).
fn native_translation_fits(
    original: &Paragraph,
    delta: i32,
    mut previous_v: i32,
    content_h: i32,
) -> bool {
    original.line_segs.iter().all(|line| {
        let continues = line.flags & 3 == 0 && line.v_pos > previous_v;
        previous_v = line.v_pos;
        continues
            && i64::from(line.v_pos) + i64::from(delta) + i64::from(line.line_height)
                <= i64::from(content_h)
    })
}

fn translated_native_lines(original: &Paragraph, delta: i32) -> Vec<LineSeg> {
    original
        .line_segs
        .iter()
        .map(|line| LineSeg {
            v_pos: line.v_pos.saturating_add(delta),
            ..*line
        })
        .collect()
}

/// #590 ruling: carry the source successor offset, without guessing the table's page split.
/// The main loop adds the next paragraph's top spacing, so remove it from this cursor once.
fn native_floating_cursor(
    source: NativeBodyCache<'_>,
    delta: i32,
    next_top: i32,
    line_cursor: i32,
    space_after: i32,
) -> i32 {
    source.successor_v_pos.map_or_else(
        || line_cursor.saturating_add(space_after),
        |next| next.saturating_add(delta).saturating_sub(next_top),
    )
}

struct NativeBodyWalk<'a, 'b> {
    source: &'a [Vec<NativeBodyCache<'b>>],
    kept: Vec<(usize, usize)>,
}

/// Native edit synthesis with source-aware body preservation. Source metadata is in
/// section/paragraph order. Return the body indices whose complete source records the writer
/// should retain. A moved unchanged zero-width floating host instead translates source successor
/// caches until the next edit, source page/column boundary or body-height limit (#590); these
/// are not returned as verbatim records. Nested lists
/// keep the existing seam policy, and authored synthesis is unchanged.
pub fn synthesize_native_linesegs_seeded(
    doc: &mut Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
    flow: &BodyFlow<'_>,
    source: &[Vec<NativeBodyCache<'_>>],
) -> Vec<(usize, usize)> {
    let mut native = NativeBodyWalk {
        source,
        kept: Vec::new(),
    };
    synthesize_linesegs_inner(doc, store, warnings, None, flow, Some(&mut native));
    native.kept
}

fn body_cache_extent(lines: &[LineSeg]) -> Option<i64> {
    let first = lines.first()?;
    let last = lines.last()?;
    Some(
        i64::from(last.v_pos) + i64::from(last.line_height) + i64::from(last.line_spacing)
            - i64::from(first.v_pos),
    )
}

fn same_native_list_caches(original: &[Paragraph], current: &[Paragraph]) -> bool {
    original.len() == current.len()
        && original.iter().zip(current).all(|(a, b)| {
            a.line_segs == b.line_segs && same_native_control_extents(&a.controls, &b.controls)
        })
}

fn same_native_caption(a: Option<&hwp_model::Caption>, b: Option<&hwp_model::Caption>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.side == b.side
                && a.direction == b.direction
                && a.gap == b.gap
                && a.width == b.width
                && a.last_width == b.last_width
                && same_native_list_caches(&a.paragraphs, &b.paragraphs)
        }
        _ => false,
    }
}

// Unknown raw object/context changes cannot establish equal extent. Keep the prior reflow
// in those cases rather than interpreting opaque headers or a new geometry rule (#509).
fn same_native_control_extents(original: &[Control], current: &[Control]) -> bool {
    original.len() == current.len()
        && original.iter().zip(current).all(|(a, b)| match (a, b) {
            (Control::SectionDef(a), Control::SectionDef(b)) => a == b,
            (Control::Table(a), Control::Table(b)) => {
                a.common_data == b.common_data
                    && a.placement == b.placement
                    && a.table_tail == b.table_tail
                    && a.extras == b.extras
                    && a.attr == b.attr
                    && a.cell_spacing == b.cell_spacing
                    && table_height(a) == table_height(b)
                    && a.cells.len() == b.cells.len()
                    && a.cells.iter().zip(&b.cells).all(|(a, b)| {
                        a.width == b.width
                            && a.height == b.height
                            && a.margins == b.margins
                            && a.list_attr == b.list_attr
                            && a.header_tail == b.header_tail
                            && same_native_list_caches(&a.paragraphs, &b.paragraphs)
                    })
                    && same_native_caption(a.caption.as_ref(), b.caption.as_ref())
            }
            (Control::Picture(a), Control::Picture(b)) => {
                a.common_data == b.common_data
                    && a.extras == b.extras
                    && a.effects_raw == b.effects_raw
                    && a.width == b.width
                    && a.height == b.height
                    && a.treat_as_char == b.treat_as_char
                    && a.vert_offset == b.vert_offset
                    && a.horz_offset == b.horz_offset
                    && same_native_caption(a.caption.as_ref(), b.caption.as_ref())
            }
            (Control::Generic(a), Control::Generic(b)) => {
                a.ctrl_id == b.ctrl_id
                    && a.data == b.data
                    && a.raw_children == b.raw_children
                    && a.extras == b.extras
                    && a.equation == b.equation
                    && a.container_box == b.container_box
                    && a.gso_shapes.len() == b.gso_shapes.len()
                    && a.gso_shapes
                        .iter()
                        .zip(&b.gso_shapes)
                        .all(|(a, b)| a.w == b.w && a.h == b.h)
                    && a.paragraph_lists.len() == b.paragraph_lists.len()
                    && a.paragraph_lists
                        .iter()
                        .zip(&b.paragraph_lists)
                        .all(|(a, b)| {
                            a.header_data == b.header_data
                                && same_native_list_caches(&a.paragraphs, &b.paragraphs)
                        })
                    && same_native_caption(a.caption.as_ref(), b.caption.as_ref())
            }
            _ => false,
        })
}

// Match the existing page/column-aware forward relation, not absolute position equality.
// Source backsteps represent an existing implicit boundary; a new shifted line must not
// place the next same-page source cache inside its synthesized block (#540 G3).
fn native_body_suffix_fits(
    original: &Paragraph,
    next: Option<NativeBodyCache<'_>>,
    synthesized_bottom: i64,
) -> bool {
    let Some(next) = next else {
        return true;
    };
    if !next.unchanged {
        return true;
    }
    let Some(saved) = next.original else {
        return false;
    };
    let Some(first) = saved.line_segs.first() else {
        return false;
    };
    let source_boundary = saved.header.break_type & 0x0c != 0
        || first.flags & 3 != 0
        || first.v_pos == 0
        || original
            .line_segs
            .last()
            .is_some_and(|last| first.v_pos < last.v_pos);
    source_boundary || synthesized_bottom <= i64::from(first.v_pos)
}

fn native_body_height_neutral(original: &Paragraph, current: &Paragraph) -> bool {
    !original.line_segs.is_empty()
        && (original.header.break_type & 0x0c) == (current.header.break_type & 0x0c)
        && original.line_segs.len() == current.line_segs.len()
        && body_cache_extent(&original.line_segs) == body_cache_extent(&current.line_segs)
        // PARA_LINE_SEG page/column-start bits, as used by layout.rs (#540 G3).
        && original.line_segs.iter().zip(&current.line_segs).all(|(a,b)| a.flags & 3 == b.flags & 3)
        && same_native_control_extents(&original.controls, &current.controls)
}

/// How an edited document's body flow is synthesized (#462, #509).
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyFlow<'a> {
    /// One seed per section, each taken from [`saved_body_cursor`] over the paragraphs before the
    /// edit that the output keeps.
    pub seeds: &'a [BodySeed],
    /// The document already carries the caches the output will retain (their nested cells,
    /// captions and lists restored from the saved record), so a nested list that has one is
    /// measured as it is rather than re-synthesized. That is what keeps a kept table's height the
    /// one the paragraphs below it were placed against (#509).
    pub preserve_nested: bool,
}

/// [`synthesize_linesegs`] with an explicit prepass page budget, so a test can prove how an
/// interrupted prepass is reported and where layout falls back to. Production passes no limit.
/// Returns the prepass result the section policies were decided from.
#[cfg(test)]
fn synthesize_linesegs_with_prepass_page_limit(
    doc: &mut Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
    page_limit: usize,
) -> Option<crate::layout::BodyAnchors> {
    synthesize_linesegs_inner(
        doc,
        store,
        warnings,
        Some(page_limit),
        &BodyFlow::default(),
        None,
    )
}

fn synthesize_linesegs_inner(
    doc: &mut Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
    prepass_page_limit: Option<usize>,
    flow: &BodyFlow<'_>,
    mut native: Option<&mut NativeBodyWalk<'_, '_>>,
) -> Option<crate::layout::BodyAnchors> {
    let snap = doc.clone();
    let seeds = flow.seeds;
    let nested_caches = if flow.preserve_nested {
        ListCaches::KeepMeasured
    } else {
        ListCaches::Recompute
    };
    let use_fragment_flow = doc.meta.source_format != "hwp5";
    let floating_sections: Vec<bool> = doc
        .sections
        .iter()
        .map(|section| {
            section
                .paragraphs
                .iter()
                .flat_map(|p| &p.controls)
                .any(|c| {
                    matches!(c,
            Control::Table(t) if t.placement.as_ref().is_some_and(|p| !p.treat_as_char))
                })
        })
        .collect();
    // Floating-table sections reuse the renderer's legal row/cell fragment flow.
    // Other sections keep their measured Hancom inline-table advance.
    let flow_anchors = if use_fragment_flow && floating_sections.iter().any(|floating| *floating) {
        for (si, section) in doc.sections.iter_mut().enumerate() {
            if !floating_sections[si] {
                continue;
            }
            let width = section
                .section_def()
                .and_then(|d| d.page.as_ref())
                .map_or(42520, |pg| {
                    pg.width.0 - pg.margin_left.0 - pg.margin_right.0
                });
            for para in &mut section.paragraphs {
                fill_nested(para, width, &snap, store, warnings, nested_caches);
            }
        }
        let mut flow = doc.clone();
        for section in &mut flow.sections {
            for para in &mut section.paragraphs {
                para.line_segs.clear();
            }
        }
        Some(match prepass_page_limit {
            Some(limit) => crate::layout::synthesized_body_anchors_with_page_limit(
                &flow, store, warnings, limit,
            ),
            None => crate::layout::synthesized_body_anchors(&flow, store, warnings),
        })
    } else {
        None
    };
    for (si, floating) in floating_sections.iter().copied().enumerate() {
        // An incomplete prepass must not publish its default entries as measured anchors:
        // that section keeps the base cursor path and reports the fallback once.
        let section_anchors = flow_anchors
            .as_ref()
            .filter(|prepass| floating && prepass.section_complete(si));
        if floating && flow_anchors.is_some() && section_anchors.is_none() {
            warnings.push(
                crate::issues::RenderIssueCode::AnchorPrepassIncomplete,
                format!("section={si}"),
            );
        }
        let page = snap.sections[si]
            .section_def()
            .and_then(|d| d.page.as_ref());
        let body_width = page.map_or(42520, |pg| {
            pg.width
                .0
                .saturating_sub(pg.margin_left.0)
                .saturating_sub(pg.margin_right.0)
        });
        // B13: the measured paragraph geometry is for a portrait, single-column body with
        // zero gutter. Keep nested lists and other page geometries on their existing paths.
        let measured_body_width = page
            .filter(|pg| pg.attr & 1 == 0 && pg.gutter.0 == 0)
            .filter(|_| {
                !snap.sections[si]
                    .paragraphs
                    .iter()
                    .flat_map(|p| &p.controls)
                    .any(|control| {
                        matches!(control, Control::Generic(g)
                        if g.column_def.as_ref().is_some_and(|columns| columns.count != 1))
                    })
            })
            .map(|_| body_width.div_euclid(4) * 4);
        // Body height excludes the top/bottom margins and, as of #423, the header/footer bands:
        // the same box `layout_document_pages` draws in. Genuine multipage caches are
        // page-relative: body vertpos resets on every page, and its maximum 59668 is below body
        // height. Without resetting, accumulated v_pos can greatly exceed page height (such as
        // 354408), which Hangul flags as corruption.
        let content_h = page
            .map_or(75686, |pg| body_box(pg, pg.height.0).height_hu)
            .max(1);
        let mut v_pos = 0i32;
        let native_mode = native.is_some() && !use_fragment_flow;
        let mut keep_body_suffix = native_mode;
        // After an unchanged native floating host moves, carry source successor positions on
        // the same source page/column until an edit or capacity boundary. These translated
        // caches are not whole-record verbatim keeps (#590).
        let mut floating_delta = None;
        for pi in 0..doc.sections[si].paragraphs.len() {
            // #540 G1 / #584: serialized body spacing is twice its HWPUNIT value on every
            // source path. Nested cache/list rules are independent and stay unchanged.
            let (sp_top, sp_bottom) = snap
                .header
                .para_shapes
                .get(snap.sections[si].paragraphs[pi].para_shape.0 as usize)
                .map_or((0, 0), |ps| (ps.spacing_top / 2, ps.spacing_bottom / 2));
            let source = native
                .as_ref()
                .and_then(|n| n.source.get(si))
                .and_then(|section| section.get(pi))
                .copied();
            let unchanged = source.is_some_and(|s| s.unchanged && s.original.is_some());
            if keep_body_suffix && unchanged {
                let original = source
                    .and_then(|s| s.original)
                    .expect("matched source body paragraph");
                if !original.line_segs.is_empty() {
                    doc.sections[si].paragraphs[pi].line_segs = original.line_segs.clone();
                    v_pos = saved_body_cursor(&doc.sections[si].paragraphs[pi], sp_bottom)
                        .expect("nonempty source cache");
                    native.as_mut().expect("native mode").kept.push((si, pi));
                    continue;
                }
                // A cache-less source record still stays cache-less in the output. Measure its
                // ephemeral flow below, exactly as the existing cache-less-prefix fallback did.
            }
            let next_top = snap.sections[si]
                .paragraphs
                .get(pi + 1)
                .and_then(|p| snap.header.para_shapes.get(p.para_shape.0 as usize))
                .map_or(0, |shape| shape.spacing_top / 2);
            if let Some((delta, previous_v)) = floating_delta {
                if let Some(source) = source.filter(|s| s.unchanged)
                    && let Some(original) = source.original.filter(|p| !p.line_segs.is_empty())
                    && native_translation_fits(original, delta, previous_v, content_h)
                {
                    let current = &mut doc.sections[si].paragraphs[pi];
                    current.line_segs = translated_native_lines(original, delta);
                    let last = current.line_segs.last().expect("nonempty source cache");
                    let line_cursor = last
                        .v_pos
                        .saturating_add(last.line_height)
                        .saturating_add(last.line_spacing);
                    v_pos = if native_floating_host(source).is_some() {
                        native_floating_cursor(source, delta, next_top, line_cursor, sp_bottom)
                    } else {
                        saved_body_cursor(current, sp_bottom).expect("nonempty translated cache")
                    };
                    floating_delta = Some((
                        delta,
                        original.line_segs.last().expect("source cache").v_pos,
                    ));
                    continue;
                }
                // A changed/new neighbour, source boundary or overflowing translated line
                // resumes ordinary synthesis. Later unchanged records cannot re-enable this run.
                floating_delta = None;
            }
            let explicit_break = snap.sections[si].paragraphs[pi].header.break_type & 0x04 != 0;
            // A seeded paragraph continues the flow a caller measured before it, so an edited
            // document's re-synthesized suffix begins where its kept prefix ends (#462) instead
            // of at a section-top cursor that contradicts the kept caches. An explicit page
            // break still wins: it restarts the page regardless of the seed. The seed comes from
            // a saved record, so the arithmetic saturates: an out-of-page cursor must degrade to
            // a page start, never to a panic or a release-mode wrap (#509).
            if let Some(seed) = seeds
                .iter()
                .find(|seed| seed.section == si && seed.paragraph == pi)
                && (0..=content_h).contains(&seed.v_pos_hu)
            {
                v_pos = seed.v_pos_hu;
            }
            if explicit_break {
                v_pos = 0;
            }
            if pi > 0 || !hosts_floating_body_object(&snap.sections[si].paragraphs[pi]) {
                v_pos = v_pos.saturating_add(sp_top);
            }
            // Floating-table measurement already populated nested caches.
            if section_anchors.is_none() {
                fill_nested(
                    &mut doc.sections[si].paragraphs[pi],
                    body_width,
                    &snap,
                    store,
                    warnings,
                    nested_caches,
                );
            }
            // Total table height in this paragraph.
            let mut table_total = 0i32;
            for ctrl in &doc.sections[si].paragraphs[pi].controls {
                if let Control::Table(t) = ctrl {
                    table_total = table_total.saturating_add(table_height(t));
                }
            }
            let floating_source = source.filter(|s| {
                native_mode
                    && (!keep_body_suffix || s.original.is_none())
                    && native_floating_host(*s).is_some()
            });
            // Native floating hosts stay in body flow; table pagination does not reset their
            // anchor or consume the entire table as a body-line advance (#590 saved bytes).
            if floating_source.is_some() {
                table_total = 0;
            }
            // Rule B11 (#644): a table-only inline anchor's saved line is the object block, which
            // can be taller than the measured grid (fixed rows, a stored object height). Rule A
            // fits that line height at the body bottom, so test the larger of the two.
            let inline_fit = floating_source
                .is_none()
                .then(|| inline_table_line_height(&doc.sections[si].paragraphs[pi]))
                .flatten();
            let fit_total = inline_fit.map_or(table_total, |block| table_total.max(block));
            // Other table blocks retain the existing whole-table page-fit fallback.
            if section_anchors.is_none()
                && fit_total > 0
                && v_pos > 0
                && v_pos.saturating_add(fit_total) > content_h
            {
                v_pos = 0;
            }
            // A table anchor's cache starts at the incoming cursor after the preceding paragraph.
            // Genuine first-paragraph sample: body advance 1600, then table-anchor v_pos=1600.
            let anchor = section_anchors.and_then(|prepass| prepass.slot(si, pi));
            if let Some(anchor) = anchor {
                v_pos = anchor.v_pos;
            }
            let anchor_v = v_pos;
            let src = &snap.sections[si].paragraphs[pi];
            let mut segs = if let Some(source) = floating_source {
                let original = native_floating_host(source).expect("matched floating host");
                let delta = anchor_v.saturating_sub(original.line_segs[0].v_pos);
                let lines = translated_native_lines(original, delta);
                let last = lines.last().expect("nonempty source floating cache");
                v_pos = last
                    .v_pos
                    .saturating_add(last.line_height)
                    .saturating_add(last.line_spacing);
                floating_delta = Some((
                    delta,
                    original.line_segs.last().expect("source cache").v_pos,
                ));
                lines
            } else {
                let line_width = measured_body_width.map_or(body_width, |width| {
                    let Some(shape) = snap.header.para_shapes.get(src.para_shape.0 as usize) else {
                        return width;
                    };
                    // Paragraph shape margins are stored in twice HWPUNIT.
                    width
                        .saturating_sub(shape.margin_left.max(0) / 2)
                        .saturating_sub(shape.margin_right.max(0) / 2)
                });
                compute_linesegs(
                    store, &snap, src, line_width, content_h, &mut v_pos, warnings,
                )
            };
            // Capture the line cursor before applying table/source-offset flow. Ordinary
            // compute_linesegs may advance less than its serialized cache can express: FIXED
            // spacing below the base (or 0) must not gain the cache's base-height floor (#509).
            // A translated floating host instead uses the stored line advance above (#590).
            let mut line_cursor = v_pos;
            // Rule B11 (#644): a paragraph holding only an inline table saves one line as tall as
            // the table object block, keeping the paragraph's line spacing, and the next paragraph
            // starts after that line's height and spacing (315 of 315 genuine anchors). Without
            // this the line kept the font height and the flow advanced by the table height alone,
            // so every following line sat one line spacing (and the object/cell-grid difference)
            // higher than in Hangul.
            let inline_block = (floating_source.is_none() && segs.len() == 1)
                .then(|| inline_table_line_height(src))
                .flatten();
            if let Some(block) = inline_block
                && let Some(line) = segs.first_mut()
            {
                line.line_height = block;
                line.text_height = block;
                // Baseline at 85% of the line, rounded half up (`paragraph_metrics` truncates).
                // Evidence: the #620 T4 strip and TOC anchors and the #644 inline-table sample
                // matched Hangul's re-saved caches byte-for-byte, text height and baseline included.
                line.baseline_gap = ((i64::from(block) * 85 + 50) / 100) as i32;
                if let Some(advance) = inline_table_line_advance(&snap, src) {
                    line.line_spacing = advance.saturating_sub(block);
                }
                line_cursor = line
                    .v_pos
                    .saturating_add(block)
                    .saturating_add(line.line_spacing);
                // The line advance replaces the measured block (whose 566 padding the anchor line
                // does not carry), except when the cell grid itself outgrew the object (an edited
                // cell): then the flow still clears the grown table, as `saved_body_cursor` does.
                let grid = table_total.saturating_sub(TABLE_BLOCK_PADDING);
                table_total = if grid > block {
                    table_total.max(line_cursor.saturating_sub(anchor_v))
                } else {
                    0
                };
            }
            if anchor.is_some_and(|anchor| anchor.starts_page)
                && let Some(first) = segs.first_mut()
            {
                // HWP 5.0 §4.3.4 (PARA_LINE_SEG): bit0 marks the first line of a page.
                first.flags |= 0x1;
            }
            // A table paragraph advances by table height rather than one line_advance, keeping
            // following text from overlapping the table (which Hangul flags as corruption), and
            // space-after moves the next paragraph's first line down. `body_cursor` holds both
            // readings, so measuring a kept prefix from its saved record cannot drift from this.
            v_pos = if let Some(source) = floating_source {
                native_floating_cursor(
                    source,
                    floating_delta.expect("translated host").0,
                    next_top,
                    line_cursor,
                    sp_bottom,
                )
            } else {
                body_cursor(anchor_v, line_cursor, table_total, sp_bottom)
            };
            doc.sections[si].paragraphs[pi].line_segs = segs;
            if keep_body_suffix {
                if unchanged {
                    native.as_mut().expect("native mode").kept.push((si, pi));
                } else {
                    let next = native
                        .as_ref()
                        .and_then(|n| n.source.get(si))
                        .and_then(|section| section.get(pi + 1))
                        .copied();
                    let current = &doc.sections[si].paragraphs[pi];
                    let cache_bottom = current.line_segs.last().map_or(i64::MAX, |line| {
                        i64::from(line.v_pos)
                            + i64::from(line.line_height)
                            + i64::from(line.line_spacing)
                    });
                    // Exclude paragraph space-after: the source cache already owns that gap.
                    // Include a hosted table's full measured block, not only its anchor line.
                    let bottom = cache_bottom.max(i64::from(body_cursor(
                        anchor_v,
                        line_cursor,
                        table_total,
                        0,
                    )));
                    keep_body_suffix = source.and_then(|s| s.original).is_some_and(|original| {
                        native_body_height_neutral(original, current)
                            && native_body_suffix_fits(original, next, bottom)
                    });
                }
            }
        }
    }
    flow_anchors
}

/// What a nested paragraph list does with the line caches it already carries (#509). The three
/// readings are separate switches: which paragraphs keep their own cache, what a descendant table
/// cell does, and whether a kept cache moves the list cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListCaches {
    /// Re-synthesize every paragraph. Table cells default to this because their line counts decide
    /// the table height, and a stale pre-edit cache would misalign the table.
    Recompute,
    /// Keep a paragraph's own cache and leave the list cursor where it was, which is the unedited
    /// convention for captions and generic paragraph lists. A kept cache does not move the cursor
    /// here, so unseeded synthesis publishes the same positions it always did (#509).
    KeepInPlace,
    /// Keep the cached prefix of the list, advancing the cursor by each kept paragraph's block, and
    /// re-synthesize from the first paragraph without a cache (#509). Only an edited document whose
    /// retained caches were restored first uses this: the prefix is what the output keeps, so it is
    /// what the table height must be measured from, and a cache after the seam is stale because an
    /// earlier paragraph in the same list moved.
    KeepMeasured,
}

impl ListCaches {
    /// What a caption or a generic control's paragraph list nested under this policy does: both are
    /// text containers the base synthesis keeps in place.
    fn text_list(self) -> Self {
        match self {
            Self::KeepMeasured => Self::KeepMeasured,
            Self::Recompute | Self::KeepInPlace => Self::KeepInPlace,
        }
    }

    /// What a table cell list nested under this policy does.
    fn cell(self) -> Self {
        match self {
            Self::KeepMeasured => Self::KeepMeasured,
            Self::Recompute | Self::KeepInPlace => Self::Recompute,
        }
    }
}

/// Native-cache rule (#540 G2, #576): only 0xffff inherits the same table margin side.
/// Explicit zero stays zero. Horizontal inheritance is observed; vertical inheritance is
/// inferred. Work on copied arrays so raw IR margins remain available for identity writes.
fn native_cache_cell_margins(margins: [u16; 4], inner_margins: [u16; 4]) -> [u16; 4] {
    std::array::from_fn(|side| {
        if margins[side] == u16::MAX {
            inner_margins[side]
        } else {
            margins[side]
        }
    })
}

/// Native list widths measured from the hwp-swift Hancom fixtures. The text-box
/// LIST_HEADER stores width at 16 and left/right margins at 8/10. The inner
/// width is rounded down to a multiple of four HWPUNIT: 22936 - 283 - 283
/// -> 22368 in text-box and 17925 - 283 - 283 -> 17356 in work_report.hwp.
/// The header/footer width at 8 is 42520; notes use the same section
/// text width and their 16-byte list header has no width. HWP 5.0 §4.3.4 defines
/// the nine PARA_LINE_SEG fields in order. First lines in these saved lists:
/// (0, 0, 1000, 1000, 850, 600, 0, 22368, 0x60000) for a text box,
/// (0, 0, 900, 900, 765, 452, 0, 42520, 0x60000) for header/footer, and
/// (0, 0, 900, 900, 765, 272, 0, 42520, 0x60000) for footnote/endnote.
/// The second text-box paragraph starts at 1600. Cache synthesis reuses the
/// existing box line walker. The saved bytes establish width at offset 28 and
/// flags at offset 32.
/// Header/footer and note spacing uses the generic fallback (450 and 270 in
/// these examples) pending the Hancom Office layout check.
fn saved_generic_list_width(ctrl_id: [u8; 4], header: &[u8], body_width: i32) -> Option<i32> {
    let read_width = |offset| {
        Some(i32::from_le_bytes(
            header.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let width = match &ctrl_id {
        b"gso " => {
            let left = i32::from(u16::from_le_bytes(header.get(8..10)?.try_into().ok()?));
            let right = i32::from(u16::from_le_bytes(header.get(10..12)?.try_into().ok()?));
            // The rounding fits the two saved rectangle widths above.
            read_width(16)?.checked_sub(left + right)? & !3
        }
        b"head" | b"foot" => read_width(8)?,
        b"fn  " | b"en  " => body_width,
        _ => return None,
    };
    (width > 0).then_some(width)
}

/// Synthesize every nested paragraph list: table captions/cells, controls within cells
/// (including nested tables), picture captions, object paragraph lists and object captions.
/// This matches the lists where hwp-convert walk_para_lists can insert paragraphs.
/// Leaving an inserted paragraph cacheless makes Hangul draw it at zero height (B2);
/// a final list paragraph also contradicts bit31 with an empty PARA_LINE_SEG (B3).
///
/// snap is a separate clone of doc, so doc can be borrowed freely here without conflict
/// while looking up paragraph and character properties.
fn fill_nested(
    para: &mut Paragraph,
    body_width: i32,
    snap: &Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
    caches: ListCaches,
) {
    for ctrl in &mut para.controls {
        match ctrl {
            Control::Table(t) => {
                if let Some(cap) = &mut t.caption {
                    let w = caption_width(cap.width, body_width);
                    fill_list(
                        &mut cap.paragraphs,
                        w,
                        caches.text_list(),
                        body_width,
                        snap,
                        store,
                        warnings,
                    );
                }
                for cell in &mut t.cells {
                    let margins = native_cache_cell_margins(cell.margins, t.inner_margins);
                    let w = (cell.width.0 - i32::from(margins[0]) - i32::from(margins[1])).max(1);
                    fill_list(
                        &mut cell.paragraphs,
                        w,
                        caches.cell(),
                        body_width,
                        snap,
                        store,
                        warnings,
                    );
                }
            }
            Control::Picture(pic) => {
                if let Some(cap) = &mut pic.caption {
                    let w = caption_width(cap.width, body_width);
                    fill_list(
                        &mut cap.paragraphs,
                        w,
                        caches.text_list(),
                        body_width,
                        snap,
                        store,
                        warnings,
                    );
                }
            }
            Control::Generic(g) => {
                let fallback_width = g
                    .gso_shapes
                    .first()
                    .map_or(body_width, |s| (s.w - 566).max(1));
                for list in &mut g.paragraph_lists {
                    let width = if g.raw_children.is_empty() {
                        fallback_width
                    } else {
                        saved_generic_list_width(g.ctrl_id, &list.header_data, body_width)
                            .unwrap_or(fallback_width)
                    };
                    fill_list(
                        &mut list.paragraphs,
                        width,
                        caches.text_list(),
                        body_width,
                        snap,
                        store,
                        warnings,
                    );
                }
                if let Some(cap) = &mut g.caption {
                    let w = caption_width(cap.width, body_width);
                    fill_list(
                        &mut cap.paragraphs,
                        w,
                        caches.text_list(),
                        body_width,
                        snap,
                        store,
                        warnings,
                    );
                }
            }
            Control::SectionDef(_) => {}
        }
    }
}

/// Caption text width. None means extending into margins, so use body width.
fn caption_width(width: Option<u32>, body_width: i32) -> i32 {
    width
        .and_then(|w| i32::try_from(w).ok())
        .filter(|w| *w > 0)
        .unwrap_or(body_width)
}

/// Fill one paragraph list. Accumulate v_pos per list rather than resetting each paragraph;
/// table_height reads the last paragraph's v_pos as the whole cell's height on that basis.
/// [`ListCaches::KeepInPlace`] keeps a paragraph's own cache without moving the cursor, and
/// [`ListCaches::KeepMeasured`] keeps the cached prefix while advancing by it and re-synthesizes
/// from the first paragraph without a cache.
fn fill_list(
    paras: &mut [Paragraph],
    width: i32,
    caches: ListCaches,
    body_width: i32,
    snap: &Document,
    store: &mut FontStore,
    warnings: &mut RenderIssueAccumulator,
) {
    let mut v = 0i32;
    // Set once KeepMeasured re-synthesized a paragraph: an earlier paragraph in this list moved, so
    // every later cache was measured against a position that no longer holds (#509).
    let mut past_seam = false;
    for para in paras.iter_mut() {
        fill_nested(para, body_width, snap, store, warnings, caches);
        let keep = match caches {
            ListCaches::Recompute => false,
            ListCaches::KeepInPlace => !para.line_segs.is_empty(),
            ListCaches::KeepMeasured => !past_seam && !para.line_segs.is_empty(),
        };
        if keep {
            if caches == ListCaches::KeepMeasured {
                // The retained prefix moves the cursor: the paragraph after it has to start below
                // it, and that is the height the table is measured from.
                if let Some(bottom) = saved_list_cursor(para, 0) {
                    v = bottom;
                }
            }
            continue;
        }
        past_seam = true;
        // Do not paginate inside lists (content_h=MAX); synthesize_linesegs handles overflow
        // at the table/object level.
        let segs = compute_box_linesegs(store, snap, para, width, &mut v, warnings);
        para.line_segs = segs;
    }
}

/// Compute one table's vertical height in HWPUNIT using measured genuine Hangul rules.
///
/// fill_nested must populate cell paragraph line_segs before this function runs.
/// Formula derived from genuine first-paragraph and work_report.hwp measurements:
///
/// ```text
/// Row height rowH = cell.top_margin + cell.bottom_margin + line block.
/// Line block of N lines = last line.v_pos + last line.line_height.
/// With uniform line metrics: (N-1)*line_advance + line_height.
/// Table height = sum over rows of max(rowH among that row's cells) + TABLE_BLOCK_PADDING(566).
/// ```
///
/// Evidence: first-paragraph sample (3 rows, one line/cell, margins top141/bottom141, lh=1000, la=1600)
/// gives 3*(141+1000+141)+566 = 4412, the genuine advance (6012-1600). work_report.hwp's
/// first table (one row, two columns, two lines in one cell) gives (141+(3200+2000)+141)+566
/// = 5482+566 = 6048, also matching genuine advance. Both yield the same 566 (2.0mm) constant.
///
/// Skip row_span>1 cells rather than assigning their full height to their starting row.
/// Derive row heights from row_span==1 cells, matching genuine single-row height measurements.
/// If a row has no single-row cells because all are merged, use a safe fallback height.
fn table_height(table: &Table) -> i32 {
    // Maximum cell height per row (rowH), indexed by Cell.row.
    let mut row_heights = vec![0i32; usize::from(table.rows)];
    for cell in &table.cells {
        // Do not force a merged cell's height onto its starting row; use the fallback below.
        if cell.row_span != 1 {
            continue;
        }
        let r = usize::from(cell.row);
        if r >= row_heights.len() {
            continue;
        }
        // The cell line block is that of its last paragraph alone. fill_nested accumulates
        // v_pos across the cell rather than resetting each paragraph, so the final line's
        // v_pos + line_height is already the whole cell height. Summing paragraph heights
        // would count earlier paragraphs twice because their heights are included in later v_pos.
        let block = cell.paragraphs.last().map_or(0, para_line_block);
        let margins = native_cache_cell_margins(cell.margins, table.inner_margins);
        let cell_h = i32::from(margins[2])
            .saturating_add(block)
            .saturating_add(i32::from(margins[3]));
        if cell_h > row_heights[r] {
            row_heights[r] = cell_h;
        }
    }
    // Rows containing only row_span>1 cells have zero height; use one fallback line to prevent overlap.
    let fallback = 141 + 1000 + 141; // Top margin + one line (lh) + bottom margin, using genuine defaults.
    let sum = row_heights.iter().fold(0i32, |total, &h| {
        total.saturating_add(if h > 0 { h } else { fallback })
    });
    sum.saturating_add(TABLE_BLOCK_PADDING)
}

/// One cell paragraph's line-block height (HWPUNIT): last line.v_pos + last line.line_height.
/// Cell-local v_pos accumulates across paragraphs in fill_nested. The uniform-line
/// shortcut (line count - 1) * line_advance applies only when all line metrics match;
/// mixed sizes use the actual final v_pos plus line height.
fn para_line_block(para: &Paragraph) -> i32 {
    match para.line_segs.last() {
        // Saved caches are untrusted input: saturate so a corrupt position cannot wrap or panic.
        Some(seg) => seg.v_pos.saturating_add(seg.line_height),
        // Without line layout (theoretically impossible), fall back to one default line.
        None => 1000,
    }
}

fn line_advance_hu(base: i32, kind: u8, value: i32) -> i32 {
    let base = i64::from(base.max(0));
    let value = i64::from(value.max(0));
    let advance = match kind {
        // FIXED is exact and may intentionally overlap adjacent lines.
        1 => value / 2,
        // BETWEEN_LINES adds the serialized margin to the natural line height.
        2 => base + value / 2,
        // AT_LEAST cannot be smaller than the natural line height.
        3 => (value / 2).max(base),
        // PERCENT defaults to Hancom's observed 160% when unspecified.
        _ if value > 0 => base * value / 100,
        _ => base * 160 / 100,
    };
    advance.clamp(0, i64::from(i32::MAX)) as i32
}

fn source_wchar_len(source: &str) -> u32 {
    source.chars().fold(0u32, |length, ch| {
        length.saturating_add(ch.len_utf16() as u32)
    })
}

/// Shared fallback and cache metrics, in HWPUNIT.
pub(crate) struct ParagraphMetrics {
    pub height: i32,
    pub advance: i32,
    pub baseline_gap: i32,
}

pub(crate) fn paragraph_metrics(doc: &Document, para: &Paragraph) -> ParagraphMetrics {
    let base = para
        .char_shape_runs
        .first()
        .and_then(|(_, id)| doc.header.char_shapes.get(id.0 as usize))
        .map_or(
            1000,
            |cs| if cs.base_size > 0 { cs.base_size } else { 1000 },
        );
    let advance = paragraph_advance(doc, para, base);
    let baseline_gap = (i64::from(base) * 85 / 100).clamp(0, i64::from(i32::MAX)) as i32;
    ParagraphMetrics {
        height: base,
        advance,
        baseline_gap,
    }
}

// Derive synthesized line spacing from the version-aware ParaShape fields
// (GG-18, specification table 46). Length-based modes serialize at twice
// the HWPUNIT value. An unspecified percentage retains the observed 160% default.
pub(crate) fn paragraph_advance(doc: &Document, para: &Paragraph, base: i32) -> i32 {
    let ps = doc.header.para_shapes.get(para.para_shape.0 as usize);
    let ls_type = ps.map_or(0, |p| p.line_spacing_type);
    let ls_val = ps.map_or(0, |p| {
        if p.line_spacing_type != 0 || p.line_spacing != 0 {
            p.line_spacing
        } else {
            p.line_spacing_old
        }
    });
    line_advance_hu(base, ls_type, ls_val)
}

/// Retain the prior nested-list synthesis convention: full supplied box width,
/// list-cumulative line advances, and no body paragraph before/after gaps.
/// This compatibility fallback is not evidence for additional native cell geometry.
pub(crate) fn compute_box_linesegs(
    store: &mut FontStore,
    doc: &Document,
    para: &Paragraph,
    width: i32,
    v: &mut i32,
    warnings: &mut RenderIssueAccumulator,
) -> Vec<LineSeg> {
    compute_linesegs(store, doc, para, width, i32::MAX, v, warnings)
}

/// Compute one paragraph's cache using a page-relative or cell-local cursor.
/// Empty paragraphs still receive one line, as required by the HWP5 writer.
pub(crate) fn compute_linesegs(
    store: &mut FontStore,
    doc: &Document,
    para: &Paragraph,
    line_width: i32,
    content_h: i32,
    v_pos: &mut i32,
    warnings: &mut RenderIssueAccumulator,
) -> Vec<LineSeg> {
    // Preserve the measured height/baseline formula and the paragraph's line spacing.
    let metrics = paragraph_metrics(doc, para);
    let base = metrics.height;
    let line_advance = metrics.advance;
    let line_spacing = line_advance.saturating_sub(base).max(0);
    let baseline_gap = metrics.baseline_gap;
    let seg_width = line_width.max(1);
    // Reduce line width by the renderer's indent (layout::line_indents): positive indent
    // affects the first line, hanging indent subsequent lines. Without subtracting it,
    // synthesized lines exceed the paragraph's right edge. List-marker width requires shaping
    // and is not included here; a marker wider than the hanging indent can lengthen the first line.
    let ps = doc.header.para_shapes.get(para.para_shape.0 as usize);
    let first_indent_hu = ps.map_or(0, |p| p.indent.max(0) / 2);
    let rest_indent_hu = ps.map_or(0, |p| (-p.indent).max(0) / 2);
    let line_limit_pt = |committed: usize| {
        let indent = if committed == 0 {
            first_indent_hu
        } else {
            rest_indent_hu
        };
        ((seg_width - indent).max(1)) as f32 / 100.0
    };
    let total = para.wchar_len();

    let make = |start: u32, v: i32| LineSeg {
        text_start: start,
        v_pos: v,
        line_height: base,
        text_height: base,
        baseline_gap,
        line_spacing,
        col_start: 0,
        seg_width,
        flags: 0x0006_0000,
    };

    // Place one line and advance the vertical cursor. Body flow restarts at the
    // next page when the line's own block (`line_height`, as in genuine saved
    // caches) no longer fits the body height; cell-local layout passes an
    // unbounded height and therefore never resets here. `layout`'s fallback
    // planner still reserves a 1.2 + 0.4 x size block in its own pass, so the two
    // can break the last line of a page differently. No local genuine sample
    // breaks a paragraph at the body bottom, so neither rule is established:
    // the cache keeps the block its own saved fields define (#423).
    let place = |segs: &mut Vec<LineSeg>, v_pos: &mut i32, start: u32| {
        if *v_pos > 0 && (*v_pos).saturating_add(base) > content_h {
            *v_pos = 0;
        }
        segs.push(make(start, *v_pos));
        *v_pos = (*v_pos).saturating_add(line_advance);
    };

    // Shape fonts and wrap within the same available width recorded in each segment.
    // Use the same glyph x_advance accumulation as place_wrapped in layout.rs.
    let items = shape_range(store, doc, para, (0, total), warnings);
    // Match place_wrapped: prefer explicit tab stops, then use the default interval.
    let tabs = crate::tab::tab_stops(doc, para);
    let mut segs = Vec::new();
    let mut line_start = 0u32;
    let mut acc = 0.0f32;
    let mut content = false;
    let mut previous_no_break = false;
    for item in &items {
        match item {
            InlineItem::Run(run) => {
                let sources = glyph_source_sequences(run);
                let mut wchar_offset = 0u32;
                for (g, source) in run.glyphs.iter().zip(&sources) {
                    let current_no_break = source_has_no_break_space(source);
                    let break_allowed = !previous_no_break
                        && !current_no_break
                        && !crate::shape::source_is_space(run, source);
                    if content && acc + g.x_advance > line_limit_pt(segs.len()) && break_allowed {
                        place(&mut segs, v_pos, line_start);
                        line_start = run.start_wchar.saturating_add(wchar_offset);
                        acc = 0.0;
                    }
                    acc += g.x_advance;
                    content = true;
                    previous_no_break = current_no_break;
                    wchar_offset = wchar_offset.saturating_add(source_wchar_len(source));
                }
            }
            InlineItem::Tab => {
                acc = crate::tab::next_tab(&tabs, acc, TAB_INTERVAL_PT);
                content = true;
                previous_no_break = false;
            }
            // LINE_BREAK (CharCtrl 10) commits the current line and resumes after the control.
            InlineItem::LineBreak(next_start) => {
                place(&mut segs, v_pos, line_start);
                line_start = *next_start;
                acc = 0.0;
                content = false;
                previous_no_break = false;
            }
        }
    }
    // Commit the final line, which is also the only line for an empty paragraph.
    place(&mut segs, v_pos, line_start);

    // Encode the paragraph's left margin in col_start (HWPUNIT). Genuine line_seg horzpos
    // stores only this margin, not first-line/hanging indent; measured hanging-indent paragraphs
    // also have horzpos = margin_left. The renderer applies indent within the line box via
    // layout::line_indents. IR margins use twice HWPUNIT, so divide by two. With zero left margin,
    // col_start stays zero and retains the previous bytes.
    let ps = doc.header.para_shapes.get(para.para_shape.0 as usize);
    let ml_hu = ps.map_or(0, |p| p.margin_left / 2).max(0);
    for seg in segs.iter_mut() {
        seg.col_start = ml_hu;
    }
    segs
}

#[cfg(test)]
mod advance_tests {
    use super::{line_advance_hu, source_wchar_len};

    #[test]
    fn line_spacing_modes_preserve_zero_and_saturate_damaged_values() {
        assert_eq!(line_advance_hu(1000, 0, 160), 1600);
        assert_eq!(line_advance_hu(1000, 1, 0), 0);
        assert_eq!(line_advance_hu(1000, 1, 600), 300);
        assert_eq!(line_advance_hu(1000, 2, 0), 1000);
        assert_eq!(line_advance_hu(1000, 2, 600), 1300);
        assert_eq!(line_advance_hu(1000, 3, 600), 1000);
        assert_eq!(line_advance_hu(i32::MAX, 2, i32::MAX), i32::MAX);
    }

    #[test]
    fn source_offsets_count_utf16_code_units() {
        assert_eq!(source_wchar_len("a"), 1);
        assert_eq!(source_wchar_len("\u{1f600}"), 2);
        assert_eq!(source_wchar_len("x\u{301}"), 2);
    }

    /// #643 review: synthesis matches `place_wrapped` when the glyph after an overflowing
    /// space is small enough to fit the freed width: it still opens a new line. The box holds
    /// "aa" plus less than one space, so the space overflows and "i" would fit only in the
    /// space's former width. Skips on a host with no font.
    #[test]
    fn synthesis_breaks_after_overflowing_space_before_small_glyph() {
        use crate::shape::{InlineItem, shape_range};
        use hwp_model::{HwpChar, Paragraph};
        let doc = hwp_convert::from_markdown("x\n");
        let mut store = crate::FontStore::new();
        let mut warns = crate::RenderIssueAccumulator::new();
        let width_of = |store: &mut crate::FontStore, chars: Vec<HwpChar>| {
            let mut warns = crate::RenderIssueAccumulator::new();
            let mut para = Paragraph {
                chars,
                ..Paragraph::default()
            };
            para.para_shape = doc.sections[0].paragraphs[0].para_shape;
            let total = para.wchar_len();
            shape_range(store, &doc, &para, (0, total), &mut warns)
                .iter()
                .map(|i| match i {
                    InlineItem::Run(r) => r.width_pt,
                    _ => 0.0,
                })
                .sum::<f32>()
        };
        let aa = width_of(&mut store, vec![HwpChar::Text('a'), HwpChar::Text('a')]);
        let space = width_of(&mut store, vec![HwpChar::Text(' ')]);
        let i = width_of(&mut store, vec![HwpChar::Text('i')]);
        if aa == 0.0 || space == 0.0 || i == 0.0 || i >= space {
            return;
        }
        // "aa" + (space - tiny): the space overflows, yet "i" fits in the remainder.
        let room = aa + (space + i) / 2.0;
        assert!(aa + space > room && aa + i <= room);
        let mut para = Paragraph {
            chars: vec![
                HwpChar::Text('a'),
                HwpChar::Text('a'),
                HwpChar::Text(' '),
                HwpChar::Text('i'),
            ],
            ..Paragraph::default()
        };
        para.para_shape = doc.sections[0].paragraphs[0].para_shape;
        let mut v = 0;
        let segs = super::compute_linesegs(
            &mut store,
            &doc,
            &para,
            (room * 100.0).floor() as i32,
            i32::MAX,
            &mut v,
            &mut warns,
        );
        let starts: Vec<u32> = segs.iter().map(|s| s.text_start).collect();
        assert_eq!(starts, [0, 3]);
    }

    /// #643 B12 in synthesis: an overflowing ordinary space stays on the line, but a FW_SPACE
    /// control (shaped from a literal space too) still breaks before it. The box is sized from
    /// the shaped widths so the boundary character overflows, so no font metric is hard-coded.
    /// Skips on a host with no font.
    #[test]
    fn synthesis_keeps_ordinary_space_but_breaks_before_fw_space() {
        use crate::shape::{InlineItem, shape_range};
        use hwp_model::{HwpChar, Paragraph, ctrl_char};
        let doc = hwp_convert::from_markdown("x\n");
        let mut store = crate::FontStore::new();
        let mut warns = crate::RenderIssueAccumulator::new();
        let starts = |store: &mut crate::FontStore, sep: HwpChar| {
            let mut warns = crate::RenderIssueAccumulator::new();
            let mut para = Paragraph {
                chars: vec![
                    HwpChar::Text('a'),
                    HwpChar::Text('a'),
                    sep,
                    HwpChar::Text('b'),
                ],
                ..Paragraph::default()
            };
            para.para_shape = doc.sections[0].paragraphs[0].para_shape;
            let items = shape_range(store, &doc, &para, (0, 2), &mut warns);
            let w: f32 = items
                .iter()
                .map(|i| match i {
                    InlineItem::Run(r) => r.width_pt,
                    _ => 0.0,
                })
                .sum();
            if w == 0.0 {
                return None;
            }
            // Room for "aa" plus a sliver, so the separator overflows.
            let width = ((w + 0.1) * 100.0).ceil() as i32;
            let mut v = 0;
            let segs =
                super::compute_linesegs(store, &doc, &para, width, i32::MAX, &mut v, &mut warns);
            Some(segs.iter().map(|s| s.text_start).collect::<Vec<_>>())
        };
        let Some(space) = starts(&mut store, HwpChar::Text(' ')) else {
            return;
        };
        // The space stays on line 1; "b" opens line 2.
        assert_eq!(space, [0, 3]);
        let fw = starts(&mut store, HwpChar::CharCtrl(ctrl_char::FW_SPACE)).unwrap();
        // FW_SPACE breaks before itself, as before B12 (the 1em FW_SPACE may wrap again on
        // this narrow box, so only the second line start is asserted).
        assert_eq!(fw.get(1), Some(&2));
        let _ = &mut warns;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hwp_model::{Cell, HwpUnit, ids::BorderFillId};

    #[test]
    fn synthesized_body_width_floors_the_page_then_subtracts_both_margins() {
        let mut doc = hwp_convert::from_markdown("Intro\n\nParagraph body\n");
        let section = &mut doc.sections[0];
        let page = section.paragraphs[0]
            .controls
            .iter_mut()
            .find_map(|control| match control {
                Control::SectionDef(def) => def.page.as_mut(),
                _ => None,
            })
            .unwrap();
        page.width.0 = 59528;
        page.margin_left.0 = 5669;
        page.margin_right.0 = 5668;
        let shape = &mut doc.header.para_shapes[section.paragraphs[1].para_shape.0 as usize];
        shape.margin_left = 2002;
        shape.margin_right = 4000;
        shape.indent = 1200;

        synthesize_linesegs(
            &mut doc,
            &mut FontStore::new_isolated(),
            &mut RenderIssueAccumulator::new(),
        );
        let lines = &doc.sections[0].paragraphs[1].line_segs;
        assert!(!lines.is_empty());
        for line in lines {
            assert_eq!(
                (line.col_start, line.seg_width),
                (1001, 48188 - 1001 - 2000),
                "floor PAGE_DEF width before the margins, keeping indent out of the cache",
            );
        }
    }

    /// Emulate a saved inline table with the affect-line-spacing flag set.
    fn set_strip_affect_line_spacing(doc: &mut Document) -> usize {
        doc.sections[0]
            .paragraphs
            .iter_mut()
            .enumerate()
            .find_map(|(index, para)| {
                para.controls.iter_mut().find_map(|control| {
                    let Control::Table(table) = control else {
                        return None;
                    };
                    table
                        .placement
                        .as_mut()
                        .expect("strip placement")
                        .affect_line_spacing = true;
                    Some(index)
                })
            })
            .expect("the strip paragraph")
    }

    /// Rule B11 (#644): a table-only inline anchor's saved line is as tall as the table object
    /// block and keeps its line spacing, and the next paragraph starts after both. Built from the
    /// `--section-strips` transform (a 1x3 inline strip at 160% spacing, the #620 T4 shape) with
    /// an isolated font store; every expectation comes from the IR's own fields, never from
    /// glyph metrics.
    #[test]
    fn an_inline_table_anchor_line_spans_the_table_and_keeps_its_spacing() {
        let mut doc = hwp_convert::from_markdown("첫 문단\n\n### 1. 절 제목\n\n뒷 문단\n");
        assert_eq!(
            hwp_convert::report_blocks::apply_section_strips(&mut doc),
            1
        );
        let strip = set_strip_affect_line_spacing(&mut doc);
        let mut store = FontStore::new_isolated();
        let mut warnings = RenderIssueAccumulator::new();
        synthesize_linesegs(&mut doc, &mut store, &mut warnings);
        let paras = &doc.sections[0].paragraphs;
        let block = inline_table_line_height(&paras[strip]).expect("a table-only inline anchor");
        let Some(Control::Table(table)) = paras[strip].controls.first() else {
            panic!("strip table first");
        };
        let object_h = table.placement.as_ref().expect("strip placement").height;
        assert_eq!(block, object_h, "no outer margins on the strip");
        let line = paras[strip].line_segs[0];
        assert_eq!(line.line_height, block, "the anchor line spans the table");
        assert_eq!(line.text_height, block);
        // The saved flag makes the percentage scale the table line itself.
        let spacing = paragraph_advance(&doc, &paras[strip], block) - block;
        assert!(spacing > 0, "the strip paragraph has percentage spacing");
        assert_eq!(
            line.line_spacing, spacing,
            "the table line keeps its spacing"
        );
        let shape = |p: &Paragraph| &doc.header.para_shapes[p.para_shape.0 as usize];
        let gap =
            shape(&paras[strip]).spacing_bottom / 2 + shape(&paras[strip + 1]).spacing_top / 2;
        assert_eq!(
            paras[strip + 1].line_segs[0].v_pos,
            line.v_pos + block + spacing + gap,
            "the next paragraph starts after the table line and its spacing"
        );
    }

    /// Rule A at the body bottom with rule B11's line (#644): when the object block is taller
    /// than the measured grid (here a stored object height), the anchor line must still fit the
    /// body or move whole to the next page; fitting only the grid leaves the saved line ending
    /// below the body.
    #[test]
    fn an_inline_table_anchor_line_taller_than_its_grid_fits_the_body() {
        let mut checked = false;
        for filler in 20..60 {
            let mut md = String::new();
            for i in 0..filler {
                md.push_str(&format!("본문 {i}\n\n"));
            }
            md.push_str("### 1. 절 제목\n\n뒷 문단\n");
            let mut doc = hwp_convert::from_markdown(&md);
            hwp_convert::report_blocks::apply_section_strips(&mut doc);
            let strip = doc.sections[0]
                .paragraphs
                .iter()
                .position(|p| p.controls.iter().any(|c| matches!(c, Control::Table(_))))
                .expect("strip");
            for c in &mut doc.sections[0].paragraphs[strip].controls {
                if let Control::Table(t) = c {
                    t.placement.as_mut().expect("placement").height = 9000;
                }
            }
            let content_h = {
                let pg = doc.sections[0]
                    .section_def()
                    .and_then(|d| d.page.as_ref())
                    .expect("page def");
                body_box(pg, pg.height.0).height_hu
            };
            let mut store = FontStore::new_isolated();
            let mut warnings = RenderIssueAccumulator::new();
            synthesize_linesegs(&mut doc, &mut store, &mut warnings);
            let line = doc.sections[0].paragraphs[strip].line_segs[0];
            assert_eq!(line.line_height, 9000);
            assert!(
                line.v_pos + line.line_height <= content_h,
                "filler={filler}: the anchor line ends at {} below the body ({content_h})",
                line.v_pos + line.line_height
            );
            let grid_fits = {
                let prev = &doc.sections[0].paragraphs[strip - 1];
                let last = prev.line_segs.last().expect("prev line");
                let cursor = last.v_pos + last.line_height + last.line_spacing;
                cursor > 0 && cursor + 9000 > content_h && cursor + 2268 + 566 <= content_h
            };
            checked |= grid_fits && line.v_pos == 0;
        }
        assert!(
            checked,
            "no filler count put the grid inside and the line outside the body"
        );
    }

    /// With the object's affect-line-spacing bit set, the percentage applies to the table line's
    /// own height (Hangul's re-save of the T4 strip: 2268 at 160% -> 1360).
    #[test]
    fn affect_line_spacing_scales_the_table_line_spacing() {
        let mut doc = hwp_convert::from_markdown("### 1. 절 제목\n\n뒷 문단\n");
        hwp_convert::report_blocks::apply_section_strips(&mut doc);
        let strip = set_strip_affect_line_spacing(&mut doc);
        let para = &doc.sections[0].paragraphs[strip];
        let block = inline_table_line_height(para).expect("inline anchor");
        let expected = paragraph_advance(&doc, para, block) - block;
        assert!(
            inline_table_affected_spacing(&doc, para, block) == Some(expected),
            "the set affect-line-spacing flag scales the table line"
        );
        assert_eq!(
            inline_table_line_advance(&doc, para),
            Some(block + expected)
        );
        let mut cleared = doc.clone();
        for c in &mut cleared.sections[0].paragraphs[strip].controls {
            if let Control::Table(t) = c {
                t.placement.as_mut().expect("placement").affect_line_spacing = false;
            }
        }
        let para = &cleared.sections[0].paragraphs[strip];
        let metrics = paragraph_metrics(&cleared, para);
        assert_eq!(
            inline_table_line_advance(&cleared, para),
            Some(block + metrics.advance - metrics.height)
        );
    }

    #[test]
    fn saved_generic_list_width_uses_container_geometry() {
        let mut header = [0; 20];
        header[8..10].copy_from_slice(&283_u16.to_le_bytes());
        header[10..12].copy_from_slice(&283_u16.to_le_bytes());
        header[16..20].copy_from_slice(&22_936_i32.to_le_bytes());
        assert_eq!(
            saved_generic_list_width(*b"gso ", &header, 50_000),
            Some(22_368)
        );
        header[8..12].copy_from_slice(&42_520_i32.to_le_bytes());
        for id in [*b"head", *b"foot"] {
            assert_eq!(saved_generic_list_width(id, &header, 50_000), Some(42_520));
        }
    }

    /// Because fill_nested accumulates v_pos within each cell, a cell's line block with N paragraphs
    /// is the last paragraph's line block alone.
    fn seg(v_pos: i32, line_height: i32) -> LineSeg {
        LineSeg {
            text_start: 0,
            v_pos,
            line_height,
            text_height: line_height,
            baseline_gap: 0,
            line_spacing: 0,
            col_start: 0,
            seg_width: 1000,
            flags: 0,
        }
    }

    fn cell_with(blocks: &[i32]) -> Cell {
        // blocks[i] is paragraph i's final line v_pos; line_height is fixed at 1000.
        Cell {
            list_attr: 0,
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
            width: HwpUnit(5000),
            height: HwpUnit(1000),
            margins: [141, 141, 141, 141],
            border_fill: BorderFillId::default(),
            header_tail: Vec::new(),
            paragraphs: blocks
                .iter()
                .map(|&v| Paragraph {
                    line_segs: vec![seg(v, 1000)],
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn table_with(cell: Cell) -> Table {
        Table {
            common_data: Vec::new(),
            placement: None,
            attr: 0,
            rows: 1,
            cols: 1,
            cell_spacing: 0,
            inner_margins: [0; 4],
            row_cell_counts: vec![1],
            border_fill: BorderFillId::default(),
            table_tail: Vec::new(),
            cells: vec![cell],
            caption: None,
            extras: Vec::new(),
        }
    }

    // Exercise the existing native-cache entry point so the pre-fix test compiles and
    // fails on geometry, not on a missing resolver API. No fonts or fixture files are used.
    fn native_test_width(margins: [u16; 4]) -> i32 {
        let snap = hwp_convert::from_markdown("Cell\n");
        let mut cell = cell_with(&[0]);
        cell.margins = margins;
        cell.paragraphs[0].line_segs.clear();
        let mut table = table_with(cell);
        table.inner_margins = [510, 510, 141, 141];
        let mut parent = Paragraph {
            controls: vec![Control::Table(table)],
            ..Default::default()
        };
        let mut fonts = FontStore::new_isolated();
        let mut issues = RenderIssueAccumulator::new();
        fill_nested(
            &mut parent,
            20000,
            &snap,
            &mut fonts,
            &mut issues,
            ListCaches::Recompute,
        );
        let Control::Table(table) = &parent.controls[0] else {
            unreachable!()
        };
        table.cells[0].paragraphs[0].line_segs[0].seg_width
    }

    fn native_test_height(margins: [u16; 4]) -> i32 {
        let mut cell = cell_with(&[0]);
        cell.margins = margins;
        let mut table = table_with(cell);
        table.inner_margins = [510, 510, 141, 141];
        table_height(&table)
    }

    #[test]
    fn native_margin_all_sentinel_width_uses_table_inner_sides() {
        assert_eq!(native_test_width([u16::MAX; 4]), 5000 - 510 - 510);
    }

    #[test]
    fn native_margin_all_sentinel_height_has_no_65535_term() {
        // Vertical fallback to the table is inferred; absence of a sentinel term is observed.
        assert_eq!(
            native_test_height([u16::MAX; 4]),
            141 + 1000 + 141 + TABLE_BLOCK_PADDING
        );
    }

    #[test]
    fn native_margin_single_horizontal_sentinel_keeps_the_other_side_zero() {
        assert_eq!(native_test_width([u16::MAX, 0, 20, 30]), 5000 - 510);
    }

    #[test]
    fn native_margin_single_vertical_sentinel_keeps_the_other_side_zero() {
        assert_eq!(
            native_test_height([0, 0, u16::MAX, 0]),
            141 + 1000 + TABLE_BLOCK_PADDING
        );
    }

    #[test]
    fn native_margin_zero_and_ordinary_values_are_unchanged_controls() {
        for margins in [[0, 0, 0, 0], [0, 250, 10, 20], [200, 300, 40, 50]] {
            assert_eq!(
                native_test_width(margins),
                5000 - i32::from(margins[0]) - i32::from(margins[1])
            );
            assert_eq!(
                native_test_height(margins),
                i32::from(margins[2]) + 1000 + i32::from(margins[3]) + TABLE_BLOCK_PADDING
            );
        }
    }

    #[test]
    fn native_body_neutrality_uses_extent_count_and_every_boundary_flag() {
        let mut original = Paragraph {
            line_segs: vec![seg(0, 1000)],
            ..Default::default()
        };
        original.line_segs[0].line_spacing = 600;
        let mut shifted = original.clone();
        shifted.line_segs[0].v_pos = 700;
        shifted.line_segs[0].baseline_gap += 5;
        assert!(
            native_body_height_neutral(&original, &shifted),
            "position/baseline changes do not change extent"
        );
        for bit in [1, 2] {
            let mut boundary = shifted.clone();
            boundary.line_segs[0].flags ^= bit;
            assert!(
                !native_body_height_neutral(&original, &boundary),
                "page/column boundary changes disqualify"
            );
        }
        for bit in [0x04, 0x08] {
            let mut boundary = shifted.clone();
            boundary.header.break_type ^= bit;
            assert!(
                !native_body_height_neutral(&original, &boundary),
                "header page/column boundary changes disqualify"
            );
        }
        let mut taller = shifted.clone();
        taller.line_segs[0].line_height += 1;
        assert!(!native_body_height_neutral(&original, &taller));
        let mut count = shifted.clone();
        count.line_segs.push(count.line_segs[0]);
        assert!(
            !native_body_height_neutral(&original, &count),
            "same overall extent cannot hide changed line count"
        );
        let mut host = original.clone();
        host.controls
            .push(Control::Table(table_with(cell_with(&[0]))));
        let mut changed = host.clone();
        let Control::Table(table) = &mut changed.controls[0] else {
            unreachable!()
        };
        table.cells[0].paragraphs[0].line_segs[0].baseline_gap += 1;
        assert!(
            !native_body_height_neutral(&host, &changed),
            "changed nested cache disqualifies even equal table height"
        );
        let mut opaque = host.clone();
        let Control::Table(table) = &mut opaque.controls[0] else {
            unreachable!()
        };
        table.table_tail.push(1);
        assert!(
            !native_body_height_neutral(&host, &opaque),
            "opaque object changes cannot establish equal layout context"
        );
    }

    #[test]
    fn native_body_forward_guard_uses_source_boundaries_and_rejects_overlap() {
        let original = Paragraph {
            line_segs: vec![seg(1000, 1000)],
            ..Default::default()
        };
        let mut following = Paragraph {
            line_segs: vec![seg(3000, 1000)],
            ..Default::default()
        };
        fn entry(p: &Paragraph) -> Option<NativeBodyCache<'_>> {
            Some(NativeBodyCache {
                original: Some(p),
                floating_original: None,
                unchanged: true,
                successor_v_pos: None,
            })
        }
        assert!(native_body_suffix_fits(&original, entry(&following), 3000));
        assert!(!native_body_suffix_fits(&original, entry(&following), 3001));
        following.header.break_type = 4;
        assert!(native_body_suffix_fits(&original, entry(&following), 9000));
        following.header.break_type = 0;
        following.line_segs[0].flags = 2;
        assert!(native_body_suffix_fits(&original, entry(&following), 9000));
        following.line_segs[0].flags = 0;
        following.line_segs[0].v_pos = 500;
        assert!(
            native_body_suffix_fits(&original, entry(&following), 9000),
            "existing source backstep is a boundary"
        );
        following.line_segs.clear();
        assert!(
            !native_body_suffix_fits(&original, entry(&following), 1600),
            "no stored boundary cannot prove a safe keep"
        );
    }

    #[test]
    fn native_body_preservation_resets_at_the_next_section() {
        let mut original = hwp_convert::from_markdown("One\n\nTwo\n");
        original.meta.source_format = "hwp5".to_owned();
        for shape in &mut original.header.char_shapes {
            shape.base_size = 1000;
        }
        for shape in &mut original.header.para_shapes {
            shape.spacing_top = 0;
            shape.spacing_bottom = 0;
            shape.line_spacing_type = 0;
            shape.line_spacing = 160;
        }
        for (i, p) in original.sections[0].paragraphs.iter_mut().enumerate() {
            p.line_segs = vec![seg(i as i32 * 1600, 1000)];
            p.line_segs[0].line_spacing = 600;
        }
        original.sections.push(original.sections[0].clone());
        original.sections[1].paragraphs[0].line_segs[0].v_pos = 777;
        original.sections[1].paragraphs[1].line_segs[0].v_pos = 2377;
        let mut current = original.clone();
        let mut large = current.header.char_shapes[0].clone();
        large.base_size = 2000;
        current.header.char_shapes.push(large);
        current.sections[0].paragraphs[0].char_shape_runs = vec![(
            0,
            hwp_model::CharShapeId((current.header.char_shapes.len() - 1) as u16),
        )];
        current.sections[0].paragraphs[0].line_segs.clear();
        let source = original
            .sections
            .iter()
            .enumerate()
            .map(|(si, section)| {
                section
                    .paragraphs
                    .iter()
                    .enumerate()
                    .map(|(pi, p)| NativeBodyCache {
                        original: Some(p),
                        floating_original: None,
                        unchanged: !(si == 0 && pi == 0),
                        successor_v_pos: section
                            .paragraphs
                            .get(pi + 1)
                            .and_then(|p| p.line_segs.first())
                            .map(|line| line.v_pos),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut fonts = FontStore::new_isolated();
        let mut warnings = RenderIssueAccumulator::new();
        let kept = synthesize_native_linesegs_seeded(
            &mut current,
            &mut fonts,
            &mut warnings,
            &BodyFlow::default(),
            &source,
        );
        assert!(
            !kept.contains(&(0, 1)),
            "first section stays in reflow after height change"
        );
        for pi in 0..2 {
            assert!(kept.contains(&(1, pi)), "next section starts fresh");
            assert_eq!(
                current.sections[1].paragraphs[pi].line_segs,
                original.sections[1].paragraphs[pi].line_segs
            );
        }
    }

    #[test]
    fn table_height_multi_paragraph_cell_is_not_double_counted() {
        // Four paragraphs at v_pos 0/1600/3200/4800 give cell line block 4800 + 1000.
        let h = table_height(&table_with(cell_with(&[0, 1600, 3200, 4800])));
        assert_eq!(h, 141 + (4800 + 1000) + 141 + TABLE_BLOCK_PADDING);
    }

    #[test]
    fn table_height_single_paragraph_cell_unchanged() {
        let h = table_height(&table_with(cell_with(&[0])));
        assert_eq!(h, 141 + 1000 + 141 + TABLE_BLOCK_PADDING);
    }

    #[test]
    fn table_height_empty_cell_falls_back() {
        // With no paragraphs, the cell line block is zero; only margins remain and fallback does not panic.
        let h = table_height(&table_with(cell_with(&[])));
        assert_eq!(h, 141 + 141 + TABLE_BLOCK_PADDING);
    }

    #[test]
    fn saved_body_cursor_follows_the_walks_two_readings() {
        // No table: the saved line block (0 + 1000) is the bottom, plus space-after 300.
        let mut plain = Paragraph {
            line_segs: vec![seg(0, 1000)],
            ..Default::default()
        };
        assert_eq!(saved_body_cursor(&plain, 300), Some(1300));

        // A table paragraph advances past the table, not past its text line (#462): the saved
        // entry position 0 plus the table's height, plus the same space-after.
        let table = table_with(cell_with(&[0]));
        let height = table_height(&table);
        plain.controls.push(Control::Table(table));
        assert_eq!(saved_body_cursor(&plain, 300), Some(height + 300));
        assert!(height > 1300, "the table reaches below the text line");
    }

    /// Rule B11 (#644) is a body rule: a kept table-only inline anchor inside a nested list
    /// (a cell) keeps the pre-#644 reading, the larger of the measured table and the saved line
    /// block, with no B11 line spacing, while the body reading takes the B11 line advance.
    #[test]
    fn b11_reads_body_anchors_only_never_nested_list_paragraphs() {
        let mut inner = table_with(cell_with(&[0]));
        inner.placement = Some(hwp_model::GsoPlacement {
            treat_as_char: true,
            height: 3000,
            ..Default::default()
        });
        let height = table_height(&inner);
        let mut anchor = Paragraph {
            line_segs: vec![LineSeg {
                line_spacing: 1800,
                ..seg(0, 3000)
            }],
            ..Default::default()
        };
        anchor.controls.push(Control::Table(inner));
        assert!(
            inline_table_line_height(&anchor).is_some(),
            "a B11 anchor shape"
        );
        assert_eq!(
            saved_list_cursor(&anchor, 0),
            Some(height.max(3000)),
            "nested lists keep the pre-#644 reading"
        );
        assert_eq!(
            saved_body_cursor(&anchor, 0),
            Some(4800),
            "the body reading takes the B11 line advance"
        );
    }

    /// A kept body anchor that ahwp saved before #644 still carries the font line height (1000)
    /// under a taller inline table block (3000); an edit that continues the flow after it must
    /// start below the table, never inside it.
    #[test]
    fn a_kept_pre_b11_anchor_line_still_clears_its_table() {
        let mut inner = table_with(cell_with(&[0]));
        inner.placement = Some(hwp_model::GsoPlacement {
            treat_as_char: true,
            height: 3000,
            ..Default::default()
        });
        let mut anchor = Paragraph {
            line_segs: vec![LineSeg {
                line_spacing: 600,
                ..seg(500, 1000)
            }],
            ..Default::default()
        };
        anchor.controls.push(Control::Table(inner));
        assert!(
            inline_table_line_height(&anchor).is_some(),
            "a B11 anchor shape"
        );
        let cursor = saved_body_cursor(&anchor, 300).expect("cursor");
        assert!(
            cursor >= 500 + 3000 + 300,
            "the next paragraph starts at {cursor}, inside the table that ends at 3500"
        );
    }

    /// A line break or tab makes a table-only paragraph more than one line, so B11 (one saved
    /// table line) does not apply on either the synthesis or the layout side.
    #[test]
    fn a_line_break_or_tab_takes_a_table_paragraph_off_b11() {
        let mut inner = table_with(cell_with(&[0]));
        inner.placement = Some(hwp_model::GsoPlacement {
            treat_as_char: true,
            height: 3000,
            ..Default::default()
        });
        let mut anchor = Paragraph::default();
        anchor.controls.push(Control::Table(inner));
        assert!(inline_table_line_height(&anchor).is_some());
        let mut broken = anchor.clone();
        broken.chars.push(hwp_model::HwpChar::CharCtrl(
            hwp_model::ctrl_char::LINE_BREAK,
        ));
        assert_eq!(inline_table_line_height(&broken), None, "line break");
        let mut tabbed = anchor;
        tabbed.chars.push(hwp_model::HwpChar::InlineCtrl {
            code: hwp_model::ctrl_char::TAB,
            payload: vec![0; 12],
        });
        assert_eq!(inline_table_line_height(&tabbed), None, "tab");
    }
}

/// Regression coverage for #426: an anchor prepass that stops before it collected every
/// paragraph must not publish its unvisited slots as measured positions.
///
/// Every fixture paginates from geometry that exists without any font: a body paragraph
/// advances 1600 HWPUNIT plus 300 of space-after, an empty cell paragraph is one 1000-HWPUNIT
/// line, and a line's baseline sits 1200 below its top with a 400 descent. The store is
/// isolated, so no host face is consulted and no assertion counts pages or reads glyph metrics.
#[cfg(test)]
mod anchor_prepass_tests {
    use super::*;
    use crate::issues::{RenderIssueAccumulator, RenderIssueCode};
    use hwp_model::{Cell, HwpUnit, ids::BorderFillId};

    /// Body height that holds two paragraph lines (0 and 1900) and moves the third to page 2.
    const TWO_LINE_BODY: i32 = 4000;
    /// Body height that holds one paragraph line, so a three-row table cannot share the page.
    const ONE_LINE_BODY: i32 = 2400;

    /// Zero-margin page of `height_hu` HWPUNIT: the body box is the whole page, so pagination
    /// follows flow geometry alone.
    fn set_body_height(doc: &mut Document, height_hu: i32) {
        for control in &mut doc.sections[0].paragraphs[0].controls {
            let Control::SectionDef(definition) = control else {
                continue;
            };
            let page = definition
                .page
                .as_mut()
                .expect("from_markdown defines a page");
            page.height = HwpUnit(height_hu);
            page.margin_top = HwpUnit(0);
            page.margin_bottom = HwpUnit(0);
            page.margin_header = HwpUnit(0);
            page.margin_footer = HwpUnit(0);
            page.margin_left = HwpUnit(0);
            page.margin_right = HwpUnit(0);
        }
    }

    /// A body-flow table (`treat_as_char == false`), the shape that makes a section read
    /// prepass anchors. Its cells hold one empty paragraph each.
    fn floating_table(rows: u16, row_height: i32) -> Table {
        let cells = (0..rows)
            .map(|row| Cell {
                list_attr: 0,
                col: 0,
                row,
                col_span: 1,
                row_span: 1,
                width: HwpUnit(20000),
                height: HwpUnit(row_height),
                margins: [0; 4],
                border_fill: BorderFillId(1),
                header_tail: Vec::new(),
                paragraphs: vec![Paragraph::default()],
            })
            .collect();
        Table {
            common_data: Vec::new(),
            placement: Some(hwp_model::GsoPlacement {
                treat_as_char: false,
                vert_rel_to: 2,
                horz_rel_to: 3,
                ..Default::default()
            }),
            attr: 1,
            rows,
            cols: 1,
            cell_spacing: 0,
            inner_margins: [0; 4],
            row_cell_counts: vec![1; usize::from(rows)],
            border_fill: BorderFillId(1),
            table_tail: Vec::new(),
            cells,
            caption: None,
            extras: Vec::new(),
        }
    }

    fn issue_count(report: &crate::issues::RenderIssueReport, code: RenderIssueCode) -> u64 {
        report
            .issues
            .iter()
            .filter(|issue| issue.code == code)
            .map(|issue| issue.count)
            .sum()
    }

    fn synthesize_complete(
        doc: &mut Document,
    ) -> (crate::layout::BodyAnchors, crate::issues::RenderIssueReport) {
        let mut store = FontStore::new_isolated();
        let mut warnings = RenderIssueAccumulator::new();
        let anchors = synthesize_linesegs_inner(
            doc,
            &mut store,
            &mut warnings,
            None,
            &BodyFlow::default(),
            None,
        )
        .expect("a floating-table section runs the prepass");
        (anchors, warnings.finish())
    }

    fn synthesize_with_prepass_limit(
        doc: &mut Document,
        page_limit: usize,
    ) -> (crate::layout::BodyAnchors, crate::issues::RenderIssueReport) {
        let mut store = FontStore::new_isolated();
        let mut warnings = RenderIssueAccumulator::new();
        let anchors =
            synthesize_linesegs_with_prepass_page_limit(doc, &mut store, &mut warnings, page_limit)
                .expect("a floating-table section runs the prepass");
        (anchors, warnings.finish())
    }

    /// Rule B11 (#644) in a floating-table section, which takes its anchors from the cacheless
    /// prepass: a table-only inline anchor whose object block (here a stored 9000 height over a
    /// one-row strip) does not fit below its entry moves whole to the next page, and its line
    /// moves with it. The synthesized line must start that page, never end below the body.
    #[test]
    fn a_floating_section_moves_an_unfitting_b11_line_with_its_table() {
        let mut moved = false;
        for filler in 20..40 {
            let mut md = String::new();
            for i in 0..filler {
                md.push_str(&format!("본문 {i}\n\n"));
            }
            md.push_str("### 1. 절 제목\n\n뒷 문단\n");
            let mut doc = hwp_convert::from_markdown(&md);
            hwp_convert::report_blocks::apply_section_strips(&mut doc);
            doc.sections[0].paragraphs[0]
                .controls
                .push(Control::Table(floating_table(1, 1000)));
            let strip = doc.sections[0]
                .paragraphs
                .iter()
                .position(|p| inline_table_line_height(p).is_some())
                .expect("strip");
            for c in &mut doc.sections[0].paragraphs[strip].controls {
                if let Control::Table(t) = c {
                    t.placement.as_mut().expect("placement").height = 9000;
                }
            }
            let content_h = {
                let pg = doc.sections[0]
                    .section_def()
                    .and_then(|d| d.page.as_ref())
                    .expect("page def");
                body_box(pg, pg.height.0).height_hu
            };
            let (anchors, _) = synthesize_complete(&mut doc);
            assert!(anchors.section_complete(0));
            let line = doc.sections[0].paragraphs[strip].line_segs[0];
            assert_eq!(line.line_height, 9000);
            assert!(
                line.v_pos + line.line_height <= content_h,
                "filler={filler}: the B11 line ends at {} below the body ({content_h})",
                line.v_pos + line.line_height
            );
            if line.v_pos == 0 && line.flags & 0x1 != 0 {
                moved = true;
            }
        }
        assert!(moved, "no filler count moved the B11 line to a new page");
    }

    /// Ordinary complete pass: measured anchors drive the section, including the page-top anchor
    /// whose `starts_page` still has to reach the synthesized first line.
    #[test]
    fn complete_prepass_publishes_every_measured_anchor_and_its_page_start() {
        let mut doc = hwp_convert::from_markdown("A\n\nB\n\nC\n");
        set_body_height(&mut doc, TWO_LINE_BODY);
        doc.sections[0].paragraphs[0]
            .controls
            .push(Control::Table(floating_table(1, 1000)));

        let (anchors, warnings) = synthesize_complete(&mut doc);
        assert!(anchors.completed(), "an unconstrained prepass completes");
        assert!(anchors.section_complete(0));
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::AnchorPrepassIncomplete),
            0,
            "a complete pass reports no fallback"
        );

        let page_top = anchors.slot(0, 2).expect("third paragraph visited");
        assert!(
            page_top.starts_page,
            "fixture: the third paragraph's first line opens page 2"
        );
        for pi in 0..3 {
            let anchor = anchors
                .slot(0, pi)
                .unwrap_or_else(|| panic!("paragraph {pi} visited"));
            let first = &doc.sections[0].paragraphs[pi].line_segs[0];
            assert_eq!(
                first.v_pos, anchor.v_pos,
                "paragraph {pi} publishes its measured anchor, not a base-cursor position"
            );
            assert_eq!(
                first.flags & 0x1 != 0,
                anchor.starts_page,
                "paragraph {pi} keeps the measured page-start bit"
            );
        }
    }

    /// Stop inside a paragraph's own line pagination: the third paragraph's first line opens
    /// page 2, which is the document's first page transition, so the fourth paragraph is never
    /// visited and its slot stays empty.
    #[test]
    fn prepass_stop_during_paragraph_pagination_never_publishes_a_default_zero_anchor() {
        let mut doc = hwp_convert::from_markdown("A\n\nB\n\nC\n\nD\n");
        set_body_height(&mut doc, TWO_LINE_BODY);
        doc.sections[0].paragraphs[3]
            .controls
            .push(Control::Table(floating_table(1, 500)));

        let (anchors, warnings) = synthesize_with_prepass_limit(&mut doc, 0);
        let opening = anchors.slot(0, 2).expect("third paragraph visited");
        assert!(
            opening.starts_page,
            "fixture: the stop happens where the third paragraph's first line opens page 2"
        );
        assert!(
            anchors.slot(0, 3).is_none(),
            "the fourth paragraph was never visited"
        );
        assert!(
            !anchors.completed(),
            "a page-budget stop is not a complete pass"
        );
        assert!(!anchors.section_complete(0));
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::AnchorPrepassIncomplete),
            1,
            "the section reports its fallback once, not once per paragraph"
        );
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::LayoutBudgetExceeded),
            1,
            "the prepass's own budget diagnostic reaches the caller"
        );

        let first = &doc.sections[0].paragraphs[3].line_segs[0];
        assert_ne!(
            first.v_pos, 0,
            "an unvisited anchor must not be published as a default-zero position"
        );
    }

    /// Stop inside the floating table's own page transition, after every paragraph's anchor was
    /// already recorded: the slots cannot reveal this stop, so the completion result has to.
    #[test]
    fn prepass_stop_during_floating_table_continuation_still_reports_incomplete() {
        let mut doc = hwp_convert::from_markdown("A\n\nB\n");
        set_body_height(&mut doc, ONE_LINE_BODY);
        doc.sections[0].paragraphs[0]
            .controls
            .push(Control::Table(floating_table(3, 1000)));

        let (anchors, warnings) = synthesize_with_prepass_limit(&mut doc, 0);
        assert!(
            anchors.slot(0, 0).is_some() && anchors.slot(0, 1).is_some(),
            "every paragraph's anchor was recorded before the table needed its next page"
        );
        assert!(
            !anchors.completed(),
            "a stop after the last anchor still leaves the prepass incomplete"
        );
        assert!(!anchors.section_complete(0));
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::AnchorPrepassIncomplete),
            1
        );
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::LayoutBudgetExceeded),
            1
        );

        let measured = anchors.slot(0, 1).expect("second paragraph visited");
        assert!(
            measured.starts_page,
            "fixture: the second paragraph opens a page once the table continues"
        );
        let first = &doc.sections[0].paragraphs[1].line_segs[0];
        assert_eq!(
            first.flags & 0x1,
            0,
            "the base cursor fallback must not apply a page-start flag it did not measure"
        );
    }

    /// Stop inside the first section leaves a later floating-table section unvisited; it must be
    /// reported and laid out by the base cursor, not from empty slots.
    #[test]
    fn prepass_stop_leaves_a_later_section_unvisited_and_reported() {
        let mut doc = hwp_convert::from_markdown("A\n\nB\n\nC\n\nD\n");
        set_body_height(&mut doc, TWO_LINE_BODY);
        let mut later = hwp_convert::from_markdown("E\n\nF\n");
        set_body_height(&mut later, TWO_LINE_BODY);
        later.sections[0].paragraphs[1]
            .controls
            .push(Control::Table(floating_table(1, 500)));
        doc.sections.push(later.sections.remove(0));

        let (anchors, warnings) = synthesize_with_prepass_limit(&mut doc, 0);
        assert!(
            anchors.slot(0, 2).is_some(),
            "fixture: the stop happens inside section 0's paragraph pagination"
        );
        assert!(
            anchors.slot(1, 0).is_none() && anchors.slot(1, 1).is_none(),
            "the later section is never visited"
        );
        assert!(!anchors.completed());
        assert!(!anchors.section_complete(1));
        assert_eq!(
            issue_count(&warnings, RenderIssueCode::AnchorPrepassIncomplete),
            1,
            "only the floating-table section consumes anchors and reports"
        );

        assert!(
            !doc.sections[1].paragraphs[0].line_segs.is_empty(),
            "the fallback still synthesizes the unvisited section"
        );
        let first = &doc.sections[1].paragraphs[1].line_segs[0];
        assert_ne!(
            first.v_pos, 0,
            "an unvisited later section is not published as default-zero anchors"
        );
    }
}

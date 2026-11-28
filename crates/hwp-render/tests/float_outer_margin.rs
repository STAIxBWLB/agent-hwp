//! Rules B14 (#655) and B16 (#659): a top-level floating table anchored to the paragraph
//! vertically and to the column or paragraph horizontally draws its grid at the anchor plus its
//! offsets plus one left and one top outer margin, as Hancom does; a paragraph-relative table also
//! moves right by the anchor paragraph's left margin. Every number is page geometry or a stored
//! table field, so no assertion depends on a host font.

use hwp_model::{BorderFillId, Cell, Control, GsoPlacement, HwpUnit, Paragraph, Table};
use hwp_render::display::Item;
use hwp_render::{FontStore, RenderIssueAccumulator};

fn floating_table(horz_rel_to: u8, vert_offset: i32, out_margins: [u16; 4]) -> Table {
    aligned_table(horz_rel_to, vert_offset, out_margins, 0, 0)
}

fn aligned_table(
    horz_rel_to: u8,
    vert_offset: i32,
    out_margins: [u16; 4],
    vert_align: u8,
    horz_align: u8,
) -> Table {
    Table {
        common_data: Vec::new(),
        placement: Some(GsoPlacement {
            treat_as_char: false,
            vert_rel_to: 2,
            horz_rel_to,
            vert_offset,
            vert_align,
            horz_align,
            out_margins,
            ..Default::default()
        }),
        attr: 1,
        rows: 1,
        cols: 1,
        cell_spacing: 0,
        inner_margins: [0; 4],
        row_cell_counts: vec![1],
        border_fill: BorderFillId(1),
        table_tail: Vec::new(),
        cells: vec![Cell {
            list_attr: 0,
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
            width: HwpUnit(10000),
            height: HwpUnit(2000),
            margins: [0; 4],
            border_fill: BorderFillId(1),
            header_tail: Vec::new(),
            paragraphs: vec![Paragraph::default()],
        }],
        caption: None,
        extras: Vec::new(),
    }
}

/// Top-left of the table's cell background, the page's only filled rect: the grid origin.
fn grid_origin(table: Table) -> (f32, f32) {
    let mut doc = hwp_convert::from_markdown("x\n");
    doc.header.border_fills.push(hwp_model::BorderFill {
        bg_color: Some(0x00ff_0000),
        ..Default::default()
    });
    let fill = BorderFillId(doc.header.border_fills.len() as u16);
    let mut table = table;
    table.border_fill = fill;
    table.cells[0].border_fill = fill;
    let para = &mut doc.sections[0].paragraphs[0];
    para.controls.push(Control::Table(table));
    para.line_segs.clear();
    let mut store = FontStore::new_isolated();
    let mut warns = RenderIssueAccumulator::new();
    let list = hwp_render::layout::layout_document(&doc, &mut store, &mut warns);
    let mut min = (f32::INFINITY, f32::INFINITY);
    for item in &list.pages[0].items {
        // The cell background is the only filled rect on the page.
        let pts: Vec<(f32, f32)> = match item {
            Item::Rect { x, y, .. } => vec![(*x, *y)],
            _ => Vec::new(),
        };
        for (x, y) in pts {
            min.0 = min.0.min(x);
            min.1 = min.1.min(y);
        }
    }
    assert!(min.0.is_finite(), "the table grid must draw");
    min
}

#[test]
fn floating_column_table_grid_moves_by_left_and_top_outer_margin() {
    let base = grid_origin(floating_table(2, 0, [0; 4]));
    // Distinct per side so a swapped field fails: left 5.67 pt, top 8.50 pt.
    let shifted = grid_origin(floating_table(2, 0, [567, 1134, 850, 283]));
    assert!(
        (shifted.0 - base.0 - 5.67).abs() < 0.01 && (shifted.1 - base.1 - 8.50).abs() < 0.01,
        "grid origin {shifted:?} must be {base:?} + (5.67, 8.50)"
    );
}

#[test]
fn offsets_add_to_the_margins_in_both_relations() {
    // #659 Hancom probe d3/d2: vertOffset 1417 / horzOffset 2834 HU add +14.17 / +28.34 pt on top
    // of the outer margins, for COLUMN and PARA alike.
    let margins = [567, 1134, 850, 283];
    for horz_rel_to in [2, 3] {
        let base = grid_origin(floating_table(horz_rel_to, 0, margins));
        let mut t = floating_table(horz_rel_to, 1417, margins);
        t.placement.as_mut().unwrap().horz_offset = 2834;
        let moved = grid_origin(t);
        assert!(
            (moved.0 - base.0 - 28.34).abs() < 0.01 && (moved.1 - base.1 - 14.17).abs() < 0.01,
            "horz_rel_to {horz_rel_to}: {moved:?} must be {base:?} + (28.34, 14.17)"
        );
    }
}

/// The same table as `floating_table`, but carried as HWP5 object-common bytes (HWP 5.0 §4.3.9
/// table 69: attr @0, vertical offset @4, horizontal offset @8, outer margins @24) with no HWPX
/// placement, so the raw decoding path is exercised.
fn hwp5_table(
    horz_rel_to: u32,
    vert_offset: i32,
    horz_offset: i32,
    out_margins: [u16; 4],
) -> Table {
    let mut t = floating_table(2, 0, [0; 4]);
    t.placement = None;
    // bit0 treat-as-char clear, bits3-4 vertical PARA (2), bits5-7 vert align TOP (0),
    // bits8-9 horizontal relation, bits10-12 horz align LEFT (0).
    let attr: u32 = (2 << 3) | (horz_rel_to << 8);
    let mut d = vec![0u8; 36];
    d[0..4].copy_from_slice(&attr.to_le_bytes());
    d[4..8].copy_from_slice(&vert_offset.to_le_bytes());
    d[8..12].copy_from_slice(&horz_offset.to_le_bytes());
    for (i, m) in out_margins.iter().enumerate() {
        d[24 + 2 * i..26 + 2 * i].copy_from_slice(&m.to_le_bytes());
    }
    t.common_data = d;
    t
}

#[test]
fn hwp5_object_common_bytes_shift_like_the_placement() {
    // The HWP5 branch must decode the same rule: COLUMN and PARA relations, and a positive
    // offset adding to the margins.
    let margins = [567, 1134, 850, 283];
    for rel in [2u32, 3] {
        let base = grid_origin(hwp5_table(rel, 0, 0, [0; 4]));
        let shifted = grid_origin(hwp5_table(rel, 0, 0, margins));
        assert!(
            (shifted.0 - base.0 - 5.67).abs() < 0.01 && (shifted.1 - base.1 - 8.50).abs() < 0.01,
            "rel {rel}: margins {shifted:?} must be {base:?} + (5.67, 8.50)"
        );
        let offset = grid_origin(hwp5_table(rel, 1417, 2834, margins));
        assert!(
            (offset.0 - base.0 - 34.01).abs() < 0.01 && (offset.1 - base.1 - 22.67).abs() < 0.01,
            "rel {rel}: offsets {offset:?} must be {base:?} + (34.01, 22.67)"
        );
    }
}

#[test]
fn hwp5_negative_offset_keeps_the_anchor_origin() {
    // Signed decoding: a negative offset is unmeasured, so the margins must not move the grid.
    let margins = [567, 1134, 850, 283];
    for (vo, ho) in [(-1000, 0), (0, -1000)] {
        assert_eq!(
            grid_origin(hwp5_table(2, vo, ho, margins)),
            grid_origin(hwp5_table(2, vo, ho, [0; 4])),
            "vo {vo} ho {ho}"
        );
    }
}

#[test]
fn offset_grid_never_overlaps_the_following_content() {
    // The flow advances by vertical offset + top margin (a no-overlap lower bound; Hancom's exact
    // flow is unestablished). With vertOffset 1417 the next paragraph's table starts at or below
    // the shifted grid's bottom.
    let (grid, next) = grid_and_next_offset([567, 1134, 850, 283], 1417);
    assert!(
        next.1 + 0.01 >= grid.3,
        "next content {next:?} starts above the offset grid's bottom {grid:?}"
    );
    let (plain, _) = grid_and_next_offset([0; 4], 0);
    assert!(
        (grid.1 - plain.1 - 22.67).abs() < 0.01,
        "grid {grid:?} vs {plain:?}"
    );
}

#[test]
fn negative_offsets_keep_the_anchor_origin() {
    // Only non-negative offsets are measured: a negative one keeps the anchor origin.
    let margins = [567, 1134, 850, 283];
    assert_eq!(
        grid_origin(floating_table(2, -1000, margins)),
        grid_origin(floating_table(2, -1000, [0; 4]))
    );
}

/// Grid origin with the anchor paragraph given a left margin and first-line indent, in HWPUNIT.
fn indented_grid_origin(horz_rel_to: u8, margin_left_hu: i32, indent_hu: i32) -> (f32, f32) {
    let mut doc = hwp_convert::from_markdown("x\n");
    doc.header.border_fills.push(hwp_model::BorderFill {
        bg_color: Some(0x00ff_0000),
        ..Default::default()
    });
    let fill = BorderFillId(doc.header.border_fills.len() as u16);
    let base_shape = doc.sections[0].paragraphs[0].para_shape.0 as usize;
    let mut shape = doc.header.para_shapes[base_shape].clone();
    // The model stores paragraph margins doubled (HWPX value x 2; layout divides by 200).
    shape.margin_left = margin_left_hu * 2;
    shape.indent = indent_hu * 2;
    doc.header.para_shapes.push(shape);
    let shape_id = (doc.header.para_shapes.len() - 1) as u16;
    let mut table = floating_table(horz_rel_to, 0, [567, 1134, 850, 283]);
    table.border_fill = fill;
    table.cells[0].border_fill = fill;
    let para = &mut doc.sections[0].paragraphs[0];
    para.para_shape = hwp_model::ParaShapeId(shape_id);
    para.controls.push(Control::Table(table));
    para.line_segs.clear();
    let mut store = FontStore::new_isolated();
    let mut warns = RenderIssueAccumulator::new();
    let list = hwp_render::layout::layout_document(&doc, &mut store, &mut warns);
    let mut min = (f32::INFINITY, f32::INFINITY);
    for item in &list.pages[0].items {
        if let Item::Rect { x, y, fill, .. } = item
            && *fill == 0x00ff_0000
        {
            min = (min.0.min(*x), min.1.min(*y));
        }
    }
    assert!(min.0.is_finite(), "the table grid must draw");
    min
}

#[test]
fn para_relative_grid_adds_the_anchor_left_margin_not_the_indent() {
    // #659 Hancom probe d0/d1: with the anchor paragraph at left 2834 / first-line 1417 HU, a
    // PARA-relative grid sits 28.34 pt right of the COLUMN one (the indent does not count), and
    // the COLUMN grid ignores the paragraph entirely.
    let column = indented_grid_origin(2, 2834, 1417);
    let column_plain = indented_grid_origin(2, 0, 0);
    let para = indented_grid_origin(3, 2834, 1417);
    assert_eq!(
        column, column_plain,
        "COLUMN must ignore the anchor paragraph's indent"
    );
    assert!(
        (para.0 - column.0 - 28.34).abs() < 0.01 && (para.1 - column.1).abs() < 0.01,
        "PARA {para:?} must be COLUMN {column:?} + (28.34, 0)"
    );
}

/// Grid origin of a floating table nested in a cell of an inline outer table: the top-left of the
/// nested cell's background.
fn nested_grid_origin(out_margins: [u16; 4]) -> (f32, f32) {
    let mut inner = floating_table(2, 0, out_margins);
    let mut outer = floating_table(2, 0, [0; 4]);
    outer.placement.as_mut().unwrap().treat_as_char = true;
    outer.cells[0].width = HwpUnit(20000);
    outer.cells[0].height = HwpUnit(8000);
    let mut doc = hwp_convert::from_markdown("x\n");
    doc.header.border_fills.push(hwp_model::BorderFill {
        bg_color: Some(0x00ff_0000),
        ..Default::default()
    });
    let fill = BorderFillId(doc.header.border_fills.len() as u16);
    inner.border_fill = fill;
    inner.cells[0].border_fill = fill;
    let mut cell_para = Paragraph::default();
    cell_para.controls.push(Control::Table(inner));
    outer.cells[0].paragraphs = vec![cell_para];
    let para = &mut doc.sections[0].paragraphs[0];
    para.controls.push(Control::Table(outer));
    para.line_segs.clear();
    let mut store = FontStore::new_isolated();
    let mut warns = RenderIssueAccumulator::new();
    let list = hwp_render::layout::layout_document(&doc, &mut store, &mut warns);
    let mut found = Vec::new();
    for item in &list.pages[0].items {
        if let Item::Rect { x, y, fill, .. } = item {
            found.push((*x, *y, *fill));
        }
    }
    // The outer cell has no background, so the only filled rect is the nested table's cell.
    assert_eq!(found.len(), 1, "only the nested grid fills: {found:?}");
    (found[0].0, found[0].1)
}

#[test]
fn nested_floating_table_keeps_the_anchor_origin() {
    // Nested lists (cells, text boxes) have no B14 evidence (#410), so margins do not move them.
    assert_eq!(
        nested_grid_origin([567, 1134, 850, 283]),
        nested_grid_origin([0; 4])
    );
}

/// A rect's (x0, y0, x1, y1) in points.
type Bounds = (f32, f32, f32, f32);

/// Bounds of the red floating grid and the blue inline marker table in the next paragraph.
/// Both are font-free filled cell rects.
fn grid_and_next(out_margins: [u16; 4], grid_width: i32) -> (Bounds, Bounds) {
    grid_and_next_flow(out_margins, grid_width, false)
}

fn grid_and_next_offset(out_margins: [u16; 4], vert_offset: i32) -> (Bounds, Bounds) {
    grid_and_next_with(out_margins, 10000, false, vert_offset)
}

fn grid_and_next_flow(out_margins: [u16; 4], grid_width: i32, flow: bool) -> (Bounds, Bounds) {
    grid_and_next_with(out_margins, grid_width, flow, 0)
}

fn grid_and_next_with(
    out_margins: [u16; 4],
    grid_width: i32,
    flow: bool,
    vert_offset: i32,
) -> (Bounds, Bounds) {
    let mut doc = hwp_convert::from_markdown("x\n\nafter\n");
    for color in [0x0000_ff00u32, 0x0000_00ff] {
        doc.header.border_fills.push(hwp_model::BorderFill {
            bg_color: Some(color),
            ..Default::default()
        });
    }
    let n = doc.header.border_fills.len() as u16;
    let (red, blue) = (BorderFillId(n - 1), BorderFillId(n));
    let mut grid = floating_table(2, vert_offset, out_margins);
    grid.placement.as_mut().unwrap().flow_with_text = flow;
    grid.cells[0].width = HwpUnit(grid_width);
    grid.cells[0].border_fill = red;
    let mut marker = floating_table(2, 0, [0; 4]);
    marker.placement.as_mut().unwrap().treat_as_char = true;
    marker.cells[0].border_fill = blue;
    let paras = &mut doc.sections[0].paragraphs;
    paras[0].controls.push(Control::Table(grid));
    paras[1].controls.push(Control::Table(marker));
    for p in paras.iter_mut() {
        p.line_segs.clear();
    }
    let mut store = FontStore::new_isolated();
    let mut warns = RenderIssueAccumulator::new();
    let list = hwp_render::layout::layout_document(&doc, &mut store, &mut warns);
    let rect = |color: u32| {
        let found: Vec<_> = list.pages[0]
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Rect { x, y, w, h, fill } if *fill == color => {
                    Some((*x, *y, *x + *w, *y + *h))
                }
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "color {color:#x} must draw once: {found:?}");
        found[0]
    };
    (rect(0x0000_ff00), rect(0x0000_00ff))
}

#[test]
fn shifted_grid_never_overlaps_the_following_content() {
    // codex review of 4882fa5: a paint-only shift drew the grid 8.50 pt into the next
    // paragraph's table. The shifted grid must end at or above where the next content starts.
    let (grid, next) = grid_and_next([567, 1134, 850, 283], 10000);
    assert!(
        grid.3 <= next.1 + 0.01,
        "grid {grid:?} overlaps the next paragraph's table {next:?}"
    );
    // And the measured shift itself still applies.
    let (plain, _) = grid_and_next([0; 4], 10000);
    assert!((grid.0 - plain.0 - 5.67).abs() < 0.01 && (grid.1 - plain.1 - 8.50).abs() < 0.01);
}

#[test]
fn shifted_grid_may_cross_the_column_for_both_flow_values() {
    // #659 Hancom probe d1/d2 and the d2 flowWithText=0 / allowOverlap=0 variants: Hancom draws
    // the shifted grid past the column's right edge (535.75 / 564.05 pt against a 510.24 body
    // edge) whatever those attributes say, so a full-width grid shifts.
    let full = 42520; // the default body width from_markdown sets up, in HWPUNIT
    for flow in [false, true] {
        let (shifted, _) = grid_and_next_flow([567, 1134, 850, 283], full, flow);
        let (plain, _) = grid_and_next_flow([0; 4], full, flow);
        assert!(
            (shifted.0 - plain.0 - 5.67).abs() < 0.01 && (shifted.1 - plain.1 - 8.50).abs() < 0.01,
            "flowWithText {flow}: grid {shifted:?} must shift from {plain:?} by (5.67, 8.50)"
        );
    }
}

#[test]
fn grid_that_would_cross_the_paper_keeps_the_anchor_origin() {
    // The paper's right edge is a guard (no probe reached it): a grid that would cross it keeps
    // the anchor origin. The 425.20 pt grid at the 85.04 pt body left, offset 90 pt, would end
    // past the 595.28 pt paper.
    let full = 42520;
    let far = |out_margins: [u16; 4]| {
        let mut t = floating_table(2, 0, out_margins);
        t.placement.as_mut().unwrap().horz_offset = 9000;
        t.cells[0].width = HwpUnit(full);
        grid_origin(t)
    };
    assert_eq!(far([567, 1134, 850, 283]), far([0; 4]));
    assert_eq!(far([0; 4]), grid_origin(floating_table(2, 0, [0; 4])));
}

#[test]
fn paper_guard_uses_the_drawn_width_from_later_rows() {
    // codex review of 56bc40e: row 0 is one narrow merged cell, but row 1's span-1 cells make the
    // drawn grid full-width. The guard must use that width. Without an offset the 425.20 pt grid
    // shifts past the column (#659); with a 90 pt offset the drawn width would cross the paper
    // (row 0's 100 pt would not), so the grid keeps the anchor origin.
    let layout = |out_margins: [u16; 4], horz_offset: i32| {
        let mut table = floating_table(2, 0, out_margins);
        table.placement.as_mut().unwrap().horz_offset = horz_offset;
        table.rows = 2;
        table.cols = 2;
        table.row_cell_counts = vec![1, 2];
        let base = table.cells[0].clone();
        let cell = |col: u16, row: u16, col_span: u16, width: i32| Cell {
            col,
            row,
            col_span,
            width: HwpUnit(width),
            ..base.clone()
        };
        // Row 0's stored width is narrow; row 1 defines 2 x 21260 = the full 42520 body.
        table.cells = vec![
            cell(0, 0, 2, 10000),
            cell(0, 1, 1, 21260),
            cell(1, 1, 1, 21260),
        ];
        let mut doc = hwp_convert::from_markdown("x\n");
        doc.header.border_fills.push(hwp_model::BorderFill {
            bg_color: Some(0x0000_ff00),
            ..Default::default()
        });
        let red = BorderFillId(doc.header.border_fills.len() as u16);
        for c in &mut table.cells {
            c.border_fill = red;
        }
        let para = &mut doc.sections[0].paragraphs[0];
        para.controls.push(Control::Table(table));
        para.line_segs.clear();
        let mut store = FontStore::new_isolated();
        let mut warns = RenderIssueAccumulator::new();
        let list = hwp_render::layout::layout_document(&doc, &mut store, &mut warns);
        let mut b = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY);
        for item in &list.pages[0].items {
            if let Item::Rect { x, y, w, fill, .. } = item
                && *fill == 0x0000_ff00
            {
                b = (b.0.min(*x), b.1.min(*y), b.2.max(*x + *w));
            }
        }
        assert!(b.0.is_finite(), "the grid must draw");
        b
    };
    assert_eq!(layout([567, 1134, 850, 283], 9000), layout([0; 4], 9000));
    let (shifted, plain) = (layout([567, 1134, 850, 283], 0), layout([0; 4], 0));
    assert!(
        (shifted.0 - plain.0 - 5.67).abs() < 0.01 && (shifted.2 - plain.2 - 5.67).abs() < 0.01,
        "grid {shifted:?} must shift its drawn width from {plain:?} by 5.67"
    );
}

#[test]
fn unmeasured_alignments_keep_the_anchor_origin() {
    let margins = [567, 1134, 850, 283];
    // Only top/left alignment is measured: a centered or bottom-aligned table keeps its origin.
    for (va, ha) in [(1, 0), (0, 1), (2, 2)] {
        assert_eq!(
            grid_origin(aligned_table(2, 0, margins, va, ha)),
            grid_origin(aligned_table(2, 0, [0; 4], va, ha)),
            "vert_align {va} horz_align {ha}"
        );
    }
}

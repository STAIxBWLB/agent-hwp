//! Rule B14 (#655): a floating table anchored to the paragraph and column with zero offsets draws
//! its grid one left and one top outer margin in from the anchor, as Hancom does. Every number is
//! page geometry or a stored table field, so no assertion depends on a host font.

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
fn unmeasured_relations_and_offsets_keep_the_anchor_origin() {
    let margins = [567, 1134, 850, 283];
    // hrel PARA (3) is unmeasured, so the margins do not move it.
    assert_eq!(
        grid_origin(floating_table(3, 0, margins)),
        grid_origin(floating_table(3, 0, [0; 4]))
    );
    // A non-zero vertical offset is unmeasured, so the margins do not move it either.
    assert_eq!(
        grid_origin(floating_table(2, 1000, margins)),
        grid_origin(floating_table(2, 1000, [0; 4]))
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

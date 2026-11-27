//! Opt-in report blocks for public-sector reports (#620): numbered section-title strips and a
//! front table of contents whose page numbers are backfilled from a layout pass.
//!
//! Both are IR transforms applied after import, so the default `report` and `plan` output is
//! unchanged unless a caller asks for them. Neither needs the renderer: the TOC is built with
//! placeholder page numbers, and `hwp-cli` runs the layout pass and calls [`fill_toc`]
//! (invariant 1 keeps `hwp-convert` and `hwp-render` independent).
//!
//! # Section strip
//!
//! A level-3 heading `N. Title` becomes a 1x3 table: a shaded number cell (`#193AAA`, white
//! bold), a 2 mm spacer and a title cell with a bottom border. The anchor paragraph keeps the
//! `개요 3` style and the title cell paragraph carries it too, so outline-aware readers still
//! see a heading. Markdown export recognizes the strip ([`strip_heading`]) and writes it back
//! as `### N. Title`.
//!
//! # Table of contents
//!
//! [`insert_toc`] puts a two-column table (title | page) at the front of the first section,
//! one row per level-2 (chapter) and level-3 (section) heading; level 1 is the document title
//! and is not listed. A page break and a page-number restart
//! (`nwno`, page 1) on the first body paragraph, so body pages count from 1. The returned
//! [`TocPlan`] records, per row, where its heading paragraph ends up; the caller maps those
//! paragraphs to pages and passes the numbers to [`fill_toc`].

use hwp_model::{
    BorderFill, BorderFillId, BorderLine, Cell, CharShape, CharShapeId, Control, Document,
    GenericControl, HwpChar, HwpUnit, ParaShapeId, Paragraph, StyleId, Table, ctrl_char,
};

use crate::from_markdown::{BODY_WIDTH, CELL_VALIGN_CENTER, TABLE_BORDER_FILL};

/// The strip's number-cell fill (`#193AAA` as a COLORREF, 0x00BBGGRR).
const STRIP_FILL: u32 = 0x00AA_3A19;
/// White text in the number cell.
const WHITE: u32 = 0x00FF_FFFF;
/// Number and spacer cell widths: 12 mm and 2 mm in HWPUNIT (1 mm = 283.465 HWPUNIT).
const NUMBER_WIDTH: i32 = 3402;
const GAP_WIDTH: i32 = 567;
const STRIP_HEIGHT: i32 = 2268;
/// The placeholder a TOC page cell holds until [`fill_toc`] runs.
pub const TOC_PLACEHOLDER: &str = "0";
/// The TOC title paragraph text.
pub const TOC_TITLE: &str = "목차";

fn style_index(doc: &Document, name: &str) -> Option<u16> {
    doc.header
        .styles
        .iter()
        .position(|s| s.name == name)
        .and_then(|i| u16::try_from(i).ok())
}

/// The outline level a paragraph's style names (`개요 N`), if any.
pub fn heading_level(doc: &Document, para: &Paragraph) -> Option<u8> {
    doc.header
        .styles
        .get(usize::from(para.style.0))?
        .name
        .strip_prefix("개요 ")?
        .trim()
        .parse()
        .ok()
        .filter(|n| (1..=6).contains(n))
}

/// Splits `N. Title` into its parts: one or more ASCII digits, a dot, then a space and the
/// title. `1.1 Title`, `2.3. Title` and `2026.11 Title` are not of this form.
fn split_numbered(text: &str) -> Option<(&str, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (number, rest) = text.split_at(digits);
    let title = rest.strip_prefix(". ")?.trim_start();
    (!title.is_empty()).then_some((number, title))
}

fn text_paragraph(text: &str, char_shape: CharShapeId, style: StyleId) -> Paragraph {
    Paragraph {
        para_shape: ParaShapeId(0),
        style,
        chars: text.chars().map(HwpChar::Text).collect(),
        char_shape_runs: vec![(0, char_shape)],
        ..Paragraph::default()
    }
}

fn cell(col: u16, width: i32, border_fill: u16, paragraph: Paragraph) -> Cell {
    Cell {
        list_attr: CELL_VALIGN_CENTER,
        col,
        row: 0,
        col_span: 1,
        row_span: 1,
        width: HwpUnit(width),
        height: HwpUnit(STRIP_HEIGHT),
        margins: [141, 141, 141, 141],
        border_fill: BorderFillId(border_fill),
        header_tail: Vec::new(),
        paragraphs: vec![paragraph],
    }
}

/// A paragraph anchoring `table` as its only content, in the shape `from_markdown` uses.
fn table_paragraph(table: Table, style: StyleId) -> Paragraph {
    let mut payload = vec![0u8; 12];
    payload[..4].copy_from_slice(b" lbt");
    Paragraph {
        style,
        chars: vec![
            HwpChar::ExtCtrl {
                code: 11,
                ctrl_id: *b"tbl ",
                payload,
                ctrl_index: Some(0),
            },
            HwpChar::CharCtrl(ctrl_char::PARA_BREAK),
        ],
        char_shape_runs: vec![(0, CharShapeId(0))],
        controls: vec![Control::Table(table)],
        ..Paragraph::default()
    }
}

fn table(rows: u16, cols: u16, cells: Vec<Cell>) -> Table {
    // An explicit inline placement, as DocumentSpec tables carry: both writers then emit the
    // object properties from the model instead of inventing them, so the published file reads
    // back to this same table (compose's semantic self-check compares the two).
    let width = cells.iter().filter(|c| c.row == 0).map(|c| c.width.0).sum();
    let height = cells
        .iter()
        .filter(|c| c.col == 0)
        .map(|c| c.height.0)
        .sum();
    Table {
        common_data: Vec::new(),
        placement: Some(hwp_model::GsoPlacement {
            treat_as_char: true,
            affect_line_spacing: true,
            flow_with_text: true,
            hold_anchor: false,
            vert_rel_to: 2,
            horz_rel_to: 3,
            vert_align: 0,
            horz_align: 0,
            vert_offset: 0,
            horz_offset: 0,
            z_order: 0,
            width,
            height,
            out_margins: [0; 4],
        }),
        attr: 0,
        rows,
        cols,
        cell_spacing: 0,
        inner_margins: [141, 141, 141, 141],
        row_cell_counts: vec![cols; usize::from(rows)],
        border_fill: BorderFillId(1),
        table_tail: Vec::new(),
        caption: None,
        cells,
        extras: Vec::new(),
    }
}

/// Appends a border fill and returns its 1-based id.
fn push_border_fill(doc: &mut Document, fill: BorderFill) -> u16 {
    doc.header.border_fills.push(fill);
    u16::try_from(doc.header.border_fills.len()).expect("border fill count fits u16")
}

fn push_char_shape(doc: &mut Document, shape: CharShape) -> CharShapeId {
    doc.header.char_shapes.push(shape);
    CharShapeId(u16::try_from(doc.header.char_shapes.len() - 1).expect("char shape count"))
}

/// Header entries one document's strips share.
struct StripShapes {
    number_fill: u16,
    gap_fill: u16,
    title_fill: u16,
    number_text: CharShapeId,
    title_text: CharShapeId,
}

fn strip_shapes(doc: &mut Document, heading_shape: CharShapeId) -> StripShapes {
    let none = BorderLine::default();
    let line = BorderLine {
        line_type: 1,
        width: 1,
        color: 0,
    };
    let number_fill = push_border_fill(
        doc,
        BorderFill {
            sides: [none; 4],
            fill_type: 1,
            bg_color: Some(STRIP_FILL),
            ..BorderFill::default()
        },
    );
    let gap_fill = push_border_fill(
        doc,
        BorderFill {
            sides: [none; 4],
            ..BorderFill::default()
        },
    );
    let title_fill = push_border_fill(
        doc,
        BorderFill {
            sides: [none, none, none, line],
            ..BorderFill::default()
        },
    );
    let base = doc
        .header
        .char_shapes
        .get(usize::from(heading_shape.0))
        .cloned()
        .unwrap_or_default();
    let number_text = push_char_shape(
        doc,
        CharShape {
            text_color: WHITE,
            attr: base.attr | 1 << 1,
            ..base.clone()
        },
    );
    let title_text = push_char_shape(
        doc,
        CharShape {
            attr: base.attr | 1 << 1,
            ..base
        },
    );
    StripShapes {
        number_fill,
        gap_fill,
        title_fill,
        number_text,
        title_text,
    }
}

/// Replaces every level-3 heading of the form `N. Title` with a 1x3 section strip. Returns how
/// many headings became strips. Headings without a leading number, and headings in nested
/// lists, stay as they are.
pub fn apply_section_strips(doc: &mut Document) -> usize {
    let Some(h3) = style_index(doc, "개요 3") else {
        return 0;
    };
    let heading_shape = doc
        .header
        .styles
        .get(usize::from(h3))
        .map_or(CharShapeId(0), |s| s.char_shape);
    let mut shapes: Option<StripShapes> = None;
    let mut count = 0;
    for si in 0..doc.sections.len() {
        for pi in 0..doc.sections[si].paragraphs.len() {
            let para = &doc.sections[si].paragraphs[pi];
            if heading_level(doc, para) != Some(3)
                || !para.controls.iter().all(is_strip_neutral_control)
            {
                continue;
            }
            let text = para.own_text();
            let Some((number, title)) = split_numbered(text.trim()) else {
                continue;
            };
            let (number, title) = (number.to_string(), title.to_string());
            let s = shapes.get_or_insert_with(|| strip_shapes(doc, heading_shape));
            let style = StyleId(h3);
            let title_width = BODY_WIDTH - NUMBER_WIDTH - GAP_WIDTH;
            let cells = vec![
                cell(
                    0,
                    NUMBER_WIDTH,
                    s.number_fill,
                    text_paragraph(&number, s.number_text, StyleId(0)),
                ),
                cell(
                    1,
                    GAP_WIDTH,
                    s.gap_fill,
                    text_paragraph("", CharShapeId(0), StyleId(0)),
                ),
                cell(
                    2,
                    title_width,
                    s.title_fill,
                    text_paragraph(&title, s.title_text, style),
                ),
            ];
            let mut strip = table_paragraph(table(1, 3, cells), style);
            // A first-block heading carries the section/column definitions: keep them first.
            let old = &mut doc.sections[si].paragraphs[pi];
            strip.header.break_type = old.header.break_type;
            move_leading_definitions(old, &mut strip);
            doc.sections[si].paragraphs[pi] = strip;
            count += 1;
        }
    }
    count
}

fn cell_text(cell: &Cell) -> String {
    cell.paragraphs
        .iter()
        .map(Paragraph::own_text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `(number, title)` when `table` is a section strip anchored by a `개요 3` paragraph: one row,
/// three cells, a numeric first cell, an empty spacer and a non-empty title. Markdown export
/// writes such a table as `### N. Title`.
pub fn strip_heading(doc: &Document, para: &Paragraph, table: &Table) -> Option<(String, String)> {
    if heading_level(doc, para) != Some(3) || table.rows != 1 || table.cols != 3 {
        return None;
    }
    let mut cells: Vec<&Cell> = table.cells.iter().collect();
    cells.sort_by_key(|c| c.col);
    let [number, gap, title] = cells.as_slice() else {
        return None;
    };
    let number = cell_text(number).trim().to_string();
    let title = cell_text(title).trim().to_string();
    (!number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && cell_text(gap).trim().is_empty()
        && !title.is_empty())
    .then_some((number, title))
}

/// One TOC row: the heading it lists and where that heading paragraph is after insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub level: u8,
    pub title: String,
    /// Section and top-level paragraph index of the heading, in the document after
    /// [`insert_toc`].
    pub section: usize,
    pub paragraph: usize,
}

/// What [`insert_toc`] built: its rows, in document order, and the TOC table's location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocPlan {
    pub entries: Vec<TocEntry>,
    /// Top-level paragraph index (section 0) of the paragraph anchoring the TOC table.
    pub table_paragraph: usize,
    /// Top-level paragraph index (section 0) where the body starts: the first paragraph with
    /// content after the TOC, which carries the page break and the page-number restart.
    pub body_start: usize,
    /// How many paragraphs [`insert_toc`] inserted (the TOC title and table). The section's
    /// paragraph count grows by this minus one when the old first paragraph held only the
    /// definitions and was dropped.
    pub front_paragraphs: usize,
}

/// The text a heading lists in the TOC: its own text, or a strip's `N. Title`.
fn heading_title(doc: &Document, para: &Paragraph) -> String {
    for control in &para.controls {
        if let Control::Table(t) = control
            && let Some((n, title)) = strip_heading(doc, para, t)
        {
            return format!("{n}. {title}");
        }
    }
    para.own_text().trim().to_string()
}

fn new_number_control() -> Control {
    let mut data = Vec::with_capacity(6);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    Control::Generic(GenericControl {
        ctrl_id: *b"nwno",
        data,
        paragraph_lists: Vec::new(),
        extras: Vec::new(),
        raw_children: Vec::new(),
        gso_shapes: Vec::new(),
        equation: None,
        column_def: None,
        caption: None,
        hwpx_raw_xml: None,
        chart: None,
        container_box: None,
    })
}

/// Inserts `ctrl` as an extended control at the start of `para`'s text (after any leading
/// section/column definitions, which must stay first), keeping `ctrl_index` and run positions
/// consistent.
fn prepend_page_restart(para: &mut Paragraph) {
    let lead = para
        .controls
        .iter()
        .take_while(|c| matches!(c, Control::SectionDef(_)) || c.ctrl_id() == *b"cold")
        .count();
    // Right after the lead-th definition character, or at the very start when there is none.
    // Never before a later extended control (a footnote or hyperlink after the text).
    let char_at = match lead.checked_sub(1) {
        None => 0,
        Some(last) => para
            .chars
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, HwpChar::ExtCtrl { .. }))
            .nth(last)
            .map_or(para.chars.len(), |(i, _)| i + 1),
    };
    let mut payload = vec![0u8; 12];
    payload[..4].copy_from_slice(b"onwn");
    let marker = HwpChar::ExtCtrl {
        code: ctrl_char::PAGE_CONTROL,
        ctrl_id: *b"nwno",
        payload,
        ctrl_index: None,
    };
    let at_wchar: u32 = para.chars[..char_at].iter().map(HwpChar::wchar_width).sum();
    let width = marker.wchar_width();
    para.chars.insert(char_at, marker);
    para.controls.insert(lead, new_number_control());
    let mut next = 0u32;
    for ch in &mut para.chars {
        if let HwpChar::ExtCtrl { ctrl_index, .. } = ch {
            *ctrl_index = Some(next);
            next += 1;
        }
    }
    for (pos, _) in para.char_shape_runs.iter_mut() {
        if *pos > at_wchar {
            *pos += width;
        }
    }
    if para.char_shape_runs.is_empty() {
        para.char_shape_runs.push((0, CharShapeId(0)));
    }
}

/// Moves the leading section/column definition controls (and their characters) of `from` to the
/// front of `to`, keeping both paragraphs' control links and run positions consistent.
fn move_leading_definitions(from: &mut Paragraph, to: &mut Paragraph) {
    let lead = from
        .controls
        .iter()
        .take_while(|c| matches!(c, Control::SectionDef(_)) || c.ctrl_id() == *b"cold")
        .count();
    if lead == 0 {
        return;
    }
    let controls: Vec<Control> = from.controls.drain(..lead).collect();
    let mut moved = Vec::new();
    let mut taken = 0;
    let mut kept = Vec::with_capacity(from.chars.len());
    for ch in from.chars.drain(..) {
        if taken < lead && matches!(ch, HwpChar::ExtCtrl { .. }) {
            moved.push(ch);
            taken += 1;
        } else {
            kept.push(ch);
        }
    }
    from.chars = kept;
    let width: u32 = moved.iter().map(HwpChar::wchar_width).sum();
    for (pos, _) in from.char_shape_runs.iter_mut() {
        *pos = pos.saturating_sub(width);
    }
    for (pos, _) in to.char_shape_runs.iter_mut().skip(1) {
        *pos += width;
    }
    to.chars.splice(0..0, moved);
    to.controls.splice(0..0, controls);
    for para in [from, to] {
        let mut next = 0u32;
        for ch in &mut para.chars {
            if let HwpChar::ExtCtrl { ctrl_index, .. } = ch {
                *ctrl_index = Some(next);
                next += 1;
            }
        }
    }
}

/// Builds a front TOC listing every level-2 and level-3 heading (strips included) of the first
/// section and inserts it at the front, followed by a page break and a page-number restart on
/// the first body paragraph. The section and column definitions move to the TOC's first
/// paragraph, because they must stay in the section's first paragraph. Returns `None`, leaving the document
/// untouched, when there is no heading to list.
pub fn insert_toc(doc: &mut Document) -> Option<TocPlan> {
    let section = doc.sections.first()?;
    let listed: Vec<(usize, u8, String)> = section
        .paragraphs
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let level = heading_level(doc, p)?;
            (2..=3)
                .contains(&level)
                .then(|| (i, level, heading_title(doc, p)))
        })
        .filter(|(_, _, t)| !t.is_empty())
        .collect();
    if listed.is_empty() || u16::try_from(listed.len()).is_err() {
        // A TOC table holds at most u16::MAX rows (HWP5 TABLE row count).
        return None;
    }
    let title_shape = style_index(doc, "개요 1")
        .and_then(|i| doc.header.styles.get(usize::from(i)))
        .map_or(CharShapeId(0), |s| s.char_shape);
    let indent = "    ";
    let page_width = 4252;
    let cells: Vec<Cell> = listed
        .iter()
        .enumerate()
        .flat_map(|(row, (_, level, title))| {
            let label = if *level == 3 {
                format!("{indent}{title}")
            } else {
                title.clone()
            };
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            let mut left = cell(
                0,
                BODY_WIDTH - page_width,
                TABLE_BORDER_FILL,
                text_paragraph(&label, CharShapeId(0), StyleId(0)),
            );
            let mut right = cell(
                1,
                page_width,
                TABLE_BORDER_FILL,
                text_paragraph(TOC_PLACEHOLDER, CharShapeId(0), StyleId(0)),
            );
            left.row = row;
            right.row = row;
            left.height = HwpUnit(1700);
            right.height = HwpUnit(1700);
            [left, right]
        })
        .collect();
    let rows = u16::try_from(listed.len()).ok()?;
    let toc_table = table(rows, 2, cells);
    let mut toc_title = text_paragraph(TOC_TITLE, title_shape, StyleId(0));
    toc_title
        .chars
        .push(HwpChar::CharCtrl(ctrl_char::PARA_BREAK));
    move_leading_definitions(&mut doc.sections[0].paragraphs[0], &mut toc_title);
    // The section/column break bits (0x03) belong to the paragraph that carries the
    // definitions (from_markdown sets them there): move them with the definitions, so the old
    // first paragraph never ends up 0x07 (compat rule J1 records body page breaks as 0x04).
    let moved_breaks = doc.sections[0].paragraphs[0].header.break_type & 0x03;
    if toc_title
        .controls
        .iter()
        .any(|c| matches!(c, Control::SectionDef(_)))
    {
        doc.sections[0].paragraphs[0].header.break_type &= !0x03;
        toc_title.header.break_type |= moved_breaks;
    }
    // A first paragraph that held only the definitions (DocumentSpec output) is now empty:
    // drop it, so the body starts at a paragraph with content of its own.
    let dropped_first = {
        let first = &doc.sections[0].paragraphs[0];
        first.controls.is_empty()
            && first
                .chars
                .iter()
                .all(|c| matches!(c, HwpChar::CharCtrl(ctrl_char::PARA_BREAK)))
            && doc.sections[0].paragraphs.len() > 1
    };
    if dropped_first {
        doc.sections[0].paragraphs.remove(0);
    }
    let toc_para = table_paragraph(toc_table, StyleId(0));
    let front = [toc_title, toc_para];
    let front_paragraphs = front.len();

    let insert_at = 0;
    let section = &mut doc.sections[0];
    section.paragraphs.splice(insert_at..insert_at, front);
    // The body starts at the first paragraph the layout gives a segment, i.e. one with
    // content. A DocumentSpec anchor that keeps only header/footer/page-number controls has
    // none (the renderer emits no row for it), so it stays before the break with the TOC.
    let first_body = insert_at + front_paragraphs;
    let body_start = (first_body..section.paragraphs.len())
        .find(|&i| paragraph_has_content(&section.paragraphs[i]))
        .unwrap_or(first_body);
    if let Some(body) = section.paragraphs.get_mut(body_start) {
        // Page break before the body (0x04), then restart page numbering at 1 there.
        body.header.break_type |= 0x04;
        prepend_page_restart(body);
    }
    let shift = |i: usize| i + front_paragraphs - usize::from(dropped_first);
    Some(TocPlan {
        entries: listed
            .into_iter()
            .map(|(i, level, title)| TocEntry {
                level,
                title,
                section: 0,
                paragraph: shift(i),
            })
            .collect(),
        table_paragraph: insert_at + 1,
        body_start,
        front_paragraphs,
    })
}

/// Controls a strip paragraph may hold besides its table without changing what it shows: the
/// section/column definitions of a first block, its page-number position (`--preset report`)
/// and the TOC's page-number restart.
pub fn is_strip_neutral_control(control: &Control) -> bool {
    matches!(control, Control::SectionDef(_))
        || matches!(&control.ctrl_id(), b"cold" | b"nwno" | b"pgnp")
}

/// Whether the layout gives `para` a segment of its own: any text, or any control other than
/// the invisible section-level ones (definitions, header/footer, page numbering).
fn paragraph_has_content(para: &Paragraph) -> bool {
    para.chars.iter().any(|c| matches!(c, HwpChar::Text(_)))
        || para.controls.iter().any(|c| match c {
            Control::SectionDef(_) => false,
            other => !matches!(
                &other.ctrl_id(),
                b"cold" | b"head" | b"foot" | b"nwno" | b"pgnp" | b"pghd" | b"pgct" | b"atno"
            ),
        })
}

/// Writes `pages[i]` into row `i`'s page cell. `pages.len()` must equal the plan's entry count.
pub fn fill_toc(doc: &mut Document, plan: &TocPlan, pages: &[u32]) -> Result<(), String> {
    if pages.len() != plan.entries.len() {
        return Err(format!(
            "목차 쪽 번호 {}개가 목차 항목 {}개와 맞지 않습니다",
            pages.len(),
            plan.entries.len()
        ));
    }
    let para = doc
        .sections
        .first_mut()
        .and_then(|s| s.paragraphs.get_mut(plan.table_paragraph))
        .ok_or("목차 표 문단이 없습니다")?;
    let Some(Control::Table(table)) = para.controls.first_mut() else {
        return Err("목차 표 문단에 표가 없습니다".into());
    };
    for cell in table.cells.iter_mut().filter(|c| c.col == 1) {
        let page = pages
            .get(usize::from(cell.row))
            .ok_or("쪽 번호가 없는 목차 행이 있습니다")?;
        let p = cell
            .paragraphs
            .first_mut()
            .ok_or("비어 있는 목차 칸이 있습니다")?;
        let shape = p.char_shape_runs.first().map_or(CharShapeId(0), |r| r.1);
        *p = text_paragraph(&page.to_string(), shape, p.style);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(md: &str) -> Document {
        crate::from_markdown(md)
    }

    /// Only `N. Title` (digits, ". ", title) is a numbered heading; decimal, multi-level and
    /// date-like numbers are not split.
    #[test]
    fn only_single_number_dot_space_headings_split() {
        assert_eq!(split_numbered("1. 추진 배경"), Some(("1", "추진 배경")));
        assert_eq!(split_numbered("12.  계획"), Some(("12", "계획")));
        for text in [
            "1.1 추진 배경",
            "2.3. 제목",
            "2026.11 실적",
            "1.",
            "1.제목",
            ". 제목",
        ] {
            assert_eq!(split_numbered(text), None, "{text}");
        }
    }

    /// The section/column break bits move with the definitions to the TOC title, and the
    /// body paragraph is a plain page break (0x04), never 0x07.
    #[test]
    fn toc_moves_the_section_break_bits_with_the_definitions() {
        let mut d = doc("## Ⅰ. 개요\n\n본문\n\n## Ⅱ. 계획\n");
        let first_bits = d.sections[0].paragraphs[0].header.break_type & 0x03;
        let plan = insert_toc(&mut d).expect("headings exist");
        let paras = &d.sections[0].paragraphs;
        assert_eq!(paras[0].header.break_type & 0x03, first_bits);
        assert_eq!(paras[plan.body_start].header.break_type, 0x04);
        for (i, p) in paras.iter().enumerate().skip(1) {
            assert_eq!(p.header.break_type & 0x03, 0, "paragraph {i}");
        }
    }

    /// With no leading definitions the restart goes at the very start of the paragraph, never
    /// next to a later extended control such as a footnote after the text.
    #[test]
    fn the_page_restart_never_lands_mid_paragraph() {
        let d = doc("첫 문단\n\n본문 앞[^1] 뒤\n\n[^1]: 각주\n");
        let mut para = d.sections[0]
            .paragraphs
            .iter()
            .find(|p| {
                p.controls.iter().all(|c| !is_strip_neutral_control(c)) && !p.controls.is_empty()
            })
            .expect("a body paragraph with a footnote")
            .clone();
        prepend_page_restart(&mut para);
        assert!(
            matches!(para.chars[0], HwpChar::ExtCtrl { ctrl_id, .. } if ctrl_id == *b"nwno"),
            "{:?}",
            &para.chars[..3]
        );
        assert_eq!(para.controls[0].ctrl_id(), *b"nwno");
    }

    #[test]
    fn numbered_level_three_headings_become_strips() {
        let mut d = doc("# 보고서\n\n### 1. 추진 배경\n\n본문\n\n### 개요 없는 제목\n");
        assert_eq!(apply_section_strips(&mut d), 1);
        let strips: Vec<(String, String)> = d.sections[0]
            .paragraphs
            .iter()
            .filter_map(|p| match p.controls.first() {
                Some(Control::Table(t)) => strip_heading(&d, p, t),
                _ => None,
            })
            .collect();
        assert_eq!(strips, [("1".to_string(), "추진 배경".to_string())]);
        let Some(Control::Table(t)) = d.sections[0]
            .paragraphs
            .iter()
            .find_map(|p| p.controls.iter().find(|c| matches!(c, Control::Table(_))))
        else {
            panic!("no strip table");
        };
        assert_eq!((t.rows, t.cols, t.cells.len()), (1, 3, 3));
        let fill = &d.header.border_fills[usize::from(t.cells[0].border_fill.0) - 1];
        assert_eq!(fill.bg_color, Some(STRIP_FILL));
        let title_fill = &d.header.border_fills[usize::from(t.cells[2].border_fill.0) - 1];
        assert_eq!(title_fill.sides[3].line_type, 1);
        assert_eq!(title_fill.sides[0].line_type, 0);
    }

    #[test]
    fn strips_are_off_by_default() {
        let d = doc("### 1. 추진 배경\n");
        assert!(
            d.sections[0]
                .paragraphs
                .iter()
                .all(|p| !p.controls.iter().any(|c| matches!(c, Control::Table(_))))
        );
    }

    #[test]
    fn toc_lists_level_two_and_three_headings_and_restarts_page_numbers() {
        let mut d = doc("# 제목\n\n## Ⅰ. 개요\n\n본문\n\n### 1. 세부\n\n## Ⅱ. 계획\n\n본문\n");
        let before = d.sections[0].paragraphs.len();
        let plan = insert_toc(&mut d).expect("headings exist");
        assert_eq!(
            d.sections[0].paragraphs.len(),
            before + plan.front_paragraphs
        );
        let titles: Vec<&str> = plan.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles.len(), 3, "{titles:?}");
        assert!(
            !titles.contains(&"제목"),
            "the level-1 title is not listed: {titles:?}"
        );
        // The section definition moved to the section's (new) first paragraph.
        let first = &d.sections[0].paragraphs[0];
        assert!(
            first
                .controls
                .iter()
                .any(|c| matches!(c, Control::SectionDef(_))),
            "section definition left the first paragraph"
        );
        assert!(d.sections[0].paragraphs[1..].iter().all(|p| {
            !p.controls
                .iter()
                .any(|c| matches!(c, Control::SectionDef(_)))
        }));
        for e in &plan.entries {
            let p = &d.sections[0].paragraphs[e.paragraph];
            assert_eq!(heading_title(&d, p), e.title);
        }
        let body = &d.sections[0].paragraphs[plan.table_paragraph + 1];
        assert_eq!(body.header.break_type & 0x04, 0x04);
        assert!(body.controls.iter().any(|c| c.ctrl_id() == *b"nwno"));
        fill_toc(&mut d, &plan, &[1, 1, 2]).unwrap();
        let Some(Control::Table(t)) = d.sections[0].paragraphs[plan.table_paragraph]
            .controls
            .first()
        else {
            panic!("no TOC table");
        };
        let pages: Vec<String> = t
            .cells
            .iter()
            .filter(|c| c.col == 1)
            .map(cell_text)
            .collect();
        assert_eq!(pages, ["1", "1", "2"]);
        assert!(fill_toc(&mut d, &plan, &[1]).is_err());
    }

    #[test]
    fn no_headings_means_no_toc() {
        let mut d = doc("본문만 있다\n");
        let before = d.clone();
        assert!(insert_toc(&mut d).is_none());
        assert_eq!(d, before);
    }
}

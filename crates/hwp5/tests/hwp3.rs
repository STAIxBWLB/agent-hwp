//! Synthetic HWP 3.00 documents exercising the Part I binary layout.

use std::io::Write;

use hwp_model::{BinRef, Control, Document};

#[path = "support/hwp3.rs"]
mod fixtures;
use fixtures::{PNG, prefix, rich_document, shapes, text_list, word_at};

fn check_objects(document: &Document) {
    assert!(document.plain_text().contains("한글伽"));
    let table = document.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|control| match control {
            Control::Table(table) => Some(table),
            _ => None,
        })
        .unwrap();
    assert_eq!((table.rows, table.cols), (2, 2));
    assert_eq!(table.cells[0].col_span, 2);
    assert_eq!((table.cells[2].row, table.cells[2].col), (1, 1));
    let picture = document.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|control| match control {
            Control::Picture(picture) => Some(picture),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&picture.bin_ref, BinRef::ItemRef(_)));
    assert_eq!(document.resolve_bin(&picture.bin_ref), Some(PNG));
}

#[test]
fn imports_johab_merged_cells_and_embedded_picture() {
    let original = rich_document();
    let result = hwp5::hwp3::read_bytes(&original).unwrap();
    assert!(result.warnings.is_empty());
    assert_eq!(result.document.metadata.title.as_deref(), Some("한글"));
    check_objects(&result.document);

    let output = std::env::temp_dir().join(format!("ahwp-hwp3-{}.hwpx", std::process::id()));
    let report = hwpx::write_document_with_report(&result.document, &output).unwrap();
    assert!(report.preservation.is_lossless(), "{report:?}");
    check_objects(&hwpx::read_document(&output).unwrap().document);
    if let Some(folder) = std::env::var_os("HWP3_REVIEW_DIR") {
        let folder = std::path::PathBuf::from(folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("synthetic-v300.hwp"), original).unwrap();
        std::fs::copy(&output, folder.join("synthetic-v300-converted.hwpx")).unwrap();
    }
    std::fs::remove_file(output).unwrap();
}

#[test]
fn reads_both_supported_compression_framings() {
    let mut payload = Vec::new();
    shapes(&mut payload);
    text_list(&mut payload, &[0xd065, 0x8b69]);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&payload).unwrap();
    let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    deflate.write_all(&payload).unwrap();
    for compressed in [gzip.finish().unwrap(), deflate.finish().unwrap()] {
        let mut bytes = prefix(true);
        bytes.extend(compressed);
        assert!(
            hwp5::hwp3::read_bytes(&bytes)
                .unwrap()
                .document
                .plain_text()
                .contains("한글")
        );
    }
}

#[test]
fn protected_document_refuses_before_the_body() {
    let mut bytes = prefix(false);
    word_at(&mut bytes, 30 + 96, 1);
    bytes.truncate(30 + 128);
    assert!(matches!(
        hwp5::hwp3::read_bytes(&bytes),
        Err(hwp5::Hwp5Error::LegacyPasswordProtected)
    ));
}

#[test]
fn truncated_records_and_invalid_nested_grids_fail_closed() {
    let bytes = rich_document();
    for length in [29, 30, 157, 1166, 1200, 1300, 1400, 2000, bytes.len() - 4] {
        assert!(
            hwp5::hwp3::read_bytes(&bytes[..length]).is_err(),
            "prefix {length}"
        );
    }
    let mut overlap = bytes.clone();
    let table_cells = 30 + 128 + 1008 + 16 + 230 + 3 * 2 + 8 + 84;
    word_at(&mut overlap, table_cells + 27 + 6, 0);
    assert!(hwp5::hwp3::read_bytes(&overlap).is_err());
}

#[test]
fn unsupported_embedded_picture_keeps_the_document_readable() {
    let mut bytes = rich_document();
    let signature = bytes.len() - 8 - PNG.len();
    bytes[signature..signature + 4].copy_from_slice(b"PCX!");
    let read = hwp5::hwp3::read_bytes(&bytes);
    assert!(
        read.is_ok(),
        "unsupported image blocked import: {:?}",
        read.as_ref().err()
    );
    let read = read.unwrap();
    assert!(read.document.plain_text().contains("한글伽"));
    assert_eq!(read.warnings.len(), 1);
    let picture = read.document.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|control| match control {
            Control::Picture(picture) => Some(picture),
            _ => None,
        })
        .unwrap();
    assert!(read.document.resolve_bin(&picture.bin_ref).is_none());

    bytes[signature - 32] = b'X';
    assert!(hwp5::hwp3::read_bytes(&bytes).is_err());
}

#[test]
fn fixed_line_and_paragraph_spacing_keep_their_lengths() {
    let mut bytes = prefix(false);
    shapes(&mut bytes);
    let paragraph_header = bytes.len();
    text_list(&mut bytes, &[0xd065]);
    // The 43-byte paragraph header precedes its shape. A set high bit
    // marks fixed line spacing in legacy length units.
    let para_shape = paragraph_header + 43;
    word_at(&mut bytes, para_shape + 6, 0x8000 | 400);
    word_at(&mut bytes, para_shape + 8, 75);

    let read = hwp5::hwp3::read_bytes(&bytes).unwrap();
    let shape = &read.document.header.para_shapes[0];
    assert_eq!(shape.line_spacing_type, 1);
    assert_eq!(shape.line_spacing, 3200);
    assert_eq!(shape.line_spacing_old, 3200);
    assert_eq!(shape.spacing_bottom, 600);

    let output =
        std::env::temp_dir().join(format!("ahwp-hwp3-spacing-{}.hwpx", std::process::id()));
    hwpx::write_document(&read.document, &output).unwrap();
    let header = hwpx::package::HwpxPackage::open(&output)
        .unwrap()
        .read_entry_string("Contents/header.xml")
        .unwrap();
    std::fs::remove_file(output).unwrap();
    assert!(header.contains(r#"<hh:lineSpacing type="FIXED" value="1600" unit="HWPUNIT"/>"#));
    assert!(header.contains(r#"<hc:next value="300" unit="HWPUNIT"/>"#));
}

#[test]
fn keeps_legacy_hyphen_and_spaces_as_controls() {
    use hwp_model::{HwpChar, ctrl_char};

    let mut bytes = prefix(false);
    shapes(&mut bytes);
    text_list(&mut bytes, &[24, 0, 24, 30, 30, 31, 31]);
    let read = hwp5::hwp3::read_bytes(&bytes).unwrap();
    assert_eq!(
        read.document.sections[0].paragraphs[0].chars[1..],
        [
            HwpChar::CharCtrl(ctrl_char::HYPHEN),
            HwpChar::CharCtrl(ctrl_char::NB_SPACE),
            HwpChar::CharCtrl(ctrl_char::FW_SPACE),
        ]
    );
}

#[test]
fn empty_paragraph_keeps_its_representative_character_shape() {
    use hwp_model::CharShapeId;

    let mut bytes = prefix(false);
    shapes(&mut bytes);
    text_list(&mut bytes, &[0xd065]);
    bytes.truncate(bytes.len() - 43); // Replace the list terminator with another paragraph.
    let empty_header = bytes.len();
    text_list(&mut bytes, &[]);
    // The representative character shape starts after the 12-byte header prefix.
    word_at(&mut bytes, empty_header + 12, 500);

    let read = hwp5::hwp3::read_bytes(&bytes).unwrap();
    let paragraphs = &read.document.sections[0].paragraphs;
    assert!(paragraphs[1].chars.is_empty());
    assert_eq!(paragraphs[1].char_shape_runs, [(0, CharShapeId(1))]);
    assert_eq!(read.document.header.char_shapes[1].base_size, 2000);

    let output = std::env::temp_dir().join(format!("ahwp-hwp3-empty-{}.hwpx", std::process::id()));
    hwpx::write_document(&read.document, &output).unwrap();
    let reread = hwpx::read_document(&output).unwrap();
    std::fs::remove_file(output).unwrap();
    let empty = reread.document.sections[0].paragraphs.last().unwrap();
    assert!(empty.chars.is_empty());
    assert_eq!(empty.char_shape_runs, [(0, CharShapeId(1))]);
}

#[test]
fn section_definition_shares_the_first_content_paragraph() {
    use hwp_model::HwpChar;

    let read = hwp5::hwp3::read_bytes(&rich_document()).unwrap();
    let paragraphs = &read.document.sections[0].paragraphs;
    assert_eq!(paragraphs.len(), 1);
    let paragraph = &paragraphs[0];
    assert!(matches!(paragraph.controls[0], Control::SectionDef(_)));
    let indices: Vec<_> = paragraph
        .chars
        .iter()
        .filter_map(|ch| match ch {
            HwpChar::ExtCtrl { ctrl_index, .. } => *ctrl_index,
            _ => None,
        })
        .collect();
    assert_eq!(indices, [0, 1, 2]);
    check_objects(&read.document);
}

#[test]
fn synthesized_empty_cell_has_a_character_shape_and_inline_placement() {
    use hwp_model::CharShapeId;

    let bytes = fixtures::rich_document_with_cell_text(&[]);
    let read = hwp5::hwp3::read_bytes(&bytes).unwrap();
    let table = read.document.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|c| match c {
            Control::Table(table) => Some(table),
            _ => None,
        })
        .unwrap();
    let empty = &table.cells[0].paragraphs[0];
    assert!(empty.chars.is_empty());
    assert_eq!(empty.char_shape_runs, [(0, CharShapeId(0))]);
    let placement = table.placement.as_ref().unwrap();
    assert!(placement.treat_as_char);
    assert_eq!((placement.vert_rel_to, placement.horz_rel_to), (2, 3));
}

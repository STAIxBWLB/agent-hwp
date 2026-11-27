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

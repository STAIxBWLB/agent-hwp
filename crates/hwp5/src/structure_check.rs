//! Record-tree structure rules the IR reader tolerates but Hangul rejects.
//!
//! Genuine Hangul-saved streams write a control's CTRL_DATA as the first child of its
//! CTRL_HEADER (observed in the #610 genuine form and the local ground-truth corpus). A
//! CTRL_DATA found later among the children, for example after a table's
//! last cell paragraph, is detached from its control: the reader still parses the file but
//! Hangul reports corruption (#610).

use std::path::Path;

use crate::record::{RecordNode, ScanMode, scan_stream, tag};

/// Per-section decompressed size cap for this scan, the same 64 MiB per-stream cap the
/// bounded (password and certification) read paths use. A larger section is skipped.
pub const MAX_SCAN_STREAM_BYTES: u64 = 64 * 1024 * 1024;

/// Returns one message per detached CTRL_DATA record in the document's `/BodyText` sections.
/// Protected and distribution documents, sections over [`MAX_SCAN_STREAM_BYTES`] and
/// streams that cannot be read or scanned are skipped here; the regular reader reports those.
pub fn detached_ctrl_data(path: &Path) -> Vec<String> {
    detached_ctrl_data_counts(path)
        .into_iter()
        .map(|(stream, count)| {
            format!(
                "{stream}: CTRL_DATA {count}개가 소유 CTRL_HEADER 바로 뒤에 있지 않습니다 \
                 (한글이 손상 파일로 판정). 이 파일을 만든 ahwp를 최신 버전으로 올린 뒤 \
                 원본 파일에서 같은 편집을 다시 실행하거나, 출력을 .hwpx로 저장하세요"
            )
        })
        .collect()
}

/// `(stream path, detached CTRL_DATA count)` for each `/BodyText` section that has any.
/// Callers build their own message so each names the next step that fits its context.
pub fn detached_ctrl_data_counts(path: &Path) -> Vec<(String, usize)> {
    let Ok(mut container) = crate::Hwp5Container::open(path) else {
        return Vec::new();
    };
    // The same gates the reader applies before it touches a body stream: a protected
    // (encrypted, certificate, DRM, signed) document is never decompressed here, and a
    // distribution document keeps its body under /ViewText, so its /BodyText stub is a decoy.
    if container.check_body_readable().is_err() || container.file_header().is_distribution() {
        return Vec::new();
    }
    let sections = container.body_sections();
    let mut problems = Vec::new();
    for stream in sections {
        // Bounded: a small raw-DEFLATE stream can expand without limit, and this scan must
        // not do more work than validation did before it existed.
        let Ok(bytes) = container.read_record_stream_bounded(&stream, MAX_SCAN_STREAM_BYTES) else {
            continue;
        };
        let Ok(scan) = scan_stream(&bytes, ScanMode::Tolerant) else {
            continue;
        };
        let mut count = 0usize;
        count_detached(&scan.roots, false, &mut count);
        if count > 0 {
            problems.push((stream, count));
        }
    }
    problems
}

/// Counts CTRL_DATA nodes that are not the first child of a CTRL_HEADER.
fn count_detached(nodes: &[RecordNode], parent_is_control: bool, count: &mut usize) {
    for (index, node) in nodes.iter().enumerate() {
        if node.tag == tag::CTRL_DATA && !(parent_is_control && index == 0) {
            *count += 1;
        }
        count_detached(&node.children, node.tag == tag::CTRL_HEADER, count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tag: u16, children: Vec<RecordNode>) -> RecordNode {
        RecordNode {
            tag,
            data: Vec::new(),
            children,
        }
    }

    /// Writes a small HWP5 file whose table CTRL_DATA sits after the cells, the #610 shape.
    fn write_detached_table_ctrl_data_fixture(path: &Path) {
        use std::io::Write as _;
        let document = hwp_convert::from_markdown("| a |\n|---|\n| b |\n");
        crate::write_document(&document, path, &crate::WriteOptions::default()).unwrap();
        let mut container = crate::Hwp5Container::open(path).unwrap();
        let compressed = container.file_header().is_compressed();
        let bytes = container.read_record_stream("/BodyText/Section0").unwrap();
        drop(container);
        let mut roots = scan_stream(&bytes, ScanMode::Strict).unwrap().roots;
        fn append(nodes: &mut [RecordNode]) -> bool {
            for node in nodes {
                if node.tag == tag::CTRL_HEADER && node.data.get(..4) == Some(b" lbt") {
                    node.children.push(RecordNode {
                        tag: tag::CTRL_DATA,
                        data: b"detached".to_vec(),
                        children: Vec::new(),
                    });
                    return true;
                }
                if append(&mut node.children) {
                    return true;
                }
            }
            false
        }
        assert!(append(&mut roots));
        let raw = RecordNode::serialize_forest(&roots);
        let encoded = if compressed {
            crate::codec::compress(&raw)
        } else {
            raw
        };
        let mut cfb = cfb::open_rw(path).unwrap();
        cfb.create_stream("/BodyText/Section0")
            .unwrap()
            .write_all(&encoded)
            .unwrap();
        cfb.flush().unwrap();
    }

    #[test]
    fn validate_message_names_cause_and_next_step() {
        let path = std::env::temp_dir().join(format!(
            "hwp5-detached-ctrl-data-{}.hwp",
            std::process::id()
        ));
        write_detached_table_ctrl_data_fixture(&path);
        let messages = detached_ctrl_data(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        let message = &messages[0];
        // Cause: which record, where, and why it matters.
        assert!(message.contains("/BodyText/Section0") && message.contains("CTRL_DATA 1개"));
        assert!(message.contains("손상 파일"), "{message}");
        // Next step: re-run the edit on the original with a fixed ahwp, or write .hwpx.
        assert!(
            message.contains("원본 파일에서 같은 편집을 다시 실행") && message.contains(".hwpx"),
            "{message}"
        );
    }

    /// Sets FileHeader attribute bits (UINT32 at offset 36) in place.
    fn set_header_attributes(path: &Path, bits: u32) {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
        let mut cfb = cfb::open_rw(path).unwrap();
        let mut header = Vec::new();
        cfb.open_stream("/FileHeader")
            .unwrap()
            .read_to_end(&mut header)
            .unwrap();
        let attributes = u32::from_le_bytes(header[36..40].try_into().unwrap()) | bits;
        header[36..40].copy_from_slice(&attributes.to_le_bytes());
        let mut stream = cfb.open_stream("/FileHeader").unwrap();
        stream.seek(SeekFrom::Start(0)).unwrap();
        stream.write_all(&header).unwrap();
        drop(stream);
        cfb.flush().unwrap();
    }

    /// Review of #611: the scan must apply the reader's gates. A protected or distribution
    /// document is never decompressed, so even a detached CTRL_DATA in it is not reported
    /// (the reader refuses those files itself).
    #[test]
    fn protected_and_distribution_documents_are_not_scanned() {
        // FileHeader attribute bits: ENCRYPTED 1, DISTRIBUTION 2, DRM 4.
        for (label, bits) in [
            ("encrypted", 1u32 << 1),
            ("drm", 1 << 4),
            ("distribution", 1 << 2),
        ] {
            let path = std::env::temp_dir().join(format!(
                "hwp5-ctrl-data-gate-{label}-{}.hwp",
                std::process::id()
            ));
            write_detached_table_ctrl_data_fixture(&path);
            assert_eq!(
                detached_ctrl_data_counts(&path).len(),
                1,
                "{label}: premise"
            );
            set_header_attributes(&path, bits);
            let found = detached_ctrl_data_counts(&path);
            std::fs::remove_file(&path).unwrap();
            assert!(
                found.is_empty(),
                "{label}: scanned a gated document: {found:?}"
            );
        }
    }

    /// A body section that decompresses past the cap is skipped, not materialized.
    #[test]
    fn section_over_the_scan_cap_is_skipped() {
        use std::io::Write as _;
        let path =
            std::env::temp_dir().join(format!("hwp5-ctrl-data-cap-{}.hwp", std::process::id()));
        write_detached_table_ctrl_data_fixture(&path);
        let compressed = crate::Hwp5Container::open(&path)
            .unwrap()
            .file_header()
            .is_compressed();
        assert!(compressed, "premise: the writer compresses record streams");
        // The real section (with its detached CTRL_DATA) followed by zero padding past the cap.
        // Zero bytes scan as empty records, so an unbounded read would still report the
        // detached CTRL_DATA; only the bounded read skips it. The padding deflates to a few KiB.
        let mut container = crate::Hwp5Container::open(&path).unwrap();
        let mut body = container.read_record_stream("/BodyText/Section0").unwrap();
        drop(container);
        body.resize(body.len() + (MAX_SCAN_STREAM_BYTES as usize), 0);
        let bomb = crate::codec::compress(&body);
        let mut cfb = cfb::open_rw(&path).unwrap();
        cfb.create_stream("/BodyText/Section0")
            .unwrap()
            .write_all(&bomb)
            .unwrap();
        cfb.flush().unwrap();
        drop(cfb);
        let found = detached_ctrl_data_counts(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn ctrl_data_must_lead_its_control_header() {
        let attached = node(
            tag::CTRL_HEADER,
            vec![node(tag::CTRL_DATA, vec![]), node(tag::TABLE, vec![])],
        );
        let mut count = 0;
        count_detached(&[attached], false, &mut count);
        assert_eq!(count, 0);

        let detached = node(
            tag::CTRL_HEADER,
            vec![node(tag::TABLE, vec![]), node(tag::CTRL_DATA, vec![])],
        );
        let orphan = node(tag::PARA_HEADER, vec![node(tag::CTRL_DATA, vec![])]);
        let mut count = 0;
        count_detached(&[detached, orphan], false, &mut count);
        assert_eq!(count, 2);
    }
}

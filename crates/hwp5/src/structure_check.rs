//! Record-tree structure rules the IR reader tolerates but Hangul rejects.
//!
//! Genuine Hangul-saved streams write a control's CTRL_DATA as the first child of its
//! CTRL_HEADER (observed in the #610 genuine form and the local ground-truth corpus). A
//! CTRL_DATA found later among the children, for example after a table's
//! last cell paragraph, is detached from its control: the reader still parses the file but
//! Hangul reports corruption (#610).

use std::path::Path;

use crate::record::{RecordNode, ScanMode, scan_stream, tag};

/// Returns one message per detached CTRL_DATA record in the document's `/BodyText` sections.
/// Streams that cannot be read or scanned (distribution documents, damaged streams) are
/// skipped here; the regular reader reports those.
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
    let mut sections = container
        .list_streams()
        .into_iter()
        .map(|stream| stream.path)
        .filter(|path| path.starts_with("/BodyText/Section"))
        .collect::<Vec<_>>();
    sections.sort_by_key(|path| {
        path.trim_start_matches("/BodyText/Section")
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    });
    let mut problems = Vec::new();
    for stream in sections {
        let Ok(bytes) = container.read_record_stream(&stream) else {
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

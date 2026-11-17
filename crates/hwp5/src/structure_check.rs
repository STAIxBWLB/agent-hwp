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
            problems.push(format!(
                "{stream}: CTRL_DATA {count}개가 소유 CTRL_HEADER 바로 뒤에 있지 않습니다 \
                 (한글이 손상 파일로 판정)"
            ));
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

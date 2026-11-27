//! #619 memo edit ops end to end: `memo_add`, `memo_set_text` and `memo_remove` through
//! `hwp edit --ops`, read back with this repository's own readers.
//!
//! The fixtures are local-only (`fixtures/hwp5/memo1.hwp`, `fixtures/hwpx/memo1.hwpx`), so the
//! tests skip without them through the shared fixture accounting. Each output is re-read from
//! disk: a synthesized HWP5 body must be found through its `MEMO_LIST` records and a synthesized
//! HWPX body through the re-parsed `fieldBegin type="MEMO"`, not through the in-memory model.

use std::path::{Path, PathBuf};
use std::process::Command;

use hwp_model::memo::Memo;

#[path = "common/fixture_skip.rs"]
mod fixture_skip;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn test_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("hwp-cli-memo-ops-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run_ops(dir: &Path, input: &Path, output: &Path, ops: &str) {
    let ops_path = dir.join("ops.json");
    std::fs::write(&ops_path, ops).unwrap();
    let run = hwp()
        .arg("edit")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--ops")
        .arg(&ops_path)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "hwp edit --ops -> {output:?} failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

fn hwp5_memos(path: &Path) -> Vec<Memo> {
    hwp_model::memo::memos(&hwp5::read_document(path).expect("hwp5 re-read").document)
}

fn hwpx_memos(path: &Path) -> Vec<Memo> {
    hwp_model::memo::memos(&hwpx::read_document(path).expect("hwpx re-read").document)
}

/// The first body paragraph wide enough to carry a two-WCHAR anchor.
fn anchor_paragraph(doc: &hwp_model::Document) -> usize {
    doc.sections[0]
        .paragraphs
        .iter()
        .position(|p| p.wchar_len() >= 2 && !p.own_text().is_empty())
        .expect("no paragraph can carry a memo anchor")
}

fn memo(memos: &[Memo], number: u32) -> &Memo {
    memos
        .iter()
        .find(|m| m.number == number)
        .unwrap_or_else(|| panic!("memo {number} missing: {memos:?}"))
}

fn body(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_string()).collect()
}

#[test]
fn hwp5_add_memo_writes_its_memo_list_body() {
    let input = fixtures().join("hwp5/memo1.hwp");
    if fixture_skip::fixture_missing(&input) {
        return;
    }
    let before = hwp5_memos(&input);
    let original = memo(&before, 1).clone();
    let doc = hwp5::read_document(&input).unwrap().document;
    let para = anchor_paragraph(&doc);

    let dir = test_dir("hwp5-add");
    let out = dir.join("out.hwp");
    run_ops(
        &dir,
        &input,
        &out,
        &format!(
            r#"[{{"op":"memo_add","section":0,"anchor":{{"para":{para},"start":0,"end":2}},"author":"테스터","body":["추가 메모"]}}]"#
        ),
    );

    let after = hwp5_memos(&out);
    assert_eq!(after.len(), 2, "one existing and one added memo: {after:?}");
    let added = memo(&after, 2);
    assert_eq!(added.body, body(&["추가 메모"]));
    assert_eq!(added.author, "테스터");
    assert!(
        added.created_filetime.is_some_and(|t| t != 0),
        "a synthesized memo carries a real FILETIME: {added:?}"
    );
    // The pre-existing memo keeps its anchor and body through the synthesized write.
    let kept = memo(&after, 1);
    assert_eq!(kept.body, original.body);
    assert_eq!(kept.author, original.author);
    assert_eq!(kept.created_filetime, original.created_filetime);
}

/// Hancom saves the last paragraph's CTRL_HEADER siblings before the memo body suffix (07 L7).
#[test]
fn hwp5_added_memo_bodies_follow_all_anchor_headers() {
    use hwp5::container::Hwp5Container;
    use hwp5::record::{ScanMode, scan_stream, tag};

    fn last_paragraph_order(file: &Path) -> Vec<u16> {
        let stream = Hwp5Container::open(file)
            .unwrap()
            .read_record_stream("/BodyText/Section0")
            .unwrap();
        let roots = scan_stream(&stream, ScanMode::Strict).unwrap().roots;
        roots
            .last()
            .unwrap()
            .children
            .iter()
            .filter_map(|record| match record.tag {
                tag::CTRL_HEADER | tag::MEMO_LIST => Some(record.tag),
                _ => None,
            })
            .collect()
    }

    let original = fixtures().join("hwp5/memo1.hwp");
    let genuine = fixtures().join("hwp5/memo2.hwp");
    if fixture_skip::fixture_missing(&original) || fixture_skip::fixture_missing(&genuine) {
        return;
    }
    let reference = last_paragraph_order(&genuine);
    assert_eq!(
        reference,
        [tag::CTRL_HEADER, tag::MEMO_LIST, tag::MEMO_LIST]
    );

    let dir = test_dir("hwp5-memo-order");
    let empty = dir.join("empty.hwp");
    run_ops(
        &dir,
        &original,
        &empty,
        r#"[{"op":"memo_remove","section":0,"number":1}]"#,
    );
    for (source, expected) in [
        (
            &original,
            [vec![tag::CTRL_HEADER], reference.clone()].concat(),
        ),
        (&empty, reference[..2].to_vec()),
    ] {
        let output = dir.join("out.hwp");
        run_ops(
            &dir,
            source,
            &output,
            r#"[{"op":"memo_add","section":0,"anchor":{"para":1,"start":0,"end":2},"author":"Tester","body":["Added memo","Second line"]}]"#,
        );
        assert_eq!(
            last_paragraph_order(&output),
            expected,
            "all anchor headers must precede the first memo body group for {source:?}",
        );
    }
}

/// 07 B4: nchars bit31 marks the last paragraph of a list, so a two-line memo body carries it on
/// the final line only. ORing the anchor template's own flag (the anchor paragraph is usually the
/// section's last, so it holds bit31) would mark every line as the end of the list.
#[test]
fn hwp5_added_memo_marks_only_its_last_body_line_as_list_final() {
    let input = fixtures().join("hwp5/memo1.hwp");
    if fixture_skip::fixture_missing(&input) {
        return;
    }
    let doc = hwp5::read_document(&input).unwrap().document;
    let para = anchor_paragraph(&doc);
    let dir = test_dir("hwp5-last-flag");
    let out = dir.join("out.hwp");
    run_ops(
        &dir,
        &input,
        &out,
        &format!(
            r#"[{{"op":"memo_add","section":0,"anchor":{{"para":{para},"start":0,"end":2}},"author":"테스터","body":["첫 줄","둘째 줄"]}}]"#
        ),
    );

    let doc = hwp5::read_document(&out).unwrap().document;
    let extras = &doc.sections[0]
        .paragraphs
        .last()
        .expect("section body")
        .extras;
    let flags = |number: u32| -> Vec<u8> {
        let start = extras
            .iter()
            .position(|e| e.tag == hwp_model::memo::MEMO_LIST_TAG && e.data == number.to_le_bytes())
            .unwrap_or_else(|| panic!("no MEMO_LIST for memo {number}"));
        extras[start + 1..]
            .iter()
            .take_while(|e| e.tag != hwp_model::memo::MEMO_LIST_TAG)
            .filter(|e| e.tag == hwp_model::memo::PARA_HEADER_TAG)
            .map(|e| e.data[3] & 0x80)
            .collect()
    };

    assert_eq!(
        flags(2),
        vec![0x00, 0x80],
        "only the final body line is list-final"
    );
    assert_eq!(
        flags(1),
        vec![0x80],
        "the fixture's single-line memo keeps its flag"
    );
}

#[test]
fn hwp5_set_and_remove_memo_body() {
    let input = fixtures().join("hwp5/memo1.hwp");
    if fixture_skip::fixture_missing(&input) {
        return;
    }
    let before = hwp5_memos(&input);
    let original = memo(&before, 1).clone();
    let dir = test_dir("hwp5-set-remove");

    let set_out = dir.join("set.hwp");
    run_ops(
        &dir,
        &input,
        &set_out,
        r#"[{"op":"memo_set_text","section":0,"number":1,"body":["바뀐 본문","둘째 줄"]}]"#,
    );
    let set = hwp5_memos(&set_out);
    assert_eq!(set.len(), 1);
    let changed = memo(&set, 1);
    assert_eq!(changed.body, body(&["바뀐 본문", "둘째 줄"]));
    assert_eq!(changed.anchor_text, original.anchor_text);
    assert_eq!(changed.author, original.author);
    assert_eq!(changed.created_filetime, original.created_filetime);

    let removed_out = dir.join("removed.hwp");
    run_ops(
        &dir,
        &input,
        &removed_out,
        r#"[{"op":"memo_remove","section":0,"number":1}]"#,
    );
    assert!(hwp5_memos(&removed_out).is_empty());
    let doc = hwp5::read_document(&removed_out).unwrap().document;
    let last = doc.sections[0].paragraphs.last().expect("last paragraph");
    assert!(
        !last
            .extras
            .iter()
            .any(|extra| extra.tag == hwp_model::memo::MEMO_LIST_TAG),
        "a removed memo leaves no MEMO_LIST body behind"
    );
}

#[test]
fn hwpx_add_set_and_remove_memo_field() {
    let input = fixtures().join("hwpx/memo1.hwpx");
    if fixture_skip::fixture_missing(&input) {
        return;
    }
    let before = hwpx_memos(&input);
    let original = memo(&before, 1).clone();
    let doc = hwpx::read_document(&input).unwrap().document;
    let para = anchor_paragraph(&doc);
    let dir = test_dir("hwpx");

    let added_out = dir.join("added.hwpx");
    run_ops(
        &dir,
        &input,
        &added_out,
        &format!(
            r#"[{{"op":"memo_add","section":0,"anchor":{{"para":{para},"start":0,"end":2}},"author":"테스터","body":["추가 메모"]}}]"#
        ),
    );
    let added = hwpx_memos(&added_out);
    assert_eq!(added.len(), 2, "one existing and one added memo: {added:?}");
    assert_eq!(memo(&added, 2).body, body(&["추가 메모"]));
    assert_eq!(memo(&added, 1).body, original.body);

    let set_out = dir.join("set.hwpx");
    run_ops(
        &dir,
        &input,
        &set_out,
        r#"[{"op":"memo_set_text","section":0,"number":1,"body":["바뀐 본문"]}]"#,
    );
    let set = hwpx_memos(&set_out);
    assert_eq!(set.len(), 1);
    assert_eq!(memo(&set, 1).body, body(&["바뀐 본문"]));
    assert_eq!(memo(&set, 1).author, original.author);

    let removed_out = dir.join("removed.hwpx");
    run_ops(
        &dir,
        &input,
        &removed_out,
        r#"[{"op":"memo_remove","section":0,"number":1}]"#,
    );
    assert!(hwpx_memos(&removed_out).is_empty());
}

//! Hangul memo (MEMO field) fixtures: memos survive hwp5 -> IR -> hwp5 byte-identically and read
//! without spurious warnings. The fixtures are local only (owner-authored, gitignored); without
//! them every test here skips and is counted (#275). No test asserts on or prints a memo author
//! or the raw field command.

use std::path::PathBuf;

#[path = "../../hwp-cli/tests/common/fixture_skip.rs"]
mod fixture_skip;

const MEMOS: &[(&str, usize)] = &[
    ("memo1.hwp", 1),
    ("memo2.hwp", 2),
    ("memo3.hwp", 2),
    ("memo4.hwp", 1),
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hwp5")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hwp5-memo-tests-{}-{name}", std::process::id()))
}

#[track_caller]
fn missing() -> bool {
    MEMOS.iter().fold(false, |acc, (name, _)| {
        fixture_skip::fixture_missing(&fixture(name)) || acc
    })
}

#[test]
fn memo_documents_round_trip_byte_identically() {
    if missing() {
        return;
    }
    let opts = hwp5::WriteOptions {
        preserve_linesegs: true,
        ..Default::default()
    };
    for (name, _) in MEMOS {
        let src = fixture(name);
        let doc = hwp5::read_document(&src).unwrap().document;
        let out = tmp(name);
        hwp5::write_document(&doc, &out, &opts).unwrap();
        let mut orig = hwp5::Hwp5Container::open(&src).unwrap();
        let mut ours = hwp5::Hwp5Container::open(&out).unwrap();
        let mut streams = vec!["/DocInfo".to_string()];
        streams.extend(orig.body_sections());
        for stream in streams {
            let a = orig.read_record_stream(&stream).unwrap();
            let b = ours.read_record_stream(&stream).unwrap();
            assert!(a == b, "{name} {stream}: {} vs {} bytes", a.len(), b.len());
        }
        let _ = std::fs::remove_file(&out);
    }
}

#[test]
fn memo_documents_read_without_ctrl_id_warnings_and_list_their_memos() {
    if missing() {
        return;
    }
    for (name, count) in MEMOS {
        let read = hwp5::read_document(&fixture(name)).unwrap();
        assert!(
            !read.warnings.iter().any(|w| w.contains("ctrl_id")),
            "{name}: {:?}",
            read.warnings
        );
        let memos = hwp_model::memo::memos(&read.document);
        assert_eq!(memos.len(), *count, "{name}");
        for (i, memo) in memos.iter().enumerate() {
            assert_eq!(memo.number as usize, i + 1, "{name}");
            assert!(memo.created_utc().is_some(), "{name}");
            assert!(memo.body.iter().any(|p| !p.trim().is_empty()), "{name}");
            assert!(!memo.anchor_text.is_empty(), "{name}");
        }
    }
}

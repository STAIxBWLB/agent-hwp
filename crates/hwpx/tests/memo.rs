//! Hangul memo (MEMO fieldBegin) fixtures in HWPX. HWPX nests a memo's body inside the field's
//! subList, so text extraction must skip it. The fixtures are local only (owner-authored,
//! gitignored); without them every test skips and is counted (#275). No test asserts on or
//! prints a memo author or the raw field command.

use std::path::PathBuf;

#[path = "../../hwp-cli/tests/common/fixture_skip.rs"]
mod fixture_skip;

const MEMOS: &[(&str, usize)] = &[
    ("memo1.hwpx", 1),
    ("memo2.hwpx", 2),
    ("memo3.hwpx", 2),
    ("memo4.hwpx", 1),
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hwpx")
        .join(name)
}

#[track_caller]
fn missing() -> bool {
    MEMOS.iter().fold(false, |acc, (name, _)| {
        fixture_skip::fixture_missing(&fixture(name)) || acc
    })
}

#[test]
fn memo_fields_are_listed_and_stay_out_of_plain_text() {
    if missing() {
        return;
    }
    for (name, count) in MEMOS {
        let doc = hwpx::read_document(&fixture(name)).unwrap().document;
        let memos = hwp_model::memo::memos(&doc);
        assert_eq!(memos.len(), *count, "{name}");
        let text = doc.plain_text();
        for body in memos
            .iter()
            .flat_map(|m| &m.body)
            .filter(|p| !p.trim().is_empty())
        {
            assert!(
                !text.contains(body.trim()),
                "{name}: memo body leaked into text"
            );
        }
    }
}

#[test]
fn memo_fields_survive_hwpx_rewrite() {
    if missing() {
        return;
    }
    for (name, count) in MEMOS {
        let doc = hwpx::read_document(&fixture(name)).unwrap().document;
        let out = std::env::temp_dir().join(format!("hwpx-memo-{}-{name}", std::process::id()));
        hwpx::write_document(&doc, &out).unwrap();
        let back = hwpx::read_document(&out).unwrap().document;
        let _ = std::fs::remove_file(&out);
        let (a, b) = (hwp_model::memo::memos(&doc), hwp_model::memo::memos(&back));
        assert_eq!(b.len(), *count, "{name}");
        for (x, y) in a.iter().zip(&b) {
            assert!(
                x.number == y.number
                    && x.created_filetime == y.created_filetime
                    && x.anchor_text == y.anchor_text
                    && x.body == y.body,
                "{name}: memo {} changed on rewrite",
                x.number
            );
        }
    }
}

//! `cat --with-memos` (issue #110). The refusal tests use the committed synthetic sample. The
//! fixture tests read local-only, owner-authored memo documents (gitignored) and skip, counted,
//! without them (#275). Comparisons avoid printing memo authors and raw field commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "common/fixture_skip.rs"]
mod fixture_skip;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn cat(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
        .arg("cat")
        .args(args)
        .output()
        .expect("ahwp 실행")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn with_memos_refuses_incompatible_modes() {
    let sample = root().join("fixtures/samples/report-tables.hwpx");
    let sample = sample.to_str().unwrap();
    for extra in [
        &["--with-segments"][..],
        &["--preview"],
        &["--format", "json"],
    ] {
        let mut args = vec![sample, "--with-memos"];
        args.extend_from_slice(extra);
        let out = cat(&args);
        assert!(!out.status.success(), "{extra:?}");
        assert!(
            text(&out.stderr).contains("--with-memos"),
            "{extra:?}: {}",
            text(&out.stderr)
        );
        if extra[0] != "--format" {
            // The refusal names the next step, not only the conflict.
            assert!(
                text(&out.stderr).contains("해결:"),
                "{extra:?}: {}",
                text(&out.stderr)
            );
        }
    }
}

#[test]
fn memo_authors_requires_with_memos() {
    let sample = root().join("fixtures/samples/report-tables.hwpx");
    let out = cat(&[sample.to_str().unwrap(), "--memo-authors"]);
    assert!(!out.status.success());
}

#[test]
fn a_document_without_memos_prints_no_appendix() {
    let sample = root().join("fixtures/samples/report-tables.hwpx");
    let sample = sample.to_str().unwrap();
    let plain = cat(&[sample]);
    let with = cat(&[sample, "--with-memos"]);
    assert!(with.status.success(), "{}", text(&with.stderr));
    assert_eq!(plain.stdout, with.stdout);
}

/// Both containers of each memo document list the same number of memos, the appendix never names an author without `--memo-authors`, and the body text
/// before the appendix is the plain `cat` output.
#[test]
fn memo_fixtures_list_identical_memos_in_both_formats() {
    let pairs: Vec<(PathBuf, PathBuf)> = (1..=4)
        .map(|n| {
            (
                root().join(format!("fixtures/hwp5/memo{n}.hwp")),
                root().join(format!("fixtures/hwpx/memo{n}.hwpx")),
            )
        })
        .collect();
    if pairs.iter().fold(false, |acc, (a, b)| {
        fixture_skip::fixture_missing(a) | fixture_skip::fixture_missing(b) || acc
    }) {
        return;
    }
    for (hwp, hwpx) in &pairs {
        let mut appendices = Vec::new();
        for path in [hwp, hwpx] {
            let path = path.to_str().unwrap();
            let plain = cat(&[path]);
            let with = cat(&[path, "--with-memos"]);
            let authors = cat(&[path, "--with-memos", "--memo-authors"]);
            assert!(with.status.success() && authors.status.success(), "{path}");
            assert!(
                text(&with.stderr).is_empty(),
                "{path}: {}",
                text(&with.stderr)
            );
            let with = text(&with.stdout);
            let body = text(&plain.stdout);
            let appendix = with
                .strip_prefix(body.as_str())
                .unwrap_or_else(|| panic!("{path}"));
            assert!(appendix.contains("[메모 1]"), "{path}");
            // The opt-in adds a `, <author>` to every entry, and only the opt-in does.
            assert!(!appendix.contains("Z, "), "{path}");
            let authors = text(&authors.stdout);
            assert_eq!(
                authors.matches("Z, ").count(),
                appendix.matches("[메모 ").count(),
                "{path}"
            );
            appendices.push(appendix.to_string());
        }
        // The two containers were saved separately, so only the entry layout is compared.
        let strip = |s: &str| s.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            strip(&appendices[0]).len(),
            strip(&appendices[1]).len(),
            "{hwp:?}"
        );
    }
}

/// Both native streams preserve the measured memo semantics through the other container.
#[test]
fn cross_format_memos_preserve_their_semantics() {
    // (source, target extension, memo count) for the local Hancom-saved files.
    let cases: Vec<(PathBuf, &str, usize)> = [(1, 1), (2, 2), (3, 2), (4, 1)]
        .into_iter()
        .flat_map(|(n, memos)| {
            [
                (
                    root().join(format!("fixtures/hwp5/memo{n}.hwp")),
                    "hwpx",
                    memos,
                ),
                (
                    root().join(format!("fixtures/hwpx/memo{n}.hwpx")),
                    "hwp",
                    memos,
                ),
            ]
        })
        .collect();
    if cases.iter().fold(false, |acc, (p, _, _)| {
        fixture_skip::fixture_missing(p) || acc
    }) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("ahwp-memo-cross-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (i, (src, ext, memos)) in cases.iter().enumerate() {
        let out = dir.join(format!("{i}.{ext}"));
        let report = dir.join(format!("{i}.json"));
        let run = Command::new(env!("CARGO_BIN_EXE_ahwp"))
            .args([
                "convert",
                src.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
            ])
            .args(["--loss-report", report.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(run.status.success(), "{src:?}: {}", text(&run.stderr));
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
        let events = report["events"].as_array().unwrap();
        // HWP5 document metadata can contain trailing title whitespace and sub-second FILETIME
        // values. Its ordinary HWPX conversion reports their pre-existing document metadata loss.
        assert!(
            events
                .iter()
                .all(|event| *ext == "hwpx" && event["code"] == "metadata_value_removed"),
            "{src:?}: {report}"
        );
        let read_memos = |file: &Path| {
            let doc = if file.extension().unwrap() == "hwpx" {
                hwpx::read_document(file).unwrap().document
            } else {
                hwp5::read_document(file).unwrap().document
            };
            hwp_model::memo::memos(&doc)
        };
        let expected = read_memos(src);
        assert_eq!(expected.len(), *memos);
        assert!(
            read_memos(&out) == expected,
            "{src:?}: converted memo semantics differ"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

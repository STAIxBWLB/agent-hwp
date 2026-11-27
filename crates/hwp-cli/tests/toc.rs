//! `new --toc` and `new --section-strips` (issue #620). Synthetic markdown only. No test asserts a
//! hardcoded page number: CI fonts differ per host, so every TOC page is compared with the page
//! the published `render --layout-json` puts its heading on, in the same environment.

use std::path::Path;
use std::process::{Command, Output};

#[path = "common/temp_path.rs"]
mod temp_path;
use temp_path::TempPath;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn tmp(name: &str) -> TempPath {
    TempPath::new("hwp-cli-toc", name)
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    hwp().args(args).output().expect("ahwp 실행")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A report long enough to span several pages: four chapters of three numbered sections.
fn report_md() -> String {
    let mut md = String::from("# 사업 보고서\n\n");
    for (c, roman) in ["Ⅰ", "Ⅱ", "Ⅲ", "Ⅳ"].iter().enumerate() {
        md.push_str(&format!("## {roman}. 장 {}\n\n", c + 1));
        for s in 1..=3 {
            md.push_str(&format!("### {s}. 절 {}-{s}\n\n", c + 1));
            for _ in 0..3 {
                md.push_str(&"본문 문단입니다. ".repeat(40));
                md.push_str("\n\n");
            }
        }
    }
    md
}

fn new_doc(out: &Path, flags: &[&str]) -> Output {
    let md = tmp(&format!(
        "{}.md",
        out.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&md, report_md()).unwrap();
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "new".as_ref(),
        "-o".as_ref(),
        out.as_os_str(),
        "--from".as_ref(),
        md.as_os_str(),
        "--preset".as_ref(),
        "report".as_ref(),
    ];
    args.extend(flags.iter().map(std::ffi::OsStr::new));
    run(&args)
}

/// The TOC rows (title, page) read back from `cat --format markdown`.
fn toc_rows(doc: &Path) -> Vec<(String, u32)> {
    let md = ok(&run(&[
        "cat".as_ref(),
        "--format".as_ref(),
        "markdown".as_ref(),
        doc.as_os_str(),
    ]));
    md.lines()
        .skip_while(|l| !l.contains("목차"))
        .filter(|l| l.starts_with("| ") && !l.starts_with("| ---"))
        .take_while(|l| l.matches('|').count() == 3)
        .map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            (
                cells[0].to_string(),
                cells[1].parse().expect("numeric page"),
            )
        })
        .collect()
}

/// Displayed body page of each heading, from the published artifacts alone: the segment
/// envelope locates the heading paragraph's id by its markdown text, the layout JSON gives the
/// page of that id, and body pages count from the page of the first body heading.
fn layout_pages(doc: &Path, titles: &[String]) -> Vec<u32> {
    let envelope = ok(&run(&[
        "cat".as_ref(),
        "--format".as_ref(),
        "markdown".as_ref(),
        "--with-segments".as_ref(),
        "--segments".as_ref(),
        "v2".as_ref(),
        doc.as_os_str(),
    ]));
    let env: serde_json::Value =
        serde_json::from_str(envelope.lines().last().unwrap()).expect("envelope JSON");
    let markdown: Vec<char> = env["markdown"].as_str().unwrap().chars().collect();
    let text = |seg: &serde_json::Value| -> String {
        let r = &seg["char_range"];
        let (s, e) = (r["start"].as_u64().unwrap(), r["end"].as_u64().unwrap());
        markdown[s as usize..e as usize].iter().collect()
    };
    let layout_path = tmp(&format!(
        "{}.layout.json",
        doc.file_name().unwrap().to_string_lossy()
    ));
    let svg = tmp(&format!(
        "{}.svg",
        doc.file_name().unwrap().to_string_lossy()
    ));
    ok(&run(&[
        "render".as_ref(),
        doc.as_os_str(),
        "-o".as_ref(),
        svg.as_os_str(),
        "--layout-json".as_ref(),
        layout_path.as_os_str(),
    ]));
    let layout: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&layout_path).unwrap()).unwrap();
    let page_of = |id: &str| -> u64 {
        layout["pages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["rows"].as_array().unwrap().iter().any(|r| r["id"] == id))
            .map(|p| p["page"].as_u64().unwrap())
            .min()
            .unwrap_or_else(|| panic!("segment {id} on no page"))
    };
    // Top-level paragraph (or strip table) segment whose markdown is the heading line.
    let heading_id = |title: &str| -> String {
        env["segments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["path"]["indices"].as_array().unwrap().len() == 1)
            .filter(|s| s["kind"] == "para")
            .find(|s| {
                let t = text(s);
                // Headings export as `## **Title**`; a strip exports as `### N. Title`.
                t.trim_start_matches('#').trim().trim_matches('*') == title
            })
            .map(|s| s["id"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("no heading segment for {title:?}"))
    };
    let ids: Vec<String> = titles.iter().map(|t| heading_id(t)).collect();
    let first = page_of(&ids[0]);
    ids.iter()
        .map(|id| u32::try_from(page_of(id) - first + 1).unwrap())
        .collect()
}

/// AC2: the TOC lists each chapter and section heading with the body page the layout pass
/// itself puts that heading on, in both containers, and the TOC actually spans several pages.
#[test]
fn toc_pages_match_the_published_layout() {
    for ext in ["hwpx", "hwp"] {
        let out = tmp(&format!("toc.{ext}"));
        ok(&new_doc(&out, &["--toc", "--section-strips"]));
        let rows = toc_rows(&out);
        assert_eq!(rows.len(), 16, "{ext}: {rows:?}");
        let titles: Vec<String> = rows.iter().map(|(t, _)| t.clone()).collect();
        let pages: Vec<u32> = rows.iter().map(|(_, p)| *p).collect();
        assert_eq!(pages, layout_pages(&out, &titles), "{ext}: {rows:?}");
        assert_eq!(pages[0], 1, "{ext}: body pages restart at 1");
        assert!(pages.windows(2).all(|w| w[0] <= w[1]), "{ext}: {pages:?}");
        assert!(
            pages.last().unwrap() > &1,
            "{ext}: the report spans one page"
        );
    }
}

/// AC3: a numbered level-3 heading becomes a 1x3 table in both containers and reads back as
/// `### N. Title`.
#[test]
fn section_strips_are_tables_that_read_back_as_headings() {
    for ext in ["hwpx", "hwp"] {
        let out = tmp(&format!("strips.{ext}"));
        ok(&new_doc(&out, &["--section-strips"]));
        let md = ok(&run(&[
            "cat".as_ref(),
            "--format".as_ref(),
            "markdown".as_ref(),
            out.as_os_str(),
        ]));
        for heading in ["### 1. 절 1-1", "### 3. 절 4-3"] {
            assert!(
                md.lines().any(|l| l == heading),
                "{ext}: {heading} missing\n{md}"
            );
        }
        let json = tmp(&format!("strips-{ext}.json"));
        ok(&run(&[
            "convert".as_ref(),
            out.as_os_str(),
            "-o".as_ref(),
            json.as_os_str(),
        ]));
        let ir: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        let strips = ir["sections"][0]["paragraphs"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p["controls"].as_array().unwrap())
            .filter_map(|c| c.get("Table"))
            .filter(|t| t["rows"] == 1 && t["cols"] == 3)
            .count();
        assert_eq!(strips, 12, "{ext}");
        // Join-is-total: every row `render --layout-json` reports (strip tables and cells
        // included) has its segment in the `cat --with-segments` envelope.
        let envelope = ok(&run(&[
            "cat".as_ref(),
            "--format".as_ref(),
            "markdown".as_ref(),
            "--with-segments".as_ref(),
            "--segments".as_ref(),
            "v2".as_ref(),
            out.as_os_str(),
        ]));
        let env: serde_json::Value =
            serde_json::from_str(envelope.lines().last().unwrap()).expect("envelope JSON");
        let ids: std::collections::BTreeSet<&str> = env["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        let layout_path = tmp(&format!("strips-{ext}.layout.json"));
        let svg = tmp(&format!("strips-{ext}.svg"));
        ok(&run(&[
            "render".as_ref(),
            out.as_os_str(),
            "-o".as_ref(),
            svg.as_os_str(),
            "--layout-json".as_ref(),
            layout_path.as_os_str(),
        ]));
        let layout: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&layout_path).unwrap()).unwrap();
        let missing: Vec<String> = layout["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p["rows"].as_array().unwrap())
            .map(|r| r["id"].as_str().unwrap().to_string())
            .filter(|id| !ids.contains(id.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "{ext}: layout rows without a segment: {missing:?}"
        );
    }
}

/// Without the flags the output is unchanged: no TOC, no strip tables.
#[test]
fn report_output_is_unchanged_without_the_flags() {
    let plain = tmp("plain.hwpx");
    ok(&new_doc(&plain, &[]));
    let md = ok(&run(&[
        "cat".as_ref(),
        "--format".as_ref(),
        "markdown".as_ref(),
        plain.as_os_str(),
    ]));
    assert!(!md.contains("목차"), "{md}");
    assert!(md.lines().any(|l| l == "### **1. 절 1-1**"), "{md}");
    assert!(!md.contains('|'), "a table appeared without the flags");
}

/// The flags need markdown input; JSON IR is refused with a next step.
#[test]
fn toc_and_strips_refuse_json_input() {
    let json = tmp("in.json");
    let src = tmp("src.hwpx");
    ok(&new_doc(&src, &[]));
    ok(&run(&[
        "convert".as_ref(),
        src.as_os_str(),
        "-o".as_ref(),
        json.as_os_str(),
    ]));
    for flag in ["--toc", "--section-strips"] {
        let out = tmp("refused.hwpx");
        let result = run(&[
            "new".as_ref(),
            "-o".as_ref(),
            out.as_os_str(),
            "--from".as_ref(),
            json.as_os_str(),
            flag.as_ref(),
        ]);
        assert!(!result.status.success(), "{flag}");
        let err = String::from_utf8_lossy(&result.stderr);
        assert!(
            err.contains("markdown") && err.contains("해결:"),
            "{flag}: {err}"
        );
        assert!(!out.exists(), "{flag}");
    }
}

/// DocumentSpec v2 `report_blocks`: headings are `개요 N`-styled paragraphs. The TOC pages match
/// the published layout and the strips read back as headings, in both containers.
#[test]
fn compose_report_blocks_match_the_published_layout() {
    let mut blocks = Vec::new();
    let para = |style: &str, text: String| serde_json::json!({"type": "paragraph", "style": style, "runs": [{"type": "text", "text": text}]});
    for c in 1..=3 {
        blocks.push(para("개요 2", format!("{c}장 제목")));
        for s in 1..=2 {
            blocks.push(para("개요 3", format!("{s}. 절 {c}-{s}")));
            for _ in 0..4 {
                blocks.push(para("본문", "본문 문단입니다. ".repeat(40)));
            }
        }
    }
    let styles = serde_json::json!({
        "개요 2": {"font_size_pt": 15.0, "bold": true},
        "개요 3": {"font_size_pt": 13.0, "bold": true},
        "본문": {},
    });
    // The plain section, and one whose anchor paragraph also holds a header, a footer and a
    // page number (controls with no text, so the layout gives that paragraph no segment).
    let plain = serde_json::json!({"blocks": blocks});
    let mut decorated = plain.clone();
    decorated["header"] = serde_json::json!({"default": [para("본문", "머리말".into())]});
    decorated["footer"] = serde_json::json!({"default": [para("본문", "꼬리말".into())]});
    decorated["page_number"] = serde_json::json!({"position": "bottom_center", "start": 1});
    for (variant, section) in [("plain", plain), ("decorated", decorated)] {
        let spec = serde_json::json!({
            "version": "2.0",
            "document": {"version": "1.0", "styles": styles, "sections": [section]},
            "report_blocks": {"toc": true, "section_strips": true},
        });
        let spec_path = tmp(&format!("report-{variant}.json"));
        std::fs::write(&spec_path, spec.to_string()).unwrap();
        // `.hwp` compose with a header or footer fails its semantic self-check on main
        // without report_blocks too (a separate writer issue), so the decorated case runs on
        // HWPX only.
        let exts: &[&str] = if variant == "decorated" {
            &["hwpx"]
        } else {
            &["hwpx", "hwp"]
        };
        for &ext in exts {
            let ext_label = format!("{variant}.{ext}");
            let out = tmp(&format!("composed-{ext_label}"));
            ok(&run(&[
                "compose".as_ref(),
                spec_path.as_os_str(),
                "-o".as_ref(),
                out.as_os_str(),
            ]));
            let rows = toc_rows(&out);
            assert_eq!(rows.len(), 9, "{ext_label}: {rows:?}");
            let titles: Vec<String> = rows.iter().map(|(t, _)| t.clone()).collect();
            let pages: Vec<u32> = rows.iter().map(|(_, p)| *p).collect();
            assert_eq!(pages, layout_pages(&out, &titles), "{ext_label}: {rows:?}");
            assert!(pages.last().unwrap() > &1, "{ext_label}: {pages:?}");
            let md = ok(&run(&[
                "cat".as_ref(),
                "--format".as_ref(),
                "markdown".as_ref(),
                out.as_os_str(),
            ]));
            assert!(
                md.lines().any(|l| l == "### 2. 절 3-2"),
                "{ext_label}\n{md}"
            );
            if ext == "hwp" {
                // The published .hwp keeps the line caches the structural writer synthesizes, as
                // without the TOC. Measuring a re-read copy and publishing that instead cleared them,
                // and the cacheless layout can paginate away from the measured TOC.
                let json = ok(&run(&[
                    "cat".as_ref(),
                    "--format".as_ref(),
                    "json".as_ref(),
                    out.as_os_str(),
                ]));
                let ir: serde_json::Value = serde_json::from_str(&json).expect("IR JSON");
                let paras = ir["sections"][0]["paragraphs"].as_array().unwrap();
                let cached = paras
                    .iter()
                    .filter(|p| p["line_segs"].as_array().is_some_and(|s| !s.is_empty()))
                    .count();
                assert_eq!(
                    cached,
                    paras.len(),
                    "{ext_label}: paragraphs without line caches"
                );
            }
        }
    }
}

/// A TOC taller than its page is refused with a next step instead of being clipped: the
/// inline TOC table never splits. The 16-entry report elsewhere in this file fits.
#[test]
fn a_toc_taller_than_one_page_is_refused() {
    let mut md = String::from("# 보고서\n\n");
    for i in 1..=60 {
        md.push_str(&format!("## {i}장\n\n본문.\n\n"));
    }
    let src = tmp("long.md");
    std::fs::write(&src, md).unwrap();
    for ext in ["hwpx", "hwp"] {
        let out = tmp(&format!("long.{ext}"));
        let result = run(&[
            "new".as_ref(),
            "-o".as_ref(),
            out.as_os_str(),
            "--from".as_ref(),
            src.as_os_str(),
            "--toc".as_ref(),
        ]);
        assert!(!result.status.success(), "{ext}");
        let err = String::from_utf8_lossy(&result.stderr);
        assert!(
            err.contains("목차 항목이 한 쪽을 넘습니다") && err.contains("해결:"),
            "{ext}: {err}"
        );
        assert!(!out.exists(), "{ext}");
    }
}

/// A numbered heading that is the document's first block (its paragraph carries the section
/// definitions, and under `--preset report` the page number) still becomes a strip, and with `--toc` the strip that starts the body (and so
/// carries the page-number restart) still reads back as its heading.
#[test]
fn a_first_block_strip_survives_definitions_and_the_page_restart() {
    let src = tmp("first.md");
    std::fs::write(&src, "### 1. 첫 절\n\n본문.\n\n### 2. 둘째 절\n\n본문.\n").unwrap();
    for (flags, label) in [
        (&["--section-strips"][..], "strips"),
        (&["--section-strips", "--toc"][..], "strips+toc"),
        // `--preset report` adds a page number (pgnp) to the first paragraph.
        (
            &["--section-strips", "--preset", "report"][..],
            "strips+report",
        ),
        (
            &["--section-strips", "--toc", "--preset", "report"][..],
            "strips+toc+report",
        ),
    ] {
        for ext in ["hwpx", "hwp"] {
            let out = tmp(&format!("first-{}.{ext}", label.replace('+', "-")));
            let mut args: Vec<&std::ffi::OsStr> = vec![
                "new".as_ref(),
                "-o".as_ref(),
                out.as_os_str(),
                "--from".as_ref(),
                src.as_os_str(),
            ];
            args.extend(flags.iter().map(std::ffi::OsStr::new));
            ok(&run(&args));
            let md = ok(&run(&[
                "cat".as_ref(),
                "--format".as_ref(),
                "markdown".as_ref(),
                out.as_os_str(),
            ]));
            assert!(
                md.lines().any(|l| l == "### 1. 첫 절"),
                "{label} {ext}\n{md}"
            );
            // The page-numbered preset's page number survives the strip (and the TOC).
            let doc = match ext {
                "hwp" => hwp5::read_document(&out).unwrap().document,
                _ => hwpx::read_document(&out).unwrap().document,
            };
            let pgnp = doc.sections[0]
                .paragraphs
                .iter()
                .flat_map(|p| &p.controls)
                .filter(|c| c.ctrl_id() == *b"pgnp")
                .count();
            let expected = usize::from(label.ends_with("report"));
            assert_eq!(pgnp, expected, "{label} {ext}: pgnp controls");
        }
    }
}

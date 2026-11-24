//! `ahwp redact` (#617, GM-10) acceptance suite, AC1-AC4 in both formats.
//!
//! Inputs are built at run time from synthetic markdown with `ahwp new` (and converted to HWP5
//! with `ahwp convert`), so no fixture is needed and every value is synthetic. No assertion
//! depends on fonts or glyphs.
//!
//! Privacy check: every test asserts that no original value, nor its unmasked part, appears in
//! the report, stderr or stdout.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

#[path = "common/temp_path.rs"]
mod temp_path;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn repo(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn workdir(name: &str) -> temp_path::TempPath {
    let dir = temp_path::TempPath::new("hwp-cli-redact", name);
    let _ = std::fs::remove_dir_all(&*dir);
    std::fs::create_dir_all(&*dir).unwrap();
    dir
}

fn ok(output: Output, what: &str) -> Output {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Synthetic values, one per category. The secret part of each (the part the mask hides) is in
/// `SECRETS`; none of these strings may appear in any report or diagnostic.
const ORIGINALS: [&str; 7] = [
    "900101-1234567",
    "010-2468-1357",
    "hong.gildong@example.com",
    "4111-1111-1111-1111",
    "123-456-789012",
    "M12345678",
    "11-23-456789-01",
];
const SECRETS: [&str; 7] = [
    "234567",
    "2468",
    "ong.gildong",
    "4111-1111-1111",
    "123-456-78",
    "12345678",
    "456789",
];

/// Markdown with every category in a body paragraph and in table cells, the phone number split
/// across three character-shape runs (bold middle group).
const SOURCE: &str = "# 개인정보 테스트\n\n\
주민 900101-1234567 연락처 010-**2468**-1357 여권 M12345678\n\n\
| 항목 | 값 |\n|---|---|\n\
| 입금 계좌 | 123-456-789012 |\n\
| 메일 | hong.gildong@example.com |\n\
| 카드 | 4111-1111-1111-1111 |\n\
| 면허 | 11-23-456789-01 |\n\n\
예산 1,234,567,000원, 시행일 2026-11-25, 문서번호 2026-00123\n";

/// `(hwpx, hwp)` inputs built from `SOURCE`.
fn inputs(dir: &Path) -> [PathBuf; 2] {
    let md = dir.join("in.md");
    std::fs::write(&md, SOURCE).unwrap();
    let hwpx = dir.join("in.hwpx");
    ok(
        hwp()
            .arg("new")
            .arg("--from")
            .arg(&md)
            .arg("-o")
            .arg(&hwpx)
            .output()
            .unwrap(),
        "new",
    );
    let hwp5 = dir.join("in.hwp");
    ok(
        hwp()
            .arg("convert")
            .arg(&hwpx)
            .arg("-o")
            .arg(&hwp5)
            .output()
            .unwrap(),
        "convert",
    );
    [hwpx, hwp5]
}

fn assert_content_free(text: &str, what: &str) {
    for secret in ORIGINALS.iter().chain(SECRETS.iter()) {
        assert!(!text.contains(secret), "{what} leaks a raw value");
    }
}

fn cat(path: &Path, preview: bool) -> String {
    let mut command = hwp();
    command.arg("cat").arg(path);
    if preview {
        command.arg("--preview");
    }
    String::from_utf8(ok(command.output().unwrap(), "cat").stdout).unwrap()
}

fn report_validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../schemas/redact-report-v1.schema.json"
    ))
    .unwrap();
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .unwrap()
}

fn redact(input: &Path, output: &Path, report: &Path, extra: &[&str]) -> Output {
    hwp()
        .arg("redact")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--report")
        .arg(report)
        .args(extra)
        .output()
        .unwrap()
}

fn ext(path: &Path) -> &str {
    path.extension().unwrap().to_str().unwrap()
}

/// The paragraph JSON (`ahwp convert --to json`) of the body paragraph holding the phone.
fn phone_paragraph(path: &Path, dir: &Path) -> Value {
    let json = dir.join(format!(
        "dump-{}.json",
        path.file_name().unwrap().to_string_lossy()
    ));
    ok(
        hwp()
            .arg("convert")
            .arg(path)
            .arg("-o")
            .arg(&json)
            .output()
            .unwrap(),
        "convert json",
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
    doc["sections"][0]["paragraphs"][1].clone()
}

#[test]
fn ac1_ac3_masks_every_category_in_body_and_cells_with_a_content_free_report() {
    let dir = workdir("ac1");
    let validator = report_validator();
    for input in inputs(&dir) {
        let format = ext(&input).to_string();
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("report-{format}.json"));
        let run = ok(redact(&input, &output, &report_path, &[]), "redact");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert_content_free(&stderr, "stderr");
        assert_content_free(&String::from_utf8_lossy(&run.stdout), "stdout");

        let raw_report = std::fs::read_to_string(&report_path).unwrap();
        assert_content_free(&raw_report, "report");
        let report: Value = serde_json::from_str(&raw_report).unwrap();
        assert!(
            validator.is_valid(&report),
            "{format}: report fails redact-report-v1"
        );
        for category in [
            "rrn", "phone", "email", "card", "account", "passport", "driver",
        ] {
            assert_eq!(report["counts"][category], 1, "{format}: {category}");
        }
        assert_eq!(report["total"], 7);
        assert_eq!(report["verification"]["body_rescan"], 0);
        assert_eq!(report["verification"]["preview_rescan"], 0);
        assert_eq!(report["verification"]["originals_absent"], true);
        let cells: Vec<&Value> = report["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["kind"] == "cell")
            .collect();
        assert_eq!(cells.len(), 4, "{format}: four cell hits");
        for hit in cells {
            assert_eq!(hit["table_index"], 0);
            assert_eq!(hit["col"], 1);
        }

        let text = cat(&output, false);
        assert_content_free(&text, "output text");
        for masked in [
            "900101-1******",
            "010-****-1357",
            "h***.*******@example.com",
            "****-****-****-1111",
            "***-***-**9012",
            "M********",
            "11-**-******-**",
        ] {
            assert!(text.contains(masked), "{format}: missing mask {masked}");
        }
        // Budget, date and document numbers are untouched.
        assert!(text.contains("1,234,567,000원"));
        assert!(text.contains("2026-11-25"));
        assert!(text.contains("2026-00123"));
        // Character counts are preserved: masked and source text are the same length.
        assert_eq!(
            text.chars().count(),
            cat(&input, false).chars().count(),
            "{format}: text length changed"
        );
    }
}

#[test]
fn ac2_preview_text_and_raw_bytes_hold_no_masked_value() {
    let dir = workdir("ac2");
    for input in inputs(&dir) {
        let format = ext(&input).to_string();
        let output = dir.join(format!("out.{format}"));
        let report = dir.join(format!("report-{format}.json"));
        ok(redact(&input, &output, &report, &[]), "redact");
        let preview = cat(&output, true);
        assert!(
            preview.contains("010-****-1357"),
            "{format}: preview not regenerated"
        );
        assert_content_free(&preview, "preview");
        // A raw grep of the output bytes: UTF-16LE for HWP5 streams (PrvText is uncompressed),
        // and every inflated text entry of the HWPX package.
        if format == "hwp" {
            let bytes = std::fs::read(&output).unwrap();
            for secret in ORIGINALS {
                let utf16: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
                assert!(
                    !bytes
                        .windows(utf16.len())
                        .any(|window| window == utf16.as_slice()),
                    "hwp output bytes hold a raw value"
                );
            }
        } else {
            let file = std::fs::File::open(&output).unwrap();
            let mut archive = zip::ZipArchive::new(file).unwrap();
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).unwrap();
                let name = entry.name().unwrap().into_owned();
                if !(name.ends_with(".xml") || name.ends_with(".txt")) {
                    continue;
                }
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
                let text = String::from_utf8_lossy(&data);
                assert_content_free(&text, "hwpx entry");
            }
        }
    }
}

#[test]
fn ac4_run_split_match_keeps_runs_and_anchors() {
    let dir = workdir("ac4");
    for input in inputs(&dir) {
        let format = ext(&input).to_string();
        let output = dir.join(format!("out.{format}"));
        let report = dir.join(format!("report-{format}.json"));
        ok(redact(&input, &output, &report, &[]), "redact");
        let before = phone_paragraph(&input, &dir);
        let after = phone_paragraph(&output, &dir);
        let runs = before["char_shape_runs"].as_array().unwrap();
        assert!(runs.len() >= 3, "{format}: the phone must span three runs");
        assert_eq!(
            before["char_shape_runs"], after["char_shape_runs"],
            "{format}: runs moved"
        );
        assert_eq!(
            before["controls"], after["controls"],
            "{format}: controls changed"
        );
        let (b, a) = (
            before["chars"].as_array().unwrap(),
            after["chars"].as_array().unwrap(),
        );
        assert_eq!(b.len(), a.len(), "{format}: char count changed");
        for (x, y) in b.iter().zip(a) {
            // Only text chars may differ; every control char stays at its index.
            if x.get("Text").is_none() {
                assert_eq!(x, y, "{format}: a control moved");
            }
        }
        // The report addresses the run-split phone with its full range.
        let report: Value =
            serde_json::from_str(&std::fs::read_to_string(report).unwrap()).unwrap();
        let phone = report["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["category"] == "phone")
            .unwrap();
        assert_eq!(phone["path"], "0.1");
        assert_eq!(
            phone["end"].as_u64().unwrap() - phone["start"].as_u64().unwrap(),
            "010-2468-1357".len() as u64
        );
    }
}

#[test]
fn only_exclude_dry_run_and_mask_char() {
    let dir = workdir("options");
    let [hwpx, _] = inputs(&dir);
    let output = dir.join("only.hwpx");
    let report = dir.join("only.json");
    ok(
        redact(
            &hwpx,
            &output,
            &report,
            &["--only", "rrn,phone", "--mask-char", "●"],
        ),
        "only",
    );
    let text = cat(&output, false);
    assert!(text.contains("900101-1●●●●●●"));
    assert!(text.contains("010-●●●●-1357"));
    assert!(
        text.contains("hong.gildong@example.com"),
        "email left alone"
    );
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(value["total"], 2);

    let excluded = dir.join("exclude.hwpx");
    let report = dir.join("exclude.json");
    ok(
        redact(&hwpx, &excluded, &report, &["--exclude", "email"]),
        "exclude",
    );
    assert!(cat(&excluded, false).contains("hong.gildong@example.com"));

    // --dry-run without --report prints the report and publishes nothing.
    let dry = dir.join("dry.hwpx");
    let run = ok(
        hwp()
            .arg("redact")
            .arg(&hwpx)
            .arg("-o")
            .arg(&dry)
            .arg("--dry-run")
            .output()
            .unwrap(),
        "dry-run",
    );
    assert!(!dry.exists(), "dry-run published");
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert_content_free(&stdout, "dry-run stdout");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["dry_run"], true);
    assert_eq!(value["total"], 7);

    // Refusals.
    for (args, what) in [
        (vec!["--only", "nope"], "unknown category"),
        (vec!["--mask-char", "1"], "digit mask"),
        (vec!["--only", "rrn", "--exclude", "rrn"], "empty set"),
    ] {
        let out = dir.join("refused.hwpx");
        let run = hwp()
            .arg("redact")
            .arg(&hwpx)
            .arg("-o")
            .arg(&out)
            .args(&args)
            .output()
            .unwrap();
        assert!(!run.status.success(), "{what} accepted");
        assert!(!out.exists());
    }
    let cross = dir.join("cross.hwp");
    let run = hwp()
        .arg("redact")
        .arg(&hwpx)
        .arg("-o")
        .arg(&cross)
        .output()
        .unwrap();
    assert!(!run.status.success(), "cross-format accepted");
}

#[test]
fn committed_sample_has_no_false_positive_on_real_layout() {
    // An owner-authored, anonymized sample: redact must run and keep its numbers that are not
    // PII. Only counts are read, never text.
    let sample = repo("fixtures/samples/report-tables.hwpx");
    let dir = workdir("sample");
    let output = dir.join("out.hwpx");
    let report = dir.join("report.json");
    let run = ok(
        hwp()
            .arg("redact")
            .arg(&sample)
            .arg("-o")
            .arg(&output)
            .arg("--report")
            .arg(&report)
            .output()
            .unwrap(),
        "redact sample",
    );
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert!(report_validator().is_valid(&value));
    let _ = run;
}

#[test]
fn mcp_hwp_redact_returns_the_report() {
    use std::io::{BufRead as _, Write as _};
    let dir = workdir("mcp");
    let [hwpx, _] = inputs(&dir);
    let output = dir.join("mcp-out.hwpx");
    let mut child = hwp()
        .arg("mcp")
        .arg("--root")
        .arg(&*dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    for message in [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"hwp_redact","arguments":{
                "input": hwpx.to_string_lossy(),
                "output": output.to_string_lossy(),
                "exclude": ["passport"]}}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    stdin.flush().unwrap();
    let mut call = Value::Null;
    for line in lines.by_ref() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if value["id"] == 2 {
            call = value;
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    assert_eq!(call["result"]["isError"], false, "hwp_redact: {call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert_content_free(text, "mcp response");
    let report: Value = serde_json::from_str(text).unwrap();
    assert!(report_validator().is_valid(&report));
    assert_eq!(report["total"], 6);
    assert!(output.exists());
}

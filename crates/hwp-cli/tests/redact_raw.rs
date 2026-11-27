//! `ahwp redact` on raw-backed and side-channel content (#617 review I1-I3, I5, I7).
//!
//! Every input is built at run time from synthetic values. The HWP5 inputs come from
//! `ahwp convert`, whose reader gives each header, footer, note and text box a raw record subtree
//! (`raw_children`) that the HWP5 writer re-emits verbatim, so these tests run the raw
//! `PARA_TEXT` rewrite, the writer emission of the masked records and the raw branch of the
//! rescan. Each test reads the output bytes directly, never only the model text, so a value left
//! in a raw record, a field command, an object description or a free CFB sector fails it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

#[path = "common/temp_path.rs"]
mod temp_path;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn workdir(name: &str) -> temp_path::TempPath {
    let dir = temp_path::TempPath::new("hwp-cli-redact-raw", name);
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

fn run(args: &[&std::ffi::OsStr]) -> Output {
    hwp().args(args).output().unwrap()
}

/// `(hwpx, hwp)` built from a DocumentSpec v1 JSON (`ahwp compose`) or a markdown/IR JSON file
/// (`ahwp new --from`).
fn build(dir: &Path, name: &str, source: &str, compose: bool) -> [PathBuf; 2] {
    let ext = if compose { "spec.json" } else { "json" };
    let src = dir.join(format!("{name}.{ext}"));
    std::fs::write(&src, source).unwrap();
    build_from(dir, name, &src, compose)
}

fn build_from(dir: &Path, name: &str, src: &Path, compose: bool) -> [PathBuf; 2] {
    let hwpx = dir.join(format!("{name}.hwpx"));
    let out = if compose {
        run(&[
            "compose".as_ref(),
            src.as_os_str(),
            "-o".as_ref(),
            hwpx.as_os_str(),
        ])
    } else {
        run(&[
            "new".as_ref(),
            "--from".as_ref(),
            src.as_os_str(),
            "-o".as_ref(),
            hwpx.as_os_str(),
        ])
    };
    ok(out, "build hwpx");
    let hwp5 = dir.join(format!("{name}.hwp"));
    ok(
        run(&[
            "convert".as_ref(),
            hwpx.as_os_str(),
            "-o".as_ref(),
            hwp5.as_os_str(),
        ]),
        "convert hwp",
    );
    [hwpx, hwp5]
}

fn redact(input: &Path, output: &Path, report: &Path) -> Output {
    run(&[
        "redact".as_ref(),
        input.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--report".as_ref(),
        report.as_os_str(),
    ])
}

fn report(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn kinds(report: &Value) -> Vec<String> {
    report["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap().to_string())
        .collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|window| window == needle)
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Every byte view of a written file: the file itself (free CFB sectors included), every HWP5
/// stream as stored and inflated, every HWPX entry inflated.
fn byte_views(path: &Path) -> Vec<Vec<u8>> {
    let mut views = vec![std::fs::read(path).unwrap()];
    if path.extension().unwrap() == "hwp" {
        let mut cfb = cfb::open(path).unwrap();
        let streams: Vec<PathBuf> = cfb
            .walk()
            .filter(|entry| entry.is_stream())
            .map(|entry| entry.path().to_path_buf())
            .collect();
        for stream in streams {
            let mut raw = Vec::new();
            std::io::Read::read_to_end(&mut cfb.open_stream(&stream).unwrap(), &mut raw).unwrap();
            let mut inflated = Vec::new();
            if std::io::Read::read_to_end(
                &mut flate2::read::DeflateDecoder::new(raw.as_slice()),
                &mut inflated,
            )
            .is_ok()
            {
                views.push(inflated);
            }
            views.push(raw);
        }
    } else {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        for index in 0..archive.len() {
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut archive.by_index(index).unwrap(), &mut data).unwrap();
            views.push(data);
        }
    }
    views
}

/// Asserts that no secret occurs in any byte view of `path`, in UTF-8 or UTF-16LE.
fn assert_absent_everywhere(path: &Path, secrets: &[&str]) {
    let views = byte_views(path);
    for secret in secrets {
        for view in &views {
            assert!(
                !contains(view, secret.as_bytes()) && !contains(view, &utf16(secret)),
                "{}: a masked value survives in the output bytes",
                path.display()
            );
        }
    }
}

/// Asserts that `needle` occurs somewhere in the input's byte views: a guard that the test
/// value really is stored where the test claims, so an absent value proves masking.
fn assert_present_somewhere(path: &Path, needle: &str) {
    let found = byte_views(path)
        .iter()
        .any(|view| contains(view, needle.as_bytes()) || contains(view, &utf16(needle)));
    assert!(
        found,
        "{}: the test value is not in the input",
        path.display()
    );
}

const HEADER_FOOTER_SPEC: &str = r#"{
  "version": "1.0",
  "sections": [{
    "header": {"default": [{"type": "paragraph", "runs": [{"type": "text", "text": "머리말 담당 010-2468-1357"}]}]},
    "footer": {"default": [{"type": "paragraph", "runs": [{"type": "text", "text": "꼬리말 hong.gildong@example.com"}]}]},
    "blocks": [{"type": "paragraph", "runs": [{"type": "text", "text": "본문"}]}]
  }]
}"#;

/// I7: header and footer. In HWP5 both are raw-backed (`raw_children`), so the masked bytes
/// reach the file only through the raw `PARA_TEXT` rewrite.
#[test]
fn header_and_footer_are_masked_in_both_formats() {
    let dir = workdir("headfoot");
    for input in build(&dir, "hf", HEADER_FOOTER_SPEC, true) {
        let format = input.extension().unwrap().to_string_lossy().to_string();
        assert_present_somewhere(&input, "2468");
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("r-{format}.json"));
        ok(redact(&input, &output, &report_path), "redact");
        let value = report(&report_path);
        assert_eq!(value["counts"]["phone"], 1, "{format}");
        assert_eq!(value["counts"]["email"], 1, "{format}");
        assert_eq!(
            kinds(&value),
            ["header_footer", "header_footer"],
            "{format}"
        );
        if format == "hwp" {
            assert_eq!(
                value["raw_record_hits"], 2,
                "hwp: the raw header/footer records were masked"
            );
        }
        assert_eq!(value["verification"]["originals_absent"], true);
        assert_absent_everywhere(&output, &["010-2468-1357", "hong.gildong@example.com"]);
        let text = String::from_utf8(
            ok(
                run(&[
                    "cat".as_ref(),
                    output.as_os_str(),
                    "--with-header-footer".as_ref(),
                ]),
                "cat",
            )
            .stdout,
        )
        .unwrap();
        assert!(text.contains("010-****-1357"), "{format}: header mask");
        assert!(
            text.contains("h***.*******@example.com"),
            "{format}: footer mask"
        );
    }
}

/// I1 and I7: a hyperlink whose `mailto:` command holds an address, and a footnote holding a
/// parenthesized landline (I8). The command is not display text; it is emitted from the field
/// control's CTRL_HEADER payload (HWP5) and as the `Command` parameter (HWPX).
#[test]
fn hyperlink_command_and_footnote_are_masked() {
    let dir = workdir("link-note");
    let md = dir.join("link.md");
    std::fs::write(
        &md,
        "# 안내\n\n문의 [담당자](mailto:kim.cheolsu@example.com) 참고[^1]\n\n[^1]: 대표 (02)2100-1234\n",
    )
    .unwrap();
    for input in build_from(&dir, "link", &md, false) {
        let format = input.extension().unwrap().to_string_lossy().to_string();
        assert_present_somewhere(&input, "kim.cheolsu");
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("r-{format}.json"));
        ok(redact(&input, &output, &report_path), "redact");
        let value = report(&report_path);
        let mut got = kinds(&value);
        got.sort();
        assert_eq!(got, ["field_command", "note"], "{format}");
        assert_eq!(value["counts"]["email"], 1, "{format}");
        assert_eq!(value["counts"]["phone"], 1, "{format}");
        assert_absent_everywhere(&output, &["kim.cheolsu", "2100-1234"]);
        assert!(
            report_validator().is_valid(&value),
            "{format}: report fails the schema"
        );
    }
}

/// I7: a text box and its accessible description. HWPX keeps the box as a shape whose
/// `hp:shapeComment` carries the title; the HWP5 conversion flattens the box into body text.
#[test]
fn text_box_and_its_description_are_masked() {
    let dir = workdir("textbox");
    let spec = r#"{
      "version": "2.0",
      "document": {"version": "1.0", "sections": [{"blocks": [
        {"type": "paragraph", "runs": [{"type": "text", "text": "본문"}]}]}]},
      "visuals": [{"id": "tb1", "location": {"section": 0, "paragraph": 0}, "alt": "상자",
        "width_mm": 60, "height_mm": 20, "title": "작성 kim.younghee@example.com",
        "content": {"type": "text_box", "text": "상자 900101-1234567"}}]
    }"#;
    for input in build(&dir, "tb", spec, true) {
        let format = input.extension().unwrap().to_string_lossy().to_string();
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("r-{format}.json"));
        ok(redact(&input, &output, &report_path), "redact");
        let value = report(&report_path);
        assert_eq!(value["counts"]["rrn"], 1, "{format}");
        if format == "hwpx" {
            assert_present_somewhere(&input, "kim.younghee");
            let mut got = kinds(&value);
            got.sort();
            assert_eq!(got, ["object", "object_description"]);
        }
        assert_absent_everywhere(&output, &["1234567", "kim.younghee"]);
    }
}

/// I7: a table caption, written through the JSON IR (no markdown syntax makes one).
#[test]
fn table_caption_is_masked() {
    let dir = workdir("caption");
    let md = dir.join("t.md");
    std::fs::write(&md, "| 항목 | 값 |\n|---|---|\n| 1 | 2 |\n").unwrap();
    let base = dir.join("t.hwpx");
    ok(
        run(&[
            "new".as_ref(),
            "--from".as_ref(),
            md.as_os_str(),
            "-o".as_ref(),
            base.as_os_str(),
        ]),
        "new",
    );
    let json = dir.join("t.json");
    ok(
        run(&[
            "convert".as_ref(),
            base.as_os_str(),
            "-o".as_ref(),
            json.as_os_str(),
        ]),
        "to json",
    );
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let table = find_table(&mut doc).expect("a table");
    let mut paragraph = table["cells"][0]["paragraphs"][0].clone();
    let end = paragraph["chars"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let mut chars: Vec<Value> = "표 1 담당 010-2468-1357"
        .chars()
        .map(|c| serde_json::json!({"Text": c.to_string()}))
        .collect();
    if end.get("Text").is_none() {
        chars.push(end);
    }
    paragraph["chars"] = Value::Array(chars);
    paragraph["controls"] = Value::Array(Vec::new());
    paragraph["line_segs"] = Value::Array(Vec::new());
    table["caption"] = serde_json::json!({
        "side": "Top", "direction": "Horizontal", "gap": 850, "width": null,
        "last_width": 0, "paragraphs": [paragraph]
    });
    let source = dir.join("tc.json");
    std::fs::write(&source, serde_json::to_vec(&doc).unwrap()).unwrap();
    for input in build_from(&dir, "tc", &source, false) {
        let format = input.extension().unwrap().to_string_lossy().to_string();
        assert_present_somewhere(&input, "2468-1357");
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("r-{format}.json"));
        ok(redact(&input, &output, &report_path), "redact");
        let value = report(&report_path);
        assert_eq!(kinds(&value), ["caption"], "{format}");
        assert_absent_everywhere(&output, &["010-2468-1357"]);
    }
}

fn find_table(value: &mut Value) -> Option<&mut Value> {
    match value {
        Value::Object(map) => {
            if map.contains_key("Table") {
                return map.get_mut("Table");
            }
            map.values_mut().find_map(find_table)
        }
        Value::Array(items) => items.iter_mut().find_map(find_table),
        _ => None,
    }
}

/// I3: the HWP5 output is a compacted container. A copied or in-place rewritten CFB keeps freed
/// sectors as they were: a stream the source once held (an earlier unmasked section, an old
/// preview) stays readable in the file even though no directory entry points to it. The input
/// here is built uncompressed and given such a free-sector remnant: a stream holding the value
/// in UTF-16, then removed. The output's raw bytes must not hold it.
#[test]
fn hwp5_output_keeps_no_free_sector_with_the_original() {
    let dir = workdir("free-sectors");
    let [_, compressed] = build(&dir, "fs", HEADER_FOOTER_SPEC, true);
    let input = dir.join("uncompressed.hwp");
    decompress_container(&compressed, &input);
    {
        let mut cfb = cfb::open_rw(&input).unwrap();
        let remnant = utf16(&"010-2468-1357 ".repeat(400));
        std::io::Write::write_all(&mut cfb.create_stream("/Remnant").unwrap(), &remnant).unwrap();
        cfb.flush().unwrap();
        cfb.remove_stream("/Remnant").unwrap();
        cfb.flush().unwrap();
    }
    // The value is readable as UTF-16 in the raw input file, outside any live stream.
    assert!(contains(
        &std::fs::read(&input).unwrap(),
        &utf16("010-2468-1357")
    ));
    let output = dir.join("out.hwp");
    let report_path = dir.join("r.json");
    ok(redact(&input, &output, &report_path), "redact");
    let raw = std::fs::read(&output).unwrap();
    for secret in ["010-2468-1357", "hong.gildong@example.com"] {
        assert!(
            !contains(&raw, &utf16(secret)),
            "a masked value survives in the raw HWP5 file (free sector)"
        );
    }
    assert_absent_everywhere(&output, &["010-2468-1357", "hong.gildong@example.com"]);
}

/// Rewrites `source` as an uncompressed HWP5 file: FileHeader attribute bit 0 cleared and every
/// record stream (DocInfo, BodyText) inflated.
fn decompress_container(source: &Path, target: &Path) {
    let mut from = cfb::open(source).unwrap();
    let entries: Vec<(PathBuf, bool)> = from
        .walk()
        .map(|entry| (entry.path().to_path_buf(), entry.is_stream()))
        .collect();
    let mut to = cfb::CompoundFile::create_with_version(
        cfb::Version::V3,
        std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(target)
            .unwrap(),
    )
    .unwrap();
    for (path, is_stream) in entries {
        let name = path.to_string_lossy().replace('\\', "/");
        if name == "/" {
            continue;
        }
        if !is_stream {
            to.create_storage(&path).unwrap();
            continue;
        }
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut from.open_stream(&path).unwrap(), &mut data).unwrap();
        if name == "/FileHeader" {
            data[36] &= !1;
        } else if name == "/DocInfo" || name.starts_with("/BodyText/") {
            let mut inflated = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::DeflateDecoder::new(data.as_slice()),
                &mut inflated,
            )
            .unwrap();
            data = inflated;
        }
        std::io::Write::write_all(&mut to.create_stream(&path).unwrap(), &data).unwrap();
    }
    to.flush().unwrap();
}

/// I5: a run with nothing to mask still publishes `-o` (a verified copy), exits 0 and says so.
#[test]
fn clean_input_publishes_a_verified_copy() {
    let dir = workdir("clean");
    let md = dir.join("clean.md");
    std::fs::write(&md, "# 계획\n\n예산 1,234,567,000원, 시행일 2026-11-25\n").unwrap();
    for input in build_from(&dir, "clean", &md, false) {
        let format = input.extension().unwrap().to_string_lossy().to_string();
        let output = dir.join(format!("out.{format}"));
        let report_path = dir.join(format!("r-{format}.json"));
        let out = ok(redact(&input, &output, &report_path), "redact");
        assert!(output.exists(), "{format}: a clean run must publish -o");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("검증된 사본"),
            "{format}: stderr says a copy was published"
        );
        let value = report(&report_path);
        assert_eq!(value["total"], 0);
        assert_eq!(value["verification"]["originals_absent"], true);
    }
}

/// --report may not alias the input or the output: it would overwrite the source document.
#[test]
fn report_path_may_not_alias_input_or_output() {
    let dir = workdir("alias");
    let [hwpx, _] = build(&dir, "a", HEADER_FOOTER_SPEC, true);
    let before = std::fs::read(&hwpx).unwrap();
    let output = dir.join("out.hwpx");
    let refused = redact(&hwpx, &output, &hwpx);
    assert!(!refused.status.success(), "--report = input accepted");
    assert_eq!(
        std::fs::read(&hwpx).unwrap(),
        before,
        "the input was overwritten"
    );
    assert!(!output.exists());
    let refused = redact(&hwpx, &output, &output);
    assert!(!refused.status.success(), "--report = output accepted");
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

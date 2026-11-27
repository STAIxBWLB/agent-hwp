//! GJ-2 read-only HWPML (`.hml`) ingestion and legacy-signature detection (issue #622).
//!
//! CI-safe by construction: every input is a synthetic document written into a temp dir from
//! the public HWPML element structure (HWPML rev1.2 Part II). No genuine Hancom-saved `.hml`
//! or third-party legacy file is committed or read. Nothing asserts on font-dependent output.

use std::process::{Command, Output};

#[path = "common/temp_path.rs"]
mod temp_path;
use temp_path::TempPath;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn tmp(name: &str) -> TempPath {
    TempPath::new("hwp-cli-hml", name)
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    hwp().args(args).output().expect("ahwp 실행")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A 1x1 RGB PNG, built in the test so no binary blob is committed.
fn tiny_png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    // Stored (uncompressed) zlib block holding one scanline: filter 0 + RGB.
    let raw = [0u8, 0xFF, 0x00, 0x00];
    let mut zlib = vec![0x78, 0x01, 0x01, 0x04, 0x00, 0xFB, 0xFF];
    zlib.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn synthetic_hml() -> String {
    let png = hwp_convert::base64::encode(&tiny_png());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<HWPML Version="2.8" SubVersion="8.0.0.0" Style2="embed">
<HEAD SecCnt="1"><DOCSUMMARY><TITLE>합성 제목</TITLE><AUTHOR>합성 작성자</AUTHOR></DOCSUMMARY>
<MAPPINGTABLE><BINDATALIST Count="1"><BINITEM BinData="7" Format="png" Type="Embedding"/></BINDATALIST></MAPPINGTABLE></HEAD>
<BODY><SECTION Id="0">
<P ParaShape="0" Style="0"><TEXT CharShape="0"><SECDEF/><CHAR>첫 문단 A&amp;B</CHAR></TEXT></P>
<P><TEXT CharShape="0"><CHAR>앞</CHAR><TAB/><CHAR>뒤</CHAR><LINEBREAK/><CHAR>둘째 줄</CHAR></TEXT></P>
<P><TEXT CharShape="0"><TABLE RowCount="2" ColCount="2"><SHAPEOBJECT><SIZE Width="1000" Height="500"/></SHAPEOBJECT>
<ROW><CELL ColAddr="0" RowAddr="0" ColSpan="2" RowSpan="1"><PARALIST><P><TEXT><CHAR>병합 셀</CHAR></TEXT></P></PARALIST></CELL></ROW>
<ROW><CELL ColAddr="0" RowAddr="1"><PARALIST><P><TEXT><CHAR>왼쪽</CHAR></TEXT></P></PARALIST></CELL>
<CELL ColAddr="1" RowAddr="1"><PARALIST><P><TEXT><CHAR>오른쪽</CHAR></TEXT></P></PARALIST></CELL></ROW>
</TABLE></TEXT></P>
<P><TEXT CharShape="0"><PICTURE><SHAPEOBJECT/><IMAGE BinItem="1"/></PICTURE><CHAR>그림 뒤</CHAR><EQUATION><SCRIPT>x</SCRIPT></EQUATION></TEXT></P>
</SECTION></BODY>
<TAIL><BINDATASTORAGE><BINDATA Id="7" Encoding="Base64">{png}</BINDATA></BINDATASTORAGE></TAIL>
</HWPML>"#
    )
}

fn write_hml(name: &str) -> TempPath {
    let path = tmp(name);
    std::fs::write(&path, synthetic_hml()).unwrap();
    path
}

#[test]
fn cat_reads_hml_paragraphs_tables_and_entities() {
    let input = write_hml("cat.hml");
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    for needle in [
        "첫 문단 A&B",
        "앞\t뒤",
        "둘째 줄",
        "병합 셀",
        "왼쪽\t오른쪽",
        "그림 뒤",
    ] {
        assert!(text.contains(needle), "{needle:?} missing in:\n{text}");
    }
    // Unsupported content is named, never dropped silently.
    assert!(stderr(&out).contains("<EQUATION>"), "{}", stderr(&out));
}

#[test]
fn json_ir_carries_meta_metadata_spans_and_bindata() {
    let input = write_hml("ir.hml");
    let out = run(&[
        "cat".as_ref(),
        input.as_os_str(),
        "--format".as_ref(),
        "json".as_ref(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let ir: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ir["meta"]["source_format"], "hml");
    assert_eq!(ir["meta"]["source_version"], "2.8");
    assert_eq!(ir["metadata"]["title"], "합성 제목");
    assert_eq!(ir["metadata"]["author"], "합성 작성자");
    let bins = ir["bin_streams"].as_array().expect("bin_streams");
    assert_eq!(bins.len(), 1, "BINDATA must be decoded into one bin stream");
    let ir_text = ir.to_string();
    assert!(ir_text.contains("\"col_span\":2"), "merged cell span lost");
}

#[test]
fn convert_hml_to_native_formats_reads_back() {
    let input = write_hml("conv.hml");
    for ext in ["hwpx", "hwp"] {
        let output = tmp(&format!("conv-out.{ext}"));
        let out = run(&[
            "convert".as_ref(),
            input.as_os_str(),
            "-o".as_ref(),
            output.as_os_str(),
        ]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
        let back = run(&["cat".as_ref(), output.as_os_str()]);
        assert!(back.status.success(), "{ext}: {}", stderr(&back));
        let text = stdout(&back);
        assert!(
            text.contains("병합 셀") && text.contains("그림 뒤"),
            "{ext}:\n{text}"
        );
    }
}

#[test]
fn info_reports_legacy_read_only() {
    let input = write_hml("info.hml");
    let out = run(&["info".as_ref(), input.as_os_str(), "--json".as_ref()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["format"], "hml");
    assert_eq!(v["read_only"], true);
    assert_eq!(v["metadata"]["title"], "합성 제목");
}

#[test]
fn edit_refuses_hml_and_leaves_source_untouched() {
    let input = write_hml("edit.hml");
    let before = std::fs::read(&*input).unwrap();
    let output = tmp("edit-out.hwpx");
    let out = run(&[
        "edit".as_ref(),
        input.as_os_str(),
        "--replace".as_ref(),
        "왼쪽=>X".as_ref(),
        "-o".as_ref(),
        output.as_os_str(),
    ]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("읽기 전용") && err.contains("ahwp convert"),
        "{err}"
    );
    assert!(!output.exists());
    assert_eq!(std::fs::read(&*input).unwrap(), before);
}

#[test]
fn hml_detection_is_by_content_not_extension() {
    let input = tmp("renamed.hwp");
    std::fs::write(&input, synthetic_hml()).unwrap();
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("첫 문단"));
}

#[test]
fn utf16_bom_hml_is_decoded() {
    let input = tmp("utf16.hml");
    let text = synthetic_hml().replace("encoding=\"UTF-8\"", "encoding=\"UTF-16\"");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&input, bytes).unwrap();
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("병합 셀"));
}

#[test]
fn legacy_hwp_signatures_get_typed_refusals() {
    let v2 = tmp("v2.hwp");
    std::fs::write(&v2, b"HWP Document File V2.00 \x1a\x01\x02\x03\x04\x05").unwrap();
    let out = run(&["cat".as_ref(), v2.as_os_str()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("HWP 2.00") && err.contains("다시 저장"),
        "{err}"
    );

    // The complete 30-byte V3.00 signature routes to the HWP 3.x reader (#114); a bare
    // signature with no body is a read failure with a recovery step, not a refusal.
    let v3 = tmp("v3.hwp");
    std::fs::write(&v3, b"HWP Document File V3.00 \x1a\x01\x02\x03\x04\x05").unwrap();
    let out = run(&["cat".as_ref(), v3.as_os_str()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("HWP 3.x 읽기 실패") && err.contains("다시 저장"),
        "{err}"
    );

    // A V3.10 revision, a V3.00 prefix with a different trailer, and a cut-off signature are
    // each refused with their own cause, never as the valid HWP 3.x input.
    let cases: [(&str, &[u8], &str); 3] = [
        (
            "v31.hwp",
            b"HWP Document File V3.10 \x1a\x01\x02\x03\x04\x05",
            "HWP 3.10",
        ),
        (
            "v3bad.hwp",
            b"HWP Document File V3.00 \x1a\x09\x09\x09\x09\x09",
            "끝 6바이트",
        ),
        ("cut.hwp", b"HWP Document File V3.0", "잘려 있습니다"),
    ];
    for (name, bytes, cause) in cases {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let out = run(&["cat".as_ref(), path.as_os_str()]);
        assert!(!out.status.success(), "{name}");
        let err = stderr(&out);
        assert!(err.contains(cause), "{name}: {err}");
        assert!(err.contains("다시 저장"), "{name}: no recovery: {err}");
        assert!(
            !err.contains("HWP 3.x 읽기"),
            "{name} read as valid HWP 3.x: {err}"
        );
    }
}

#[test]
fn render_hml_produces_pages() {
    let input = write_hml("render.hml");
    let output = tmp("render-out.svg");
    let report = tmp("render-report.json");
    let out = run(&[
        "render".as_ref(),
        "--format".as_ref(),
        "svg".as_ref(),
        "--report".as_ref(),
        report.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        input.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    // Structure only: glyphs and page counts depend on the host fonts.
    let svg = std::fs::read_to_string(&*output).unwrap();
    assert!(svg.contains("<svg"), "no svg root");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&*report).unwrap()).unwrap();
    assert_eq!(report["contract"], "hwp-render-report-v1");
}

#[test]
fn grep_finds_hml_body_and_cell_text() {
    let input = write_hml("grep.hml");
    for needle in ["둘째 줄", "오른쪽"] {
        let out = run(&["grep".as_ref(), needle.as_ref(), input.as_os_str()]);
        assert!(out.status.success(), "{needle}: {}", stderr(&out));
        assert!(stdout(&out).contains(needle), "{needle}: {}", stdout(&out));
    }
    let out = run(&["grep".as_ref(), "없는 문자열".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
}

#[test]
fn non_hml_xml_keeps_the_signature_error() {
    let input = tmp("other.xml");
    std::fs::write(&input, "<?xml version=\"1.0\"?><html><p>x</p></html>").unwrap();
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("시그니처"), "{}", stderr(&out));
}

#[test]
fn oversized_hml_is_refused_before_reading_it_whole() {
    // A sparse file one byte over the limit: the loader must refuse it from a bounded read.
    let input = tmp("huge.hml");
    let head = synthetic_hml();
    let file = std::fs::File::create(&*input).unwrap();
    use std::io::Write as _;
    (&file).write_all(head.as_bytes()).unwrap();
    file.set_len(hwpx::HML_LIMITS.max_input_bytes + 1).unwrap();
    drop(file);
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("상한") && err.contains("다시 저장"), "{err}");
}

/// Two SECTIONs, a blank paragraph and a TAB-only paragraph, reported through `info`.
#[test]
fn info_keeps_sections_and_blank_paragraphs() {
    let input = tmp("sections.hml");
    std::fs::write(
        &input,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\"><HEAD/><BODY>\
         <SECTION><P><TEXT><CHAR>첫 구역</CHAR></TEXT></P><P><TEXT/></P>\
         <P><TEXT><TAB/></TEXT></P></SECTION>\
         <SECTION><P><TEXT><CHAR>둘째 구역</CHAR></TEXT></P></SECTION></BODY></HWPML>",
    )
    .unwrap();
    let out = run(&["info".as_ref(), input.as_os_str(), "--json".as_ref()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["sections"], 2, "{v}");
    let out = run(&[
        "cat".as_ref(),
        "--format".as_ref(),
        "json".as_ref(),
        input.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let ir: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        ir["sections"][0]["paragraphs"].as_array().unwrap().len(),
        3,
        "blank and TAB-only paragraphs kept"
    );
}

#[test]
fn convert_reads_hml_from_stdin() {
    use std::io::Write as _;
    let output = tmp("stdin-out.hwpx");
    let mut child = hwp()
        .args(["convert", "-", "-o"])
        .arg(&*output)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(synthetic_hml().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let back = run(&["cat".as_ref(), output.as_os_str()]);
    assert!(stdout(&back).contains("병합 셀"), "{}", stdout(&back));
}

/// Stdin is bounded by the detected format's limit. The writer never closes the pipe and keeps
/// writing past the HWPML limit, so the run can only finish if the reader stops at the bound
/// and refuses (an unbounded read would block until the deadline and fail the test).
#[test]
fn convert_stdin_over_the_input_limit_is_refused() {
    use std::io::Write as _;
    let output = tmp("stdin-over.hwpx");
    let mut child = hwp()
        .args(["convert", "-", "-o"])
        .arg(&*output)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let head = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\">";
        if stdin.write_all(head.as_bytes()).is_err() {
            return;
        }
        let chunk = vec![b' '; 1 << 20];
        // Endless: only a broken pipe (the reader gave up) ends it.
        while stdin.write_all(&chunk).is_ok() {}
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("convert - kept reading stdin past the HWPML limit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    let limit = hwpx::HML_LIMITS.max_input_bytes.to_string();
    assert!(err.contains("stdin") && err.contains(&limit), "{err}");
    assert!(!output.exists());
}

/// Content wins over a `.json` extension, and `--preview` names the legacy format.
#[test]
fn json_extension_and_preview_route_through_legacy_detection() {
    let input = tmp("renamed.json");
    std::fs::write(&input, synthetic_hml()).unwrap();
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("병합 셀"));

    let hml = write_hml("preview.hml");
    let out = run(&["cat".as_ref(), "--preview".as_ref(), hml.as_os_str()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("HWPML") && err.contains("미리보기"), "{err}");
    assert!(!err.contains("시그니처 불일치"), "{err}");
}

/// A legacy reader's omissions are a typed loss: `--loss-report` records
/// `legacy_content_omitted` for every target and `--strict` refuses before publishing (#114).
#[test]
fn legacy_reader_losses_reach_strict_and_the_loss_report() {
    let input = tmp("lossy.hml");
    std::fs::write(
        &input,
        synthetic_hml().replace("<SECDEF/>", "<SECDEF/><EQUATION/>"),
    )
    .unwrap();
    for ext in ["md", "docx", "hwpx", "hwp"] {
        let output = tmp(&format!("lossy-out.{ext}"));
        let report = tmp(&format!("lossy-{ext}.json"));
        let out = run(&[
            "convert".as_ref(),
            input.as_os_str(),
            "-o".as_ref(),
            output.as_os_str(),
            "--loss-report".as_ref(),
            report.as_os_str(),
        ]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
        let ledger = std::fs::read_to_string(&*report).unwrap();
        assert!(ledger.contains("legacy_content_omitted"), "{ext}: {ledger}");

        let strict_out = tmp(&format!("lossy-strict.{ext}"));
        let out = run(&[
            "convert".as_ref(),
            input.as_os_str(),
            "-o".as_ref(),
            strict_out.as_os_str(),
            "--strict".as_ref(),
        ]);
        assert!(
            !out.status.success(),
            "{ext}: strict published a lossy legacy read"
        );
        assert!(
            stderr(&out).contains("legacy_content_omitted"),
            "{ext}: {}",
            stderr(&out)
        );
        assert!(!strict_out.exists(), "{ext}");
    }
}

/// A clean legacy read stays lossless, so `--strict` still converts it.
#[test]
fn clean_legacy_read_passes_strict() {
    let input = tmp("clean.hml");
    std::fs::write(
        &input,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\"><HEAD/><BODY><SECTION>\
         <P><TEXT><CHAR>깨끗한 문단</CHAR></TEXT></P></SECTION></BODY></HWPML>",
    )
    .unwrap();
    let output = tmp("clean-out.md");
    let out = run(&[
        "convert".as_ref(),
        input.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--strict".as_ref(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
}

/// Editing a legacy input is refused whatever the output format, before anything is written.
#[test]
fn edit_refuses_legacy_input_for_every_output_format() {
    let input = write_hml("edit-any.hml");
    for ext in ["md", "json", "hwpx", "hwp"] {
        let output = tmp(&format!("edit-any-out.{ext}"));
        let out = run(&[
            "edit".as_ref(),
            input.as_os_str(),
            "--replace".as_ref(),
            "왼쪽=>X".as_ref(),
            "-o".as_ref(),
            output.as_os_str(),
        ]);
        assert!(
            !out.status.success(),
            "{ext}: edit published from a legacy input"
        );
        let err = stderr(&out);
        assert!(
            err.contains("읽기 전용") && err.contains("ahwp convert"),
            "{ext}: {err}"
        );
        assert!(!output.exists(), "{ext}");
    }
}

/// The stdout path (`-o -`) runs the same legacy-loss gate as file output: `--strict` refuses a
/// lossy legacy read and prints nothing, and without `--strict` the text is still printed.
#[test]
fn stdout_convert_runs_the_legacy_loss_gate() {
    let input = tmp("lossy-stdout.hml");
    std::fs::write(
        &input,
        synthetic_hml().replace("<SECDEF/>", "<SECDEF/><EQUATION/>"),
    )
    .unwrap();
    for to in ["md", "txt", "json"] {
        let strict = run(&[
            "convert".as_ref(),
            input.as_os_str(),
            "-o".as_ref(),
            "-".as_ref(),
            "--to".as_ref(),
            to.as_ref(),
            "--strict".as_ref(),
        ]);
        assert!(
            !strict.status.success(),
            "{to}: strict stdout published a lossy read"
        );
        assert!(
            stderr(&strict).contains("legacy_content_omitted"),
            "{to}: {}",
            stderr(&strict)
        );
        assert!(strict.stdout.is_empty(), "{to}: printed before refusing");

        let lenient = run(&[
            "convert".as_ref(),
            input.as_os_str(),
            "-o".as_ref(),
            "-".as_ref(),
            "--to".as_ref(),
            to.as_ref(),
        ]);
        assert!(lenient.status.success(), "{to}: {}", stderr(&lenient));
        assert!(!lenient.stdout.is_empty(), "{to}");
    }
}

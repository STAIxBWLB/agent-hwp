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
    assert!(stderr(&out).contains("읽기 전용"), "{}", stderr(&out));
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
    assert!(stderr(&out).contains("HWP 2.x"), "{}", stderr(&out));

    let v3 = tmp("v3.hwp");
    std::fs::write(&v3, b"HWP Document File V3.00 \x1a\x01\x02\x03\x04\x05").unwrap();
    let out = run(&["cat".as_ref(), v3.as_os_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("HWP 3.x"), "{}", stderr(&out));
}

#[test]
fn non_hml_xml_keeps_the_signature_error() {
    let input = tmp("other.xml");
    std::fs::write(&input, "<?xml version=\"1.0\"?><html><p>x</p></html>").unwrap();
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("시그니처"), "{}", stderr(&out));
}

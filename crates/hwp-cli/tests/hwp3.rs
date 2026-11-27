//! HWP 3.x reading through the CLI (issue #622 T2): format detection routes the complete
//! V3.00 signature to `hwp5::hwp3`, so cat/info/grep/convert/render all read it.
//!
//! CI-safe by construction: every input is synthesized by the hwp5 test support from the
//! public HWP 3.0 layout. No genuine HWP 3.x file is committed or read, and nothing asserts on
//! font-dependent output.

use std::process::{Command, Output};

#[path = "common/temp_path.rs"]
mod temp_path;
use temp_path::TempPath;

#[allow(dead_code)]
#[path = "../../hwp5/tests/support/hwp3.rs"]
mod support;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn tmp(name: &str) -> TempPath {
    TempPath::new("hwp-cli-hwp3", name)
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

/// A plain one-paragraph document whose Johab text decodes to "한글".
fn simple() -> Vec<u8> {
    let mut bytes = support::prefix(false);
    support::shapes(&mut bytes);
    support::text_list(&mut bytes, &[0xd065, 0x8b69]);
    bytes
}

fn write(name: &str, bytes: &[u8]) -> TempPath {
    let path = tmp(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn cat_reads_hwp3_text_and_tables() {
    let input = write("simple.hwp", &simple());
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("한글"), "{}", stdout(&out));

    // The rich document carries a merged-cell table with cell text "A".
    let rich = write("rich.hwp", &support::rich_document());
    let out = run(&["cat".as_ref(), rich.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("한글") && text.contains('A'), "{text}");
}

#[test]
fn detection_is_by_content_not_extension() {
    let input = write("renamed.hwpx", &simple());
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("한글"));
}

#[test]
fn info_reports_hwp3_read_only() {
    let input = write("info.hwp", &simple());
    let out = run(&["info".as_ref(), input.as_os_str(), "--json".as_ref()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["format_label"], "HWP 3.x");
    assert_eq!(v["read_only"], true);
    assert!(v["sections"].as_u64().unwrap() >= 1);
}

#[test]
fn grep_finds_hwp3_text() {
    let input = write("grep.hwp", &support::rich_document());
    let out = run(&["grep".as_ref(), "한글".as_ref(), input.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("한글"));
    let out = run(&["grep".as_ref(), "없는 문자열".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
}

#[test]
fn convert_hwp3_to_native_formats_reads_back() {
    let input = write("conv.hwp", &support::rich_document());
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
        assert!(stdout(&back).contains("한글"), "{ext}: {}", stdout(&back));
    }
}

#[test]
fn convert_reads_hwp3_from_stdin() {
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
    child.stdin.take().unwrap().write_all(&simple()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let back = run(&["cat".as_ref(), output.as_os_str()]);
    assert!(stdout(&back).contains("한글"), "{}", stdout(&back));
}

#[test]
fn render_hwp3_produces_svg() {
    let input = write("render.hwp", &support::rich_document());
    let output = tmp("render-out.svg");
    let out = run(&[
        "render".as_ref(),
        "--format".as_ref(),
        "svg".as_ref(),
        "-o".as_ref(),
        output.as_os_str(),
        input.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    // Structure only: glyphs and page counts depend on the host fonts.
    assert!(std::fs::read_to_string(&*output).unwrap().contains("<svg"));
}

#[test]
fn password_protected_hwp3_is_refused_with_recovery() {
    let mut bytes = support::prefix(false);
    support::word_at(&mut bytes, 30 + 96, 1);
    support::shapes(&mut bytes);
    support::text_list(&mut bytes, &[0xd065, 0x8b69]);
    let input = write("locked.hwp", &bytes);
    for cmd in ["cat", "info"] {
        let out = run(&[cmd.as_ref(), input.as_os_str()]);
        assert!(!out.status.success(), "{cmd}");
        let err = stderr(&out);
        assert!(
            err.contains("암호") && err.contains("다시 저장"),
            "{cmd}: {err}"
        );
        assert!(!stdout(&out).contains("한글"), "{cmd} leaked body text");
    }
}

#[test]
fn truncated_hwp3_fails_with_recovery() {
    let mut bytes = simple();
    bytes.truncate(30 + 64);
    let input = write("cut.hwp", &bytes);
    let out = run(&["cat".as_ref(), input.as_os_str()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("HWP 3.x 읽기 실패") && err.contains("다시 저장"),
        "{err}"
    );
}

#[test]
fn edit_refuses_hwp3_and_points_at_convert() {
    let input = write("edit.hwp", &simple());
    let before = std::fs::read(&*input).unwrap();
    let output = tmp("edit-out.hwp");
    let out = run(&[
        "edit".as_ref(),
        input.as_os_str(),
        "--replace".as_ref(),
        "한글=>X".as_ref(),
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

/// HWP 3.x on stdin stages under the HWP 3.x reader's own 64 MiB bound, not the native 2 GiB
/// container bound: the writer never stops, so the run only finishes if staging refuses at
/// that bound (and the message names it).
#[test]
fn convert_stdin_hwp3_stops_at_the_reader_bound() {
    use std::io::Write as _;
    let output = tmp("stdin-hwp3-over.hwpx");
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
        if stdin.write_all(hwp5::hwp3::SIGNATURE).is_err() {
            return;
        }
        let chunk = vec![0u8; 1 << 20];
        while stdin.write_all(&chunk).is_ok() {}
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("convert - kept reading HWP 3.x stdin past the reader bound");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("stdin") && err.contains(&hwp5::hwp3::MAX_BYTES.to_string()),
        "{err}"
    );
    assert!(!output.exists());
}

//! `hwp validate`: structural validation.
//!
//! Checks the mimetype, required entries and XML parsing (body and header) for HWPX, and the
//! reader plus record-structure rules for HWP5. XSD schema validation is out of scope. Any hard
//! error yields a non-zero exit code (consumers judge by the exit code).

use std::path::Path;

use serde_json::{Value, json};

use crate::format::{FileFormat, detect};

/// Builds the structural validation result as a JSON object (shared by the CLI and MCP; no
/// `process::exit`).
/// `{file, format, valid, errors, warnings}`.
pub fn validate_json(path: &Path) -> Value {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut format = "unknown";
    let mut package_limits = None;

    match detect(path) {
        Ok(FileFormat::Hwpx) => {
            format = "hwpx";
            package_limits = Some(json!(hwpx::NATIVE_PACKAGE_LIMITS_PROFILE));
            validate_hwpx(path, &mut errors, &mut warnings);
        }
        Ok(FileFormat::Hwp5) => {
            format = "hwp5";
            match hwp5::read_document(path) {
                Ok(r) => {
                    warnings.extend(r.warnings);
                    // #610: the reader accepts a detached CTRL_DATA, Hangul does not. Only a
                    // document the reader accepted is scanned, so protected or damaged files
                    // keep their single read error. A section the scan could not inspect is a
                    // warning, never a silent pass.
                    let scan = hwp5::structure_check::scan_ctrl_data(path);
                    errors.extend(hwp5::structure_check::detached_ctrl_data_errors(&scan));
                    warnings.extend(hwp5::structure_check::unchecked_warnings(&scan));
                }
                Err(e) => errors.push(format!("파싱 실패: {e}")),
            }
        }
        Err(e) => errors.push(format!("포맷 감지 실패: {e}")),
    }

    json!({
        "file": path.display().to_string(),
        "format": format,
        "valid": errors.is_empty(),
        "errors": errors,
        "warnings": warnings,
        "package_limits": package_limits,
    })
}

pub fn run(path: &Path, json: bool) -> anyhow::Result<()> {
    let result = validate_json(path);
    let valid = result["valid"].as_bool().unwrap_or(false);

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("파일: {}", path.display());
        println!("포맷: {}", result["format"].as_str().unwrap_or("unknown"));
        println!("결과: {}", if valid { "유효" } else { "오류" });
        for e in result["errors"].as_array().into_iter().flatten() {
            println!("  오류: {}", e.as_str().unwrap_or_default());
        }
        for w in result["warnings"].as_array().into_iter().flatten() {
            println!("  경고: {}", w.as_str().unwrap_or_default());
        }
    }

    if valid {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn validate_hwpx(path: &Path, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
    let mut pkg = match hwpx::HwpxPackage::open(path) {
        Ok(p) => p,
        Err(e) => {
            errors.push(format!("패키지/mimetype 오류: {e}"));
            return;
        }
    };

    let names: Vec<String> = match pkg.entries() {
        Ok(es) => es.into_iter().map(|e| e.name).collect(),
        Err(e) => {
            errors.push(format!("엔트리 목록 실패: {e}"));
            return;
        }
    };
    for req in [
        "mimetype",
        "version.xml",
        "Contents/header.xml",
        "META-INF/container.xml",
    ] {
        if !names.iter().any(|n| n == req) {
            errors.push(format!("필수 엔트리 누락: {req}"));
        }
    }
    if !names.iter().any(|n| n.starts_with("Contents/section")) {
        errors.push("본문 섹션(Contents/section*.xml) 없음".to_string());
    }
    // Packaging rule: mimetype must be the first entry (an invariant the write path enforces).
    // The reader opens by name, so a wrong order still opens; surface it as a warning only.
    if names.first().map(String::as_str) != Some("mimetype") {
        warnings.push("mimetype이 첫 엔트리가 아님 (패키징 규칙 위반)".to_string());
    }

    // Structural validation does not load BinData into memory, but it streams every
    // non-directory entry to EOF, so CRC/deflate damage in raw-copied entries is caught too.
    match hwpx::read_structure(path) {
        Ok(r) => warnings.extend(r.warnings),
        Err(e) => errors.push(format!("XML 파싱 실패: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review of #611: when the CTRL_DATA scan cannot inspect a section (here one that
    /// decompresses past its 64 MiB cap while the reader still accepts the file), `validate`
    /// must say the section was not verified rather than pass it silently.
    #[test]
    fn unchecked_section_is_a_validate_warning_not_a_silent_pass() {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "hwp611-validate-unchecked-{}.hwp",
            std::process::id()
        ));
        let document = hwp_convert::from_markdown("| a |\n|---|\n| b |\n");
        hwp5::write_document(&document, &path, &hwp5::WriteOptions::default()).unwrap();
        let mut container = hwp5::Hwp5Container::open(&path).unwrap();
        let mut body = container.read_record_stream("/BodyText/Section0").unwrap();
        drop(container);
        // Zero padding scans as empty records, so the reader still accepts the section.
        body.resize(
            body.len() + hwp5::structure_check::MAX_SCAN_STREAM_BYTES as usize,
            0,
        );
        let mut cfb = cfb::open_rw(&path).unwrap();
        cfb.create_stream("/BodyText/Section0")
            .unwrap()
            .write_all(&hwp5::codec::compress(&body))
            .unwrap();
        cfb.flush().unwrap();
        drop(cfb);

        let result = validate_json(&path);
        std::fs::remove_file(&path).unwrap();
        let warnings = result["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|warning| {
                let text = warning.as_str().unwrap();
                text.contains("/BodyText/Section0")
                    && text.contains("검증되지")
                    && text.contains("64 MiB")
            }),
            "{result}"
        );
    }
}

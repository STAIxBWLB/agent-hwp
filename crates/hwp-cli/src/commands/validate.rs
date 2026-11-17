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
                    // keep their single read error.
                    errors.extend(hwp5::structure_check::detached_ctrl_data(path));
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

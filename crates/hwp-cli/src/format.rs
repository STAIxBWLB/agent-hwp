//! 파일 포맷 감지.
//!
//! 확장자가 아니라 매직 바이트로 판별한다 — 확장자가 틀린 파일도 처리.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// HWP 5.0 (CFB 컨테이너)
    Hwp5,
    /// HWPX (ZIP + OWPML)
    Hwpx,
}

const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const ZIP_MAGIC: [u8; 2] = [0x50, 0x4B]; // "PK"

pub fn detect(path: &Path) -> anyhow::Result<FileFormat> {
    let mut head = [0u8; 8];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .with_context(|| format!("파일을 열 수 없습니다: {}", path.display()))?;

    if n >= 8 && head == CFB_MAGIC {
        Ok(FileFormat::Hwp5)
    } else if n >= 2 && head[..2] == ZIP_MAGIC {
        Ok(FileFormat::Hwpx)
    } else {
        match detect_legacy(path)? {
            Some(legacy) => anyhow::bail!(
                "{}: {}은(는) 읽기 전용 입력입니다. cat·convert·render 등 읽기 명령으로 열거나 \
                 convert --to hwp|hwpx로 변환한 뒤 편집하세요",
                path.display(),
                legacy.label()
            ),
            None => anyhow::bail!(
                "{}: HWP(CFB)도 HWPX(ZIP)도 아닙니다 (시그니처 불일치)",
                path.display()
            ),
        }
    }
}

/// Read-only legacy inputs (GJ-2, GJ-3). They are lowered to the IR on load and never
/// written back in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyFormat {
    /// HWPML single-file XML (`.hml`).
    Hml,
    /// HWP 3.x binary (`HWP Document File V3.00` signature).
    Hwp3,
}

impl LegacyFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hml => "HWPML(.hml)",
            Self::Hwp3 => "HWP 3.x",
        }
    }
}

/// Signature prefix shared by HWP 1.x-3.x files; the version digits follow it.
const HWP_LEGACY_PREFIX: &[u8] = b"HWP Document File V";

/// Detects a read-only legacy format by content. `Ok(None)` means "not legacy" (the caller
/// keeps its CFB/ZIP path). Recognized-but-unsupported variants (HWP 1.x/2.x) are typed errors.
pub fn detect_legacy(path: &Path) -> anyhow::Result<Option<LegacyFormat>> {
    let mut head = vec![0u8; 4096];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .with_context(|| format!("파일을 열 수 없습니다: {}", path.display()))?;
    head.truncate(n);
    detect_legacy_bytes(&head).with_context(|| path.display().to_string())
}

/// Byte-level form of [`detect_legacy`] over the first bytes of a file.
pub fn detect_legacy_bytes(head: &[u8]) -> anyhow::Result<Option<LegacyFormat>> {
    if let Some(rest) = head.strip_prefix(HWP_LEGACY_PREFIX) {
        return match rest.first() {
            Some(b'3') => Ok(Some(LegacyFormat::Hwp3)),
            Some(v) => anyhow::bail!(
                "HWP {}.x 문서는 지원하지 않습니다 (HWP 3.x 이상만 읽을 수 있습니다)",
                char::from(*v)
            ),
            None => anyhow::bail!("HWP 레거시 시그니처가 잘려 있습니다"),
        };
    }
    if hwpx::looks_like_hml(head) {
        return Ok(Some(LegacyFormat::Hml));
    }
    Ok(None)
}

/// 바이트 시그니처로 포맷을 판별해 표준 확장자를 돌려준다 (stdin 스테이징용, GM-2).
pub fn detect_bytes(bytes: &[u8]) -> anyhow::Result<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == CFB_MAGIC {
        Ok("hwp")
    } else if bytes.len() >= 2 && bytes[..2] == ZIP_MAGIC {
        Ok("hwpx")
    } else {
        anyhow::bail!("HWP(CFB)도 HWPX(ZIP)도 아닙니다 (시그니처 불일치)")
    }
}

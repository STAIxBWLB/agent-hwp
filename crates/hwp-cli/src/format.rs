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
                "{}: {}은(는) 읽기 전용 입력이라 제자리에서 편집할 수 없습니다. {}",
                path.display(),
                legacy.label(),
                legacy.edit_hint()
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

    /// Recovery for an in-place edit. HWPML converts in this build; HWP 3.x reading is not
    /// wired until #114, so `convert` cannot help yet and Hancom re-saving is the path.
    pub fn edit_hint(self) -> String {
        match self {
            Self::Hml => {
                "먼저 `ahwp convert <입력> -o <출력>.hwpx`(또는 .hwp)로 변환한 뒤 그 파일을 편집하세요"
                    .to_string()
            }
            Self::Hwp3 => format!("이 빌드는 HWP 3.x를 아직 읽지 못합니다. {LEGACY_RESAVE_HINT}"),
        }
    }
}

/// Signature prefix shared by HWP 1.x-3.x files; the version digits follow it.
const HWP_LEGACY_PREFIX: &[u8] = b"HWP Document File V";

/// The complete 30-byte HWP 3.00 signature (한글문서파일형식 3.0 §3.1). The same constant
/// is `hwp5::hwp3::SIGNATURE` in the reader (#113).
const HWP3_SIGNATURE: &[u8; 30] = b"HWP Document File V3.00 \x1a\x01\x02\x03\x04\x05";

/// Recovery for legacy inputs this build cannot read: Hancom itself re-saves them.
const LEGACY_RESAVE_HINT: &str =
    "한글(한컴오피스)에서 열어 HWP 5.0(.hwp) 또는 HWPX로 다시 저장한 뒤 사용하세요";

/// Detects a read-only legacy format by content. `Ok(None)` means "not legacy" (the caller
/// keeps its CFB/ZIP path). Recognized-but-unsupported variants (HWP 1.x/2.x, other 3.x
/// revisions, a cut-off signature) are typed errors, each distinct from a valid input.
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
    if head.starts_with(HWP_LEGACY_PREFIX) {
        if head.starts_with(HWP3_SIGNATURE) {
            return Ok(Some(LegacyFormat::Hwp3));
        }
        if head.len() < HWP3_SIGNATURE.len() {
            anyhow::bail!(
                "HWP 레거시 시그니처가 잘려 있습니다 ({}/{}바이트). 파일이 완전히 복사됐는지 확인하세요",
                head.len(),
                HWP3_SIGNATURE.len()
            );
        }
        // The version field is `D.DD` right after the prefix.
        let version = String::from_utf8_lossy(&head[HWP_LEGACY_PREFIX.len()..][..4]).into_owned();
        if version == "3.00" {
            anyhow::bail!(
                "HWP 3.00 시그니처의 끝 6바이트가 다릅니다 (손상됐거나 다른 프로그램이 만든 파일). \
                 원본을 다시 받거나 {LEGACY_RESAVE_HINT}"
            );
        }
        anyhow::bail!(
            "HWP {version} 문서는 지원하지 않습니다 (HWP 3.00 시그니처만 읽습니다). {LEGACY_RESAVE_HINT}"
        );
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

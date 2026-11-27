//! HWP 5.0 binary format reader/writer.
//!
//! 본 제품은 한글과컴퓨터의 한글 문서 파일(.hwp) 공개 문서(한글 문서 파일 형식 5.0,
//! <https://store.hancom.com/etc/hwpDownload.do>, © (주)한글과컴퓨터)를 참고하여 개발하였습니다.
//! (Required attribution, kept verbatim: this product was developed with reference to
//! Hancom's public HWP 5.0 file format document.)
//!
//! An HWP 5.0 file is an MS CFB (Compound File Binary) container:
//! - `FileHeader`        - fixed 256-byte header (signature, version, attribute flags)
//! - `DocInfo`           - document information record stream (usually raw-deflate compressed)
//! - `BodyText/Section0..N` - body record streams (usually raw-deflate compressed)
//! - `BinData/*`         - embedded binaries (images and the like)
//! - `PrvText`/`PrvImage` - previews (uncompressed)
//! - Other: `\x05HwpSummaryInformation`, `Scripts/*`, `DocOptions/*` and so on
//!
//! Layers:
//! - [`container`] - CFB wrapping, stream listing and reading
//! - [`file_header`] - FileHeader parsing and serialization
//! - [`codec`] - byte cursors (reader/writer) and raw-deflate compression
//! - [`record`] - record header codec, flat stream to tree conversion

pub mod body_text;
pub mod codec;
pub mod container;
pub mod distdoc;
pub mod doc_info;
pub mod error;
pub mod file_header;
pub mod hwp3;

pub mod numbering;
pub mod read;
pub mod record;
pub mod structure_check;
pub mod summary;
pub mod write;

pub use container::Hwp5Container;
pub use error::Hwp5Error;
pub use file_header::FileHeader;
pub use read::{
    BoundedReadLimits, BoundedReadSnapshot, ReadOptions, ReadResult, ScriptPresence,
    authenticate_container_with_options, read_document, read_document_with_options,
};
pub use summary::parse_summary;
pub use write::{
    WriteOptions, rewrite_document_with_report, validate_evidenced_official_numbering,
    validate_official_hwp_numbering, write_document, write_document_with_report,
};

/// Conservative live-memory reservation for one password-protected HWP5
/// document. Batch callers reserve this before invoking the reader.
pub const PASSWORD_PROTECTED_DOCUMENT_LIVE_LIMIT: u64 =
    read::PASSWORD_PROTECTED_DOCUMENT_LIVE_LIMIT;

/// Maximum HWP5 CFB container size accepted by snapshot-based publication.
/// HWP5 uses CFB major version 3, whose supported file range ends at 2 GiB;
/// this is intentionally separate from the protected reader's live-memory cap.
pub const SUPPORTED_CONTAINER_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

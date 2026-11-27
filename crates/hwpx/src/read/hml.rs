//! Read-only HWPML (`.hml`) ingestion into the shared IR (GJ-2).
//!
//! HWPML is Hancom's single-file XML serialization (HWPML rev1.2, Part II). The element names used
//! here follow that public structure: `HWPML` > `HEAD` (`DOCSUMMARY`, `MAPPINGTABLE`) > `BODY` >
//! `SECTION` > `P` > `TEXT` > `CHAR`, with `TABLE` > `ROW` > `CELL` > `PARALIST` > `P` for tables and
//! `TAIL` > `BINDATASTORAGE` > `BINDATA` (base64) for embedded binaries.
//!
//! The reader does not build IR records itself. It lowers the HWPML body to the strict HTML subset
//! `hwp_convert::from_html` already turns into IR (paragraphs, line breaks, spanned tables and
//! data-URI images), so the paragraph, char-shape-run and table invariants live in one place.
//!
//! Unverified (no genuine Hancom-saved `.hml` was available; synthetic inputs only):
//! - char/para shape mapping: all text takes the default body shape, `CHARSHAPE`/`PARASHAPE` from
//!   `MAPPINGTABLE` are not carried over;
//! - picture size: images take their natural size, `SHAPEOBJECT/SIZE` is ignored;
//! - the `IMAGE@BinItem` -> `BINITEM@BinData` -> `BINDATA@Id` chain is followed as written in the
//!   spec structure. Only when the document has no `BINITEM` table at all is `BinItem` matched to
//!   `BINDATA@Id` directly; with a table, a missing or refused mapped binary skips the picture
//!   instead of substituting an unrelated `BINDATA`.
//!
//! - `BINDATA@Compress="true"` is refused with a warning: the compressed stream format is not
//!   pinned by a genuine file. An absent `Compress` is read as uncompressed.
//!
//! Untrusted input is bounded by [`HML_LIMITS`] (input size, table depth, lowered size, picture
//! count and projected table slots), each checked before the allocation it guards. Every append
//! to the lowering, including a paragraph buffer not yet flushed, is counted against
//! `max_lowered_bytes` before it is made.
//!
//! Content the lowering cannot express (shapes, equations, fields, headers/footers, notes) is
//! dropped with a warning naming the element, never silently.
use std::collections::{BTreeSet, HashMap};

use hwp_model::{DocMeta, Document, Metadata};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::xml::{attr, resolve_entity};
use crate::error::{HwpxError, Result};

const ENTRY: &str = "hml";

/// Result of reading an HWPML file.
pub struct HmlReadResult {
    pub document: Document,
    pub warnings: Vec<String>,
}

/// True when `bytes` look like an HWPML document: an XML prolog (any of the BOMs this reader
/// decodes) whose root element is `HWPML` within the first 4 KiB.
pub fn looks_like_hml(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let text = decode(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with('<') && find_root(&text).is_some_and(|name| name == "HWPML")
}

fn find_root(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        if rest.starts_with('?') || rest.starts_with('!') {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(rest.len());
        return Some(&rest[..end]);
    }
    None
}

/// Decodes the byte stream: UTF-16 LE/BE by BOM, otherwise UTF-8 (BOM stripped), falling back
/// to EUC-KR when the prolog declares a Korean legacy encoding.
fn decode(bytes: &[u8]) -> String {
    if let Some((enc, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc
            .decode_without_bom_handling(&bytes[bom..])
            .0
            .into_owned();
    }
    let prolog = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).to_ascii_lowercase();
    if prolog.contains("euc-kr") || prolog.contains("ks_c_5601") || prolog.contains("cp949") {
        return encoding_rs::EUC_KR
            .decode_without_bom_handling(bytes)
            .0
            .into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Resource limits for untrusted HWPML input. Every bound is checked before the allocation it
/// guards, and a violation fails the read instead of truncating the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HmlLimits {
    /// Maximum input file size. Callers reading from disk check it before reading.
    pub max_input_bytes: u64,
    /// Maximum table nesting (a table inside a cell counts one level).
    pub max_table_depth: usize,
    /// Maximum size of the intermediate lowering, embedded image data included, so repeated
    /// references to one large BINDATA cannot multiply memory.
    pub max_lowered_bytes: usize,
    /// Maximum number of picture references lowered.
    pub max_pictures: usize,
    /// Maximum projected table grid slots (rows x columns, summed over every table). The IR
    /// builder allocates one occupancy slot per grid position and synthesizes an empty cell for
    /// every uncovered one, so the bound is checked while cells are placed, before the grid
    /// exists.
    pub max_table_slots: usize,
}

/// Default limits: the input bound matches the HWPX XML part bound
/// ([`crate::NATIVE_PACKAGE_LIMITS`] `max_xml_uncompressed_bytes`), and the lowered output may
/// be at most four times that.
pub const HML_LIMITS: HmlLimits = HmlLimits {
    max_input_bytes: 64 * 1024 * 1024,
    max_table_depth: 32,
    max_lowered_bytes: 256 * 1024 * 1024,
    max_pictures: 4_096,
    max_table_slots: 262_144,
};

fn limit_err(message: String) -> HwpxError {
    HwpxError::PackageLimit(format!("HWPML: {message}"))
}

/// Reads an HWPML byte stream into the IR with [`HML_LIMITS`].
pub fn read_hml(bytes: &[u8]) -> Result<HmlReadResult> {
    read_hml_with_limits(bytes, &HML_LIMITS)
}

/// Reads an HWPML byte stream into the IR under explicit limits.
pub fn read_hml_with_limits(bytes: &[u8], limits: &HmlLimits) -> Result<HmlReadResult> {
    if bytes.len() as u64 > limits.max_input_bytes {
        return Err(limit_err(format!(
            "입력 크기 {}바이트가 상한 {}바이트를 넘습니다",
            bytes.len(),
            limits.max_input_bytes
        )));
    }
    let text = decode(bytes);
    if find_root(&text) != Some("HWPML") {
        return Err(xml_err("루트 요소가 HWPML이 아닙니다"));
    }
    let mut lower = Lower {
        limits: *limits,
        ..Lower::default()
    };
    lower.collect_tail(&text)?;
    lower.run(&text)?;
    let html = std::mem::take(&mut lower.html);
    let mut document = lower_sections(&html, &lower.section_starts)?;
    document.meta = DocMeta {
        source_format: "hml".to_string(),
        source_version: lower.version.clone(),
    };
    document.metadata = lower.metadata.clone();
    let mut warnings: Vec<String> = lower
        .dropped
        .iter()
        .map(|name| format!("HWPML <{name}> 요소는 읽기 미지원이라 건너뜁니다"))
        .collect();
    warnings.append(&mut lower.warnings);
    warnings.extend(
        lower
            .flattened
            .iter()
            .map(|name| format!("HWPML <{name}> 필드는 글자만 남기고 필드 정보는 건너뜁니다")),
    );
    Ok(HmlReadResult { document, warnings })
}

fn xml_err(message: &str) -> HwpxError {
    HwpxError::Xml {
        entry: ENTRY.to_string(),
        message: message.to_string(),
    }
}

/// Builds the IR with one section per HWPML `SECTION`. `starts` are the byte offsets in
/// `html` where each section's markup begins (block boundaries, so every slice is a complete
/// fragment). Image names are seeded per section so they stay unique across the document.
fn lower_sections(html: &str, starts: &[usize]) -> Result<Document> {
    let build = |fragment: &str, seed: usize| {
        hwp_convert::from_html::from_html_with(
            fragment,
            &hwp_convert::from_html::HtmlImportOptions::lowering(seed),
        )
        .map_err(|e| xml_err(&format!("IR 구성 실패: {e}")))
    };
    if starts.len() <= 1 {
        return build(html, 0);
    }
    // Anything before the first SECTION (malformed, but possible) stays in the first section.
    let mut begins: Vec<usize> = starts.to_vec();
    begins[0] = 0;
    let mut ends: Vec<usize> = starts[1..].to_vec();
    ends.push(html.len());
    let mut document: Option<Document> = None;
    for (&start, &end) in begins.iter().zip(&ends) {
        let seed = document.as_ref().map_or(0, |d| d.bin_streams.len());
        let part = build(&html[start..end], seed)?;
        match document.as_mut() {
            None => document = Some(part),
            Some(doc) => {
                // Every part shares the default header, so only body and binaries move over.
                doc.sections.extend(part.sections);
                doc.bin_streams.extend(part.bin_streams);
            }
        }
    }
    Ok(document.expect("at least two sections"))
}

#[derive(Default)]
struct Lower {
    html: String,
    version: String,
    metadata: Metadata,
    /// BINDATA@Id -> (decoded bytes).
    bin_data: HashMap<String, Vec<u8>>,
    /// BINITEM ordinal (1-based) -> BINDATA@Id.
    bin_items: Vec<Option<String>>,
    dropped: BTreeSet<String>,
    warnings: Vec<String>,
    limits: HmlLimits,
    table_depth: usize,
    pictures: usize,
    /// Slots charged to every table so far, nested tables included (see [`Lower::charge`]).
    table_slots: usize,
    /// Byte offset in `html` where each `SECTION` starts; each section becomes one IR section.
    section_starts: Vec<usize>,
    /// Inline controls whose text is kept but whose control semantics are dropped.
    flattened: BTreeSet<String>,
}

/// Mirrors the first-free-slot placement of `hwp_convert::from_html`'s table builder so the
/// projected grid (`rows x cols`) is known, and bounded, before that builder allocates it.
#[derive(Default)]
struct GridSim {
    /// Rows still covered by a rowspan from an earlier row, per column (current row included).
    carry: Vec<u32>,
    /// Occupancy of the current row.
    row: Vec<bool>,
    rows: usize,
    cols: usize,
    /// First column of the current row not yet known to be covered; cells are placed from
    /// here, so a row of n cells costs O(n).
    cursor: usize,
    /// Furthest row any placed cell's rowspan reaches (exclusive).
    reach: usize,
    /// Whether a ROW is open (it counts as one projected row before it closes).
    open: bool,
}

impl GridSim {
    fn start_row(&mut self) {
        self.row = self.carry.iter().map(|&c| c > 0).collect();
        self.cursor = 0;
        self.open = true;
    }

    fn end_row(&mut self) -> std::result::Result<(), ()> {
        self.rows += 1;
        self.open = false;
        self.cols = self.cols.max(self.row.len());
        for c in &mut self.carry {
            *c = c.saturating_sub(1);
        }
        if self.rows > IR_MAX_DIM {
            return Err(());
        }
        Ok(())
    }

    /// Places one cell. Only the IR dimension bound is checked here; the slot budget is
    /// charged by the caller from [`GridSim::projected`] right after, before any cell content
    /// (and so any nested table) is read. Vector growth is bounded by `IR_MAX_DIM`.
    fn place(&mut self, col_span: u16, row_span: u16) -> std::result::Result<(), ()> {
        let mut ci = self.cursor;
        while self.row.get(ci).copied().unwrap_or(false) {
            ci += 1;
        }
        let need = ci + col_span as usize;
        let reach = self.rows + row_span as usize;
        if need > IR_MAX_DIM || reach > IR_MAX_DIM {
            return Err(());
        }
        if self.row.len() < need {
            self.row.resize(need, false);
        }
        if self.carry.len() < need {
            self.carry.resize(need, 0);
        }
        for c in ci..need {
            self.row[c] = true;
            self.carry[c] = self.carry[c].max(u32::from(row_span));
        }
        self.cursor = need;
        self.cols = self.cols.max(need);
        self.reach = self.reach.max(reach);
        Ok(())
    }

    /// Grid slots this table will occupy at least, given what has been read so far. The IR
    /// builder gives every table at least one column and fills it, so rows with no cells
    /// still occupy one slot each.
    fn projected(&self) -> usize {
        let rows = (self.rows + usize::from(self.open)).max(self.reach);
        rows.saturating_mul(self.cols.max(1))
    }
}

/// The IR stores table rows, columns and cell addresses as `u16`.
const IR_MAX_DIM: usize = u16::MAX as usize;

/// Exact length of [`html_escape`]'s output, so the bound is checked before the append.
fn escaped_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '&' => 5,
            '<' | '>' => 4,
            '"' => 6,
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => 0,
            c => c.len_utf8(),
        })
        .sum()
}

impl Default for HmlLimits {
    fn default() -> Self {
        HML_LIMITS
    }
}

/// Elements whose subtree carries no body text the lowering can express.
const SKIPPED: &[&str] = &[
    "HEADER",
    "FOOTER",
    "FOOTNOTE",
    "ENDNOTE",
    "EQUATION",
    "LINE",
    "RECTANGLE",
    "ELLIPSE",
    "ARC",
    "POLYGON",
    "CURVE",
    "CONTAINER",
    "OLE",
    "TEXTART",
    "UNKNOWNOBJECT",
    "FORMOBJECT",
    "BUTTON",
    "RADIOBUTTON",
    "CHECKBUTTON",
    "COMBOBOX",
    "EDIT",
    "LISTBOX",
    "SCROLLBAR",
    "HIDDENCOMMENT",
];

/// Wrapper/property elements that are transparent or carry no text.
const TRANSPARENT: &[&str] = &[
    "BODY",
    "SECTION",
    "TEXT",
    "PARALIST",
    "CELLZONELIST",
    "CELLZONE",
    "SHAPEOBJECT",
    "SHAPECOMPONENT",
    "SIZE",
    "POSITION",
    "OUTSIDEMARGIN",
    "INSIDEMARGIN",
    "CAPTION",
    "IMAGERECT",
    "IMAGECLIP",
    "EFFECTS",
    "ROTATIONINFO",
    "RENDERINGINFO",
    "TRANSMATRIX",
    "SCAMATRIX",
    "ROTMATRIX",
    "LINESHAPE",
    "FILLBRUSH",
    "SECDEF",
    "COLDEF",
    "STARTNUMBER",
    "HIDE",
    "PAGEDEF",
    "PAGEMARGIN",
    "FOOTNOTESHAPE",
    "ENDNOTESHAPE",
    "PAGEBORDERFILL",
    "MASTERPAGE",
    "PARAMETERSET",
    "PARAMETERARRAY",
    "ITEM",
    "MARKPENEND",
    "TITLEMARK",
    "FIELDBEGIN",
    "FIELDEND",
    "BOOKMARK",
    "AUTONUM",
    "NEWNUM",
    "PAGENUMCTRL",
    "PAGEHIDING",
    "PAGENUM",
    "INDEXMARK",
    "COMPOSE",
    "COMPCHARSHAPE",
    "DUTMAL",
    "MAINTEXT",
    "SUBTEXT",
    "COLUMNDEFLINE",
    "PAGEBREAK",
    "COLDEFINE",
];

fn html_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // XML 1.0 forbids most C0 controls; drop them rather than fail the parse.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => out.push(c),
        }
    }
}

fn name_of(e: &BytesStart<'_>) -> String {
    e.local_name().into_inner().to_string()
}

impl Lower {
    /// First pass: binaries live in `TAIL`, after the body that references them.
    fn collect_tail(&mut self, text: &str) -> Result<()> {
        let mut r = Reader::from_str(text);
        // id, encoding, compressed flag, payload
        let mut in_bindata: Option<(String, String, bool, String)> = None;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e))
                    if e.local_name().into_inner() == "BINITEM" =>
                {
                    self.bin_items.push(attr(&e, "BinData"));
                }
                Ok(Event::Start(e)) if e.local_name().into_inner() == "BINDATA" => {
                    in_bindata = Some((
                        attr(&e, "Id").unwrap_or_default(),
                        attr(&e, "Encoding").unwrap_or_else(|| "Base64".into()),
                        attr(&e, "Compress").is_some_and(|v| v.eq_ignore_ascii_case("true")),
                        String::new(),
                    ));
                }
                Ok(Event::Text(t)) => {
                    if let Some((_, _, _, payload)) = in_bindata.as_mut() {
                        payload.push_str(&t.xml10_content());
                    }
                }
                Ok(Event::End(e)) if e.local_name().into_inner() == "BINDATA" => {
                    if let Some((id, encoding, compressed, payload)) = in_bindata.take() {
                        // The compressed representation's stream format is not pinned by a
                        // genuine file, so it is refused by name rather than guessed at.
                        if compressed {
                            self.warnings.push(format!(
                                "BINDATA {id}: 압축(Compress=\"true\") 데이터는 미지원이라 건너뜁니다"
                            ));
                            continue;
                        }
                        if !encoding.eq_ignore_ascii_case("base64") {
                            self.warnings.push(format!(
                                "BINDATA {id}: 인코딩 {encoding}은 미지원이라 건너뜁니다"
                            ));
                            continue;
                        }
                        let compact: String =
                            payload.chars().filter(|c| !c.is_whitespace()).collect();
                        match hwp_convert::base64::decode(&compact) {
                            Ok(data) => {
                                self.bin_data.insert(id, data);
                            }
                            Err(e) => self
                                .warnings
                                .push(format!("BINDATA {id}: base64 디코딩 실패 ({e})")),
                        }
                    }
                }
                Ok(Event::Eof) => return Ok(()),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }

    /// With a `BINITEM` table the mapping is authoritative: a missing or refused mapped binary
    /// yields `None` rather than a `BINDATA` that happens to share the reference's number.
    fn image_bytes(&self, bin_item: &str) -> Option<&Vec<u8>> {
        if self.bin_items.is_empty() {
            return self.bin_data.get(bin_item);
        }
        bin_item
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.bin_items.get(i))
            .and_then(|id| id.as_ref())
            .and_then(|id| self.bin_data.get(id))
    }

    fn run(&mut self, text: &str) -> Result<()> {
        let mut r = Reader::from_str(text);
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => match e.local_name().into_inner() {
                    "HWPML" => self.version = attr(&e, "Version").unwrap_or_default(),
                    "DOCSUMMARY" => self.summary(&mut r)?,
                    "BODY" => {
                        self.blocks(&mut r, "BODY")?;
                    }
                    "HEAD" | "TAIL" => {}
                    _ => skip(&mut r, &e)?,
                },
                Ok(Event::Eof) => return Ok(()),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }

    fn summary(&mut self, r: &mut Reader<&[u8]>) -> Result<()> {
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    let name = name_of(&e);
                    let value = text_of(r, &name)?;
                    let value = value.trim().to_string();
                    if value.is_empty() {
                        continue;
                    }
                    let slot = match name.as_str() {
                        "TITLE" => &mut self.metadata.title,
                        "AUTHOR" => &mut self.metadata.author,
                        "SUBJECT" => &mut self.metadata.subject,
                        "KEYWORDS" => &mut self.metadata.keywords,
                        "COMMENTS" => &mut self.metadata.description,
                        _ => continue,
                    };
                    *slot = Some(value);
                }
                Ok(Event::End(e)) if e.local_name().into_inner() == "DOCSUMMARY" => return Ok(()),
                Ok(Event::Eof) => return Err(xml_err("닫히지 않은 DOCSUMMARY")),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }

    /// Block container (BODY, SECTION, PARALIST): emits one HTML block per `P`, with tables
    /// lifted out of their anchoring paragraph. Returns whether any block was emitted.
    fn blocks(&mut self, r: &mut Reader<&[u8]>, end: &str) -> Result<bool> {
        let mut emitted = false;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    let name = name_of(&e);
                    match name.as_str() {
                        "P" => {
                            self.paragraph(r)?;
                            emitted = true;
                        }
                        // One IR section per SECTION; nested SECTIONs cannot occur in a
                        // well-formed body, so only BODY-level ones split.
                        "SECTION" if end == "BODY" => {
                            self.section_starts.push(self.html.len());
                        }
                        n if TRANSPARENT.contains(&n) => {}
                        _ => {
                            self.dropped.insert(name);
                            skip(r, &e)?;
                        }
                    }
                }
                Ok(Event::Empty(e)) => {
                    let local = e.local_name().into_inner();
                    if local == "P" {
                        self.push_html("<p></p>")?;
                        emitted = true;
                    } else if local == "SECTION" && end == "BODY" {
                        // An empty section still becomes its own (blank) IR section.
                        self.section_starts.push(self.html.len());
                        self.push_html("<p></p>")?;
                    } else if SKIPPED.contains(&local) {
                        self.dropped.insert(local.to_string());
                    }
                }
                Ok(Event::End(e)) if e.local_name().into_inner() == end => {
                    return Ok(emitted);
                }
                Ok(Event::Eof) => return Err(xml_err(&format!("닫히지 않은 {end}"))),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }

    /// One `P`. Inline text accumulates into an open `<p>`; a `TABLE` closes it, emits the
    /// table as its own block, and text after the table opens a new paragraph.
    fn paragraph(&mut self, r: &mut Reader<&[u8]>) -> Result<()> {
        let mut buf = String::new();
        let mut had_block = false;
        let mut in_char = 0usize;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    let name = name_of(&e);
                    match name.as_str() {
                        "CHAR" => in_char += 1,
                        "TABLE" => {
                            if !buf.is_empty() {
                                self.flush_p(&mut buf)?;
                            }
                            self.table(r)?;
                            had_block = true;
                        }
                        "PICTURE" => self.picture(r, &mut buf)?,
                        // Field markers: the text between them stays in the paragraph, the
                        // field itself is not lowered, and that loss is reported.
                        "FIELDBEGIN" | "FIELDEND" => {
                            self.flattened.insert("FIELDBEGIN/FIELDEND".into());
                        }
                        n if SKIPPED.contains(&n) => {
                            self.dropped.insert(name);
                            skip(r, &e)?;
                        }
                        n if TRANSPARENT.contains(&n) => {}
                        _ => {
                            self.dropped.insert(name);
                            skip(r, &e)?;
                        }
                    }
                }
                Ok(Event::Empty(e)) => match e.local_name().into_inner() {
                    "TAB" => self.push_buf(&mut buf, "\t")?,
                    "LINEBREAK" => self.push_buf(&mut buf, "<br/>")?,
                    "NBSPACE" | "FWSPACE" => self.push_buf(&mut buf, "\u{a0}")?,
                    "HYPEN" | "HYPHEN" => self.push_buf(&mut buf, "-")?,
                    "FIELDBEGIN" | "FIELDEND" => {
                        self.flattened.insert("FIELDBEGIN/FIELDEND".into());
                    }
                    n if SKIPPED.contains(&n) => {
                        self.dropped.insert(n.to_string());
                    }
                    _ => {}
                },
                Ok(Event::Text(t)) if in_char > 0 => {
                    self.push_escaped(&mut buf, &t.xml10_content())?;
                }
                Ok(Event::GeneralRef(g)) if in_char > 0 => {
                    if let Some(c) = resolve_entity(&g) {
                        self.push_escaped(&mut buf, c.encode_utf8(&mut [0u8; 4]))?;
                    }
                }
                Ok(Event::CData(c)) if in_char > 0 => {
                    self.push_escaped(&mut buf, &c.into_inner())?;
                }
                Ok(Event::End(e)) => match e.local_name().into_inner() {
                    "CHAR" => in_char = in_char.saturating_sub(1),
                    "P" => {
                        if !buf.is_empty() || !had_block {
                            self.flush_p(&mut buf)?;
                        }
                        return Ok(());
                    }
                    _ => {}
                },
                Ok(Event::Eof) => return Err(xml_err("닫히지 않은 P")),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }

    /// Appends raw markup to the lowering after checking the bound.
    fn push_html(&mut self, s: &str) -> Result<()> {
        self.reserve(s.len())?;
        self.html.push_str(s);
        Ok(())
    }

    /// Appends raw markup to an unflushed paragraph buffer after checking the bound.
    fn push_buf(&self, buf: &mut String, s: &str) -> Result<()> {
        self.reserve(buf.len().saturating_add(s.len()))?;
        buf.push_str(s);
        Ok(())
    }

    /// Appends escaped text to a paragraph buffer, checking its exact escaped size first.
    fn push_escaped(&self, buf: &mut String, s: &str) -> Result<()> {
        self.reserve(buf.len().saturating_add(escaped_len(s)))?;
        html_escape(buf, s);
        Ok(())
    }

    fn flush_p(&mut self, buf: &mut String) -> Result<()> {
        self.reserve(buf.len() + 7)?;
        self.html.push_str("<p>");
        self.html.push_str(buf);
        self.html.push_str("</p>");
        buf.clear();
        Ok(())
    }

    /// Fails before the lowering grows past `max_lowered_bytes`. `pending` covers bytes about
    /// to be appended, including a paragraph buffer not yet flushed.
    fn reserve(&self, pending: usize) -> Result<()> {
        let total = self.html.len().saturating_add(pending);
        if total > self.limits.max_lowered_bytes {
            return Err(limit_err(format!(
                "변환 중간 결과가 상한 {}바이트를 넘습니다",
                self.limits.max_lowered_bytes
            )));
        }
        Ok(())
    }

    fn picture(&mut self, r: &mut Reader<&[u8]>, buf: &mut String) -> Result<()> {
        let mut bin_item: Option<String> = None;
        let mut depth = 1usize;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    if e.local_name().into_inner() == "IMAGE" {
                        bin_item = attr(&e, "BinItem");
                    }
                    depth += 1;
                }
                Ok(Event::Empty(e)) if e.local_name().into_inner() == "IMAGE" => {
                    bin_item = attr(&e, "BinItem");
                }
                Ok(Event::End(_)) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Ok(Event::Eof) => return Err(xml_err("닫히지 않은 PICTURE")),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
        let Some(item) = bin_item else {
            self.warnings
                .push("PICTURE에 IMAGE@BinItem이 없어 그림을 건너뜁니다".into());
            return Ok(());
        };
        self.pictures += 1;
        if self.pictures > self.limits.max_pictures {
            return Err(limit_err(format!(
                "그림 참조 수가 상한 {}을 넘습니다",
                self.limits.max_pictures
            )));
        }
        let Some(data) = self.image_bytes(&item) else {
            self.warnings.push(format!(
                "PICTURE BinItem {item}: 대응하는 BINDATA가 없어 건너뜁니다"
            ));
            return Ok(());
        };
        let (ext, _) = hwp_convert::image::image_kind(data);
        if ext == "bin" {
            self.warnings.push(format!(
                "PICTURE BinItem {item}: 지원하지 않는 이미지 형식이라 건너뜁니다"
            ));
            return Ok(());
        }
        // Exact size of the data-URI tag; checked before the base64 text is allocated.
        let tag = "<img src=\"data:image/".len() + ext.len() + ";base64,".len() + "\"/>".len();
        self.reserve(
            buf.len()
                .saturating_add(data.len().div_ceil(3) * 4)
                .saturating_add(tag),
        )?;
        buf.push_str("<img src=\"data:image/");
        buf.push_str(ext);
        buf.push_str(";base64,");
        buf.push_str(&hwp_convert::base64::encode(data));
        buf.push_str("\"/>");
        Ok(())
    }

    fn table(&mut self, r: &mut Reader<&[u8]>) -> Result<()> {
        if self.table_depth >= self.limits.max_table_depth {
            return Err(limit_err(format!(
                "표 중첩 깊이가 상한 {}을 넘습니다",
                self.limits.max_table_depth
            )));
        }
        self.table_depth += 1;
        let result = self.table_body(r);
        self.table_depth -= 1;
        result
    }

    fn table_body(&mut self, r: &mut Reader<&[u8]>) -> Result<()> {
        self.push_html("<table>")?;
        let mut grid = GridSim::default();
        // Slots of this table already charged to `table_slots`.
        let mut charged = 0usize;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => match e.local_name().into_inner() {
                    "ROW" => {
                        grid.start_row();
                        self.push_html("<tr>")?;
                    }
                    "CELL" => {
                        let span = |n: &str| {
                            attr(&e, n)
                                .and_then(|v| v.parse::<u16>().ok())
                                .filter(|&v| v > 1)
                        };
                        let (cs, rs) = (span("ColSpan"), span("RowSpan"));
                        grid.place(cs.unwrap_or(1), rs.unwrap_or(1))
                            .map_err(|()| self.slot_err())?;
                        // Reserve before the cell body is read, so nested tables see this
                        // table's slots already charged.
                        self.charge(&grid, &mut charged)?;
                        let mut open = String::from("<td");
                        if let Some(c) = cs {
                            open.push_str(&format!(" colspan=\"{c}\""));
                        }
                        if let Some(rs) = rs {
                            open.push_str(&format!(" rowspan=\"{rs}\""));
                        }
                        open.push('>');
                        self.push_html(&open)?;
                        self.blocks(r, "CELL")?;
                        self.push_html("</td>")?;
                    }
                    _ => {
                        let name = name_of(&e);
                        if !TRANSPARENT.contains(&name.as_str()) {
                            self.dropped.insert(name);
                        }
                        skip(r, &e)?;
                    }
                },
                Ok(Event::End(e)) => match e.local_name().into_inner() {
                    "ROW" => {
                        grid.end_row().map_err(|()| self.slot_err())?;
                        self.charge(&grid, &mut charged)?;
                        self.push_html("</tr>")?;
                    }
                    "TABLE" => {
                        self.charge(&grid, &mut charged)?;
                        self.push_html("</table>")?;
                        return Ok(());
                    }
                    _ => {}
                },
                Ok(Event::Eof) => return Err(xml_err("닫히지 않은 TABLE")),
                Err(e) => return Err(xml_err(&e.to_string())),
                _ => {}
            }
        }
    }
}

impl Lower {
    /// Charges the growth of `grid`'s projection to the document-wide slot budget. Called at
    /// every cell placement and row end, so an ancestor table's reservation is always in
    /// place before a nested table's cells are read.
    fn charge(&mut self, grid: &GridSim, charged: &mut usize) -> Result<()> {
        let now = grid.projected();
        if now > *charged {
            let total = self.table_slots.saturating_add(now - *charged);
            if total > self.limits.max_table_slots {
                return Err(self.slot_err());
            }
            self.table_slots = total;
            *charged = now;
        }
        Ok(())
    }

    fn slot_err(&self) -> HwpxError {
        limit_err(format!(
            "표 칸 수(행 x 열)가 상한 {}을 넘거나 행·열 수가 {IR_MAX_DIM}을 넘습니다 (표 중첩 {}단계)",
            self.limits.max_table_slots, self.table_depth
        ))
    }
}

fn skip(r: &mut Reader<&[u8]>, e: &BytesStart<'_>) -> Result<()> {
    r.read_to_end(e.name())
        .map(|_| ())
        .map_err(|err| xml_err(&err.to_string()))
}

fn text_of(r: &mut Reader<&[u8]>, end: &str) -> Result<String> {
    let mut out = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Text(t)) => out.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(g)) => {
                if let Some(c) = resolve_entity(&g) {
                    out.push(c);
                }
            }
            Ok(Event::CData(c)) => out.push_str(&c.into_inner()),
            Ok(Event::End(e)) if e.local_name().into_inner() == end => return Ok(out),
            Ok(Event::Eof) => return Err(xml_err(&format!("닫히지 않은 {end}"))),
            Err(e) => return Err(xml_err(&e.to_string())),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str, tail: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\"><HEAD/>\
             <BODY><SECTION>{body}</SECTION></BODY><TAIL><BINDATASTORAGE>{tail}</BINDATASTORAGE></TAIL></HWPML>"
        )
    }

    fn nested_tables(depth: usize) -> String {
        let mut s = String::from("<P><TEXT><CHAR>leaf</CHAR></TEXT></P>");
        for _ in 0..depth {
            s = format!(
                "<P><TEXT><TABLE><ROW><CELL><PARALIST>{s}</PARALIST></CELL></ROW></TABLE></TEXT></P>"
            );
        }
        s
    }

    fn limit_message(result: Result<HmlReadResult>) -> String {
        match result {
            Err(HwpxError::PackageLimit(m)) => m,
            Err(other) => panic!("expected a limit error, got {other}"),
            Ok(_) => panic!("expected a limit error, got a document"),
        }
    }

    #[test]
    fn input_size_is_bounded_before_decoding() {
        let text = doc("<P/>", "");
        let limits = HmlLimits {
            max_input_bytes: (text.len() - 1) as u64,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(text.as_bytes(), &limits));
        assert!(m.contains("입력 크기"), "{m}");
        // One byte more is accepted.
        let ok = HmlLimits {
            max_input_bytes: text.len() as u64,
            ..HML_LIMITS
        };
        assert!(read_hml_with_limits(text.as_bytes(), &ok).is_ok());
    }

    #[test]
    fn table_nesting_is_bounded() {
        let limits = HmlLimits {
            max_table_depth: 3,
            ..HML_LIMITS
        };
        assert!(read_hml_with_limits(doc(&nested_tables(3), "").as_bytes(), &limits).is_ok());
        let m = limit_message(read_hml_with_limits(
            doc(&nested_tables(4), "").as_bytes(),
            &limits,
        ));
        assert!(m.contains("중첩 깊이"), "{m}");
    }

    #[test]
    fn default_depth_limit_is_accepted_end_to_end() {
        // The default bound must sit inside what the IR builder itself accepts.
        let depth = HML_LIMITS.max_table_depth;
        let result = read_hml(doc(&nested_tables(depth), "").as_bytes()).unwrap();
        assert!(result.document.plain_text().contains("leaf"));
    }

    #[test]
    fn deep_default_nesting_fails_closed_without_overflow() {
        // Far past the default depth: must be a typed error, not a stack overflow.
        let m = limit_message(read_hml(doc(&nested_tables(2_000), "").as_bytes()));
        assert!(m.contains("중첩 깊이"), "{m}");
    }

    /// 1x1 GIF; image_kind recognizes the signature.
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

    fn pictures(n: usize) -> String {
        "<P><TEXT><PICTURE><IMAGE BinItem=\"1\"/></PICTURE></TEXT></P>".repeat(n)
    }

    fn bindata(attrs: &str, data: &[u8]) -> String {
        format!(
            "<BINDATA Id=\"1\" Encoding=\"Base64\"{attrs}>{}</BINDATA>",
            hwp_convert::base64::encode(data)
        )
    }

    #[test]
    fn repeated_image_references_are_bounded_by_lowered_size() {
        let mut big = GIF.to_vec();
        big.resize(64 * 1024, 0);
        let text = doc(&pictures(8), &bindata("", &big));
        // Eight 64 KiB references lower to ~700 KiB; a 256 KiB cap must refuse before encoding.
        let limits = HmlLimits {
            max_lowered_bytes: 256 * 1024,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(text.as_bytes(), &limits));
        assert!(m.contains("중간 결과"), "{m}");
    }

    #[test]
    fn picture_count_is_bounded() {
        let text = doc(&pictures(3), &bindata("", GIF));
        let limits = HmlLimits {
            max_pictures: 2,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(text.as_bytes(), &limits));
        assert!(m.contains("그림 참조 수"), "{m}");
        assert!(
            read_hml_with_limits(doc(&pictures(2), &bindata("", GIF)).as_bytes(), &limits).is_ok()
        );
    }

    #[test]
    fn long_text_is_bounded_by_lowered_size() {
        let body = format!("<P><TEXT><CHAR>{}</CHAR></TEXT></P>", "a".repeat(10_000));
        let limits = HmlLimits {
            max_lowered_bytes: 1_000,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(doc(&body, "").as_bytes(), &limits));
        assert!(m.contains("중간 결과"), "{m}");
    }

    #[test]
    fn compressed_bindata_is_refused_not_inserted() {
        let text = doc(&pictures(1), &bindata(" Compress=\"true\"", GIF));
        let result = read_hml(text.as_bytes()).unwrap();
        assert!(
            result.document.bin_streams.is_empty(),
            "compressed bytes inserted as an image"
        );
        assert!(
            result.warnings.iter().any(|w| w.contains("Compress")),
            "{:?}",
            result.warnings
        );
        // Uncompressed control: the same picture is inserted.
        let plain =
            read_hml(doc(&pictures(1), &bindata(" Compress=\"false\"", GIF)).as_bytes()).unwrap();
        assert_eq!(plain.document.bin_streams.len(), 1);
    }

    /// Size of the lowering for `text` under no effective bound.
    fn lowered_len(text: &str) -> usize {
        let limits = HML_LIMITS;
        let mut lower = Lower {
            limits,
            ..Lower::default()
        };
        lower.collect_tail(text).unwrap();
        lower.run(text).unwrap();
        lower.html.len()
    }

    /// The lowering bound is exact: `n` bytes passes when the lowering is `n` bytes and fails
    /// at `n - 1`, for every append path (empty paragraphs, inline controls, CDATA, character
    /// references, row and cell markup).
    #[test]
    fn every_append_path_is_bounded_exactly() {
        let bodies = [
            "<P/><P/><P/>".to_string(),
            "<P><TEXT><CHAR>a</CHAR><TAB/><LINEBREAK/><NBSPACE/><HYPHEN/></TEXT></P>".into(),
            "<P><TEXT><CHAR><![CDATA[<&\"\"]]></CHAR></TEXT></P>".into(),
            "<P><TEXT><CHAR>&amp;&lt;&#x20AC;&quot;</CHAR></TEXT></P>".into(),
            "<P><TEXT><TABLE><ROW><CELL ColSpan=\"2\"><PARALIST><P/></PARALIST></CELL></ROW>\
             <ROW><CELL><PARALIST><P/></PARALIST></CELL><CELL><PARALIST><P/></PARALIST></CELL></ROW>\
             </TABLE></TEXT></P>"
                .into(),
        ];
        for body in bodies {
            let text = doc(&body, "");
            let n = lowered_len(&text);
            let at = HmlLimits {
                max_lowered_bytes: n,
                ..HML_LIMITS
            };
            assert!(read_hml_with_limits(text.as_bytes(), &at).is_ok(), "{body}");
            let under = HmlLimits {
                max_lowered_bytes: n - 1,
                ..HML_LIMITS
            };
            let m = limit_message(read_hml_with_limits(text.as_bytes(), &under));
            assert!(m.contains("중간 결과"), "{body}: {m}");
        }
    }

    /// A long unflushed paragraph buffer fails before it is flushed, even when the text arrives
    /// as CDATA or character references.
    #[test]
    fn pending_paragraph_buffer_is_bounded() {
        let limits = HmlLimits {
            max_lowered_bytes: 100,
            ..HML_LIMITS
        };
        for chunk in [
            "<![CDATA[aaaaaaaaaa]]>",
            "&#x41;&#x41;&#x41;&#x41;&#x41;",
            "",
        ] {
            let char_body = if chunk.is_empty() {
                "<TAB/>".repeat(200)
            } else {
                format!("<CHAR>{}</CHAR>", chunk.repeat(40))
            };
            let body = format!("<P><TEXT>{char_body}</TEXT></P>");
            let m = limit_message(read_hml_with_limits(doc(&body, "").as_bytes(), &limits));
            assert!(m.contains("중간 결과"), "{chunk}: {m}");
        }
    }

    fn one_cell_table(row: &str, rows: usize) -> String {
        format!("<P><TEXT><TABLE>{}</TABLE></TEXT></P>", row.repeat(rows))
    }

    /// Projected grid slots are bounded exactly, including the empty cells the IR builder
    /// synthesizes to complete a ragged grid, and across several tables.
    #[test]
    fn table_grid_slots_are_bounded_before_allocation() {
        let cell = "<CELL><PARALIST><P/></PARALIST></CELL>";
        // One wide row then narrow rows: 10 columns x 5 rows = 50 projected slots, 14 cells.
        let wide = "<ROW><CELL ColSpan=\"10\"><PARALIST><P/></PARALIST></CELL></ROW>";
        let narrow = format!("<ROW>{cell}</ROW>");
        let body = format!(
            "<P><TEXT><TABLE>{wide}{}</TABLE></TEXT></P>",
            narrow.repeat(4)
        );
        let at = HmlLimits {
            max_table_slots: 50,
            ..HML_LIMITS
        };
        let ok = read_hml_with_limits(doc(&body, "").as_bytes(), &at).unwrap();
        let table_cells: usize = ok
            .document
            .sections
            .iter()
            .flat_map(|s| &s.paragraphs)
            .flat_map(|p| &p.controls)
            .filter_map(|c| match c {
                hwp_model::Control::Table(t) => Some(t.cells.len()),
                _ => None,
            })
            .sum();
        // 1 wide + 4 narrow + 4x9 synthesized gap cells.
        assert_eq!(table_cells, 41);
        let under = HmlLimits {
            max_table_slots: 49,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(doc(&body, "").as_bytes(), &under));
        assert!(m.contains("표 칸 수"), "{m}");

        // A huge span is refused at placement, before any grid vector grows.
        let huge = "<P><TEXT><TABLE><ROW><CELL ColSpan=\"65535\" RowSpan=\"65535\">\
                    <PARALIST><P/></PARALIST></CELL></ROW></TABLE></TEXT></P>";
        let m = limit_message(read_hml(doc(huge, "").as_bytes()));
        assert!(m.contains("표 칸 수"), "{m}");

        // The budget is shared by every table in the document.
        let two = one_cell_table(&narrow, 30).repeat(2);
        let limits = HmlLimits {
            max_table_slots: 59,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(doc(&two, "").as_bytes(), &limits));
        assert!(m.contains("표 칸 수"), "{m}");
        let limits = HmlLimits {
            max_table_slots: 60,
            ..HML_LIMITS
        };
        assert!(read_hml_with_limits(doc(&two, "").as_bytes(), &limits).is_ok());

        // Rows with no cells still take one slot each (the builder adds a column and fills it).
        let empty_rows = one_cell_table("<ROW></ROW>", 10);
        let at = HmlLimits {
            max_table_slots: 10,
            ..HML_LIMITS
        };
        assert!(read_hml_with_limits(doc(&empty_rows, "").as_bytes(), &at).is_ok());
        let under = HmlLimits {
            max_table_slots: 9,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(
            doc(&empty_rows, "").as_bytes(),
            &under,
        ));
        assert!(m.contains("표 칸 수"), "{m}");

        // Dimensions past the IR's u16 are refused even under an unbounded slot budget.
        let unbounded = HmlLimits {
            max_table_slots: usize::MAX,
            ..HML_LIMITS
        };
        let too_many_rows = one_cell_table("<ROW></ROW>", IR_MAX_DIM + 1);
        let m = limit_message(read_hml_with_limits(
            doc(&too_many_rows, "").as_bytes(),
            &unbounded,
        ));
        assert!(m.contains("표 칸 수"), "{m}");
        let too_many_cols = format!(
            "<P><TEXT><TABLE><ROW><CELL ColSpan=\"65535\"><PARALIST><P/></PARALIST></CELL>\
             {cell}</ROW></TABLE></TEXT></P>"
        );
        let m = limit_message(read_hml_with_limits(
            doc(&too_many_cols, "").as_bytes(),
            &unbounded,
        ));
        assert!(m.contains("표 칸 수"), "{m}");
    }

    #[test]
    fn nested_tables_are_charged_before_their_cells_are_read() {
        // A 1x2 parent whose first cell holds a 3x1 child: parent 2 + child 3 = 5 slots.
        let child = "<P><TEXT><TABLE><ROW><CELL><PARALIST><P/></PARALIST></CELL></ROW>\
                     <ROW><CELL><PARALIST><P/></PARALIST></CELL></ROW>\
                     <ROW><CELL><PARALIST><P/></PARALIST></CELL></ROW></TABLE></TEXT></P>";
        let body = format!(
            "<P><TEXT><TABLE><ROW><CELL ColSpan=\"2\"><PARALIST>{child}</PARALIST></CELL>\
             </ROW></TABLE></TEXT></P>"
        );
        let at = HmlLimits {
            max_table_slots: 5,
            ..HML_LIMITS
        };
        assert!(read_hml_with_limits(doc(&body, "").as_bytes(), &at).is_ok());
        // At 4 the parent's 2 slots are reserved first, so the child's third row fails
        // inside the nesting, not after the parent closes.
        let under = HmlLimits {
            max_table_slots: 4,
            ..HML_LIMITS
        };
        let m = limit_message(read_hml_with_limits(doc(&body, "").as_bytes(), &under));
        assert!(m.contains("표 칸 수"), "{m}");
        // Ordering: the parent's two slots are reserved when its cell is placed, so the limit
        // fires inside the child (nesting level 2) at the child's third row. Charging the
        // parent only when it closes would let the child grow past the ancestor's reservation
        // and fail later, at level 1.
        assert!(
            m.contains("중첩 2단계"),
            "limit fired outside the child: {m}"
        );
    }

    /// A row of many cells is placed in linear time. With the former scan-from-zero
    /// placement this input took quadratic time in both the HML pass and the IR builder.
    #[test]
    fn wide_row_is_placed_in_linear_time() {
        let n = 60_000;
        let row = "<CELL><PARALIST><P/></PARALIST></CELL>".repeat(n);
        let body = format!("<P><TEXT><TABLE><ROW>{row}</ROW></TABLE></TEXT></P>");
        let started = std::time::Instant::now();
        let result = read_hml(doc(&body, "").as_bytes()).unwrap();
        let elapsed = started.elapsed();
        let cells: usize = result
            .document
            .sections
            .iter()
            .flat_map(|s| &s.paragraphs)
            .flat_map(|p| &p.controls)
            .filter_map(|c| match c {
                hwp_model::Control::Table(t) => Some(t.cells.len()),
                _ => None,
            })
            .sum();
        assert_eq!(cells, n);
        // Quadratic placement of 60k cells is ~1.8e9 steps per pass; linear is milliseconds.
        assert!(elapsed.as_secs() < 20, "wide row took {elapsed:?}");
    }

    #[test]
    fn sections_and_blank_paragraphs_are_kept() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\"><HEAD/><BODY>\
            <SECTION><P><TEXT><CHAR>one</CHAR></TEXT></P><P><TEXT/></P><P/>\
            <P><TEXT><TAB/></TEXT></P><P><TEXT><NBSPACE/></TEXT></P></SECTION>\
            <SECTION><P><TEXT><CHAR>two</CHAR></TEXT></P></SECTION>\
            </BODY></HWPML>";
        let result = read_hml(text.as_bytes()).unwrap();
        let doc = &result.document;
        assert_eq!(doc.sections.len(), 2, "one IR section per SECTION");
        // one, <TEXT/>, <P/>, TAB-only, NBSPACE-only.
        assert_eq!(doc.sections[0].paragraphs.len(), 5);
        let tab_para = &doc.sections[0].paragraphs[3];
        assert!(
            tab_para.chars.iter().any(|c| matches!(
                c,
                hwp_model::HwpChar::InlineCtrl { code, .. } if *code == hwp_model::ctrl_char::TAB
            )),
            "TAB lost: {:?}",
            tab_para.chars
        );
        assert!(
            doc.sections[1]
                .paragraphs
                .iter()
                .any(|p| p.plain_text().contains("two"))
        );
    }

    #[test]
    fn fields_and_empty_equations_are_reported() {
        let body = "<P><TEXT><FIELDBEGIN Type=\"Hyperlink\"/><CHAR>link</CHAR><FIELDEND/>\
                    <EQUATION/></TEXT></P>";
        let result = read_hml(doc(body, "").as_bytes()).unwrap();
        assert!(
            result.document.plain_text().contains("link"),
            "field text kept"
        );
        let w = result.warnings.join("\n");
        assert!(w.contains("FIELDBEGIN"), "{w}");
        assert!(w.contains("<EQUATION>"), "{w}");
        // The paired (Start) field form warns the same way.
        let paired = "<P><TEXT><FIELDBEGIN Type=\"Hyperlink\"></FIELDBEGIN><CHAR>x</CHAR>\
                      <FIELDEND></FIELDEND></TEXT></P>";
        let result = read_hml(doc(paired, "").as_bytes()).unwrap();
        assert!(result.warnings.join("\n").contains("FIELDBEGIN"));
    }

    /// With a BINITEM table, a refused mapped binary is never replaced by the BINDATA whose Id
    /// equals the reference number.
    #[test]
    fn binitem_table_is_authoritative_for_image_lookup() {
        let gif = hwp_convert::base64::encode(GIF);
        let text = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><HWPML Version=\"2.8\"><HEAD><MAPPINGTABLE>\
             <BINDATALIST><BINITEM BinData=\"7\"/></BINDATALIST></MAPPINGTABLE></HEAD>\
             <BODY><SECTION>{}</SECTION></BODY><TAIL><BINDATASTORAGE>\
             <BINDATA Id=\"7\" Encoding=\"Base64\" Compress=\"true\">{gif}</BINDATA>\
             <BINDATA Id=\"1\" Encoding=\"Base64\">{gif}</BINDATA>\
             </BINDATASTORAGE></TAIL></HWPML>",
            pictures(1)
        );
        let result = read_hml(text.as_bytes()).unwrap();
        assert!(
            result.document.bin_streams.is_empty(),
            "BINDATA 1 substituted for refused BINDATA 7"
        );
        assert!(
            result.warnings.iter().any(|w| w.contains("BinItem 1")),
            "{:?}",
            result.warnings
        );
        // Without a BINITEM table the direct Id match still applies.
        let direct = read_hml(doc(&pictures(1), &bindata("", GIF)).as_bytes()).unwrap();
        assert_eq!(direct.document.bin_streams.len(), 1);
    }
}

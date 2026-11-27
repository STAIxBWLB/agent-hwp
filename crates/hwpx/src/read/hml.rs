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
//!   spec structure; when no `BINITEM` table exists, `BinItem` is matched to `BINDATA@Id` directly.
//!
//! - `BINDATA@Compress="true"` is refused with a warning: the compressed stream format is not
//!   pinned by a genuine file. An absent `Compress` is read as uncompressed.
//!
//! Untrusted input is bounded by [`HML_LIMITS`] (input size, table depth, lowered size and
//! picture count), each checked before the allocation it guards.
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
}

/// Default limits: the input bound matches the HWPX XML part bound
/// ([`crate::NATIVE_PACKAGE_LIMITS`] `max_xml_uncompressed_bytes`), and the lowered output may
/// be at most four times that.
pub const HML_LIMITS: HmlLimits = HmlLimits {
    max_input_bytes: 64 * 1024 * 1024,
    max_table_depth: 32,
    max_lowered_bytes: 256 * 1024 * 1024,
    max_pictures: 4_096,
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
    let mut document = hwp_convert::from_html::from_html(&html)
        .map_err(|e| xml_err(&format!("IR 구성 실패: {e}")))?;
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
    Ok(HmlReadResult { document, warnings })
}

fn xml_err(message: &str) -> HwpxError {
    HwpxError::Xml {
        entry: ENTRY.to_string(),
        message: message.to_string(),
    }
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

    fn image_bytes(&self, bin_item: &str) -> Option<&Vec<u8>> {
        let via_item = bin_item
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.bin_items.get(i))
            .and_then(|id| id.as_ref())
            .and_then(|id| self.bin_data.get(id));
        via_item.or_else(|| self.bin_data.get(bin_item))
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
                        n if TRANSPARENT.contains(&n) => {}
                        _ => {
                            self.dropped.insert(name);
                            skip(r, &e)?;
                        }
                    }
                }
                Ok(Event::Empty(e)) => {
                    if e.local_name().into_inner() == "P" {
                        self.html.push_str("<p></p>");
                        emitted = true;
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
                    "TAB" => buf.push('\t'),
                    "LINEBREAK" => buf.push_str("<br/>"),
                    "NBSPACE" | "FWSPACE" => buf.push('\u{a0}'),
                    "HYPEN" | "HYPHEN" => buf.push('-'),
                    _ => {}
                },
                Ok(Event::Text(t)) if in_char > 0 => {
                    let s = t.xml10_content();
                    // Escaping grows text at most 6x (`"` -> `&quot;`).
                    self.reserve(buf.len() + s.len() * 6)?;
                    html_escape(&mut buf, &s);
                }
                Ok(Event::GeneralRef(g)) if in_char > 0 => {
                    if let Some(c) = resolve_entity(&g) {
                        html_escape(&mut buf, &c.to_string());
                    }
                }
                Ok(Event::CData(c)) if in_char > 0 => {
                    html_escape(&mut buf, &c.into_inner());
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
        // base64 grows 4/3; check the paragraph buffer plus this image before encoding it.
        self.reserve(buf.len() + data.len().div_ceil(3) * 4 + 64)?;
        let (ext, _) = hwp_convert::image::image_kind(data);
        if ext == "bin" {
            self.warnings.push(format!(
                "PICTURE BinItem {item}: 지원하지 않는 이미지 형식이라 건너뜁니다"
            ));
            return Ok(());
        }
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
        self.html.push_str("<table>");
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => match e.local_name().into_inner() {
                    "ROW" => self.html.push_str("<tr>"),
                    "CELL" => {
                        let span = |n: &str| {
                            attr(&e, n)
                                .and_then(|v| v.parse::<u16>().ok())
                                .filter(|&v| v > 1)
                        };
                        self.html.push_str("<td");
                        if let Some(c) = span("ColSpan") {
                            self.html.push_str(&format!(" colspan=\"{c}\""));
                        }
                        if let Some(rs) = span("RowSpan") {
                            self.html.push_str(&format!(" rowspan=\"{rs}\""));
                        }
                        self.html.push('>');
                        self.reserve(0)?;
                        self.blocks(r, "CELL")?;
                        self.html.push_str("</td>");
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
                    "ROW" => self.html.push_str("</tr>"),
                    "TABLE" => {
                        self.html.push_str("</table>");
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
}

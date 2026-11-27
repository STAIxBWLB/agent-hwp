//! PII detection and format-preserving masking for `ahwp redact` (issue #617, gap GM-10).
//!
//! # Privacy boundary
//!
//! Nothing this module returns to a caller carries a matched value: a [`Hit`] holds a category,
//! a segment address and a UTF-16 range, never text. The paragraph id in a hit is computed over
//! the paragraph **after** masking, so its content checksum covers only what the output document
//! already shows. The original matched strings live only inside [`RedactOutcome`], are never
//! exposed, and are used only to prove their absence from a re-read output
//! ([`RedactOutcome::originals_present_in`]).
//!
//! # Format preservation
//!
//! A mask replaces one ASCII letter or digit with one mask character whose UTF-16 width is one,
//! in place. Separators, digit grouping and every non-text character stay where they were, so
//! the paragraph's WCHAR length, its `CharShapeRun` boundaries, its inline and extended control
//! anchors and any `PARA_RANGE_TAG` offsets do not move. The line cache of a changed paragraph is
//! dropped, as every other text edit in this crate does, so the writer lays it out again.
//!
//! # HWP5 raw-backed controls
//!
//! An HWP5 text box, header, footer or note read from a saved file is written back from its raw
//! record subtree (`GenericControl::raw_children`), and its `paragraph_lists` are only a text
//! mirror. Both are masked: the mirror through the paragraph walk, the raw `PARA_TEXT` records by
//! an in-place UTF-16 unit rewrite with the same detectors. A table inside such a control is
//! masked cell by cell with the row and column-header context the mirror walk uses.
//!
//! # Values outside display text
//!
//! A field command (a hyperlink's `mailto:` URL) and an object's accessible description are
//! written by both writers but shown by no text view, so they are masked in place too and
//! reported as `field_command` and `object_description` hits. A caller must still re-read the
//! output, rescan it ([`scan_document`]) and sweep its bytes
//! ([`RedactOutcome::originals_present_in_bytes`]) before publishing.

use std::collections::BTreeMap;

use hwp_model::control::{Caption, Cell, Table};
use hwp_model::paragraph::{CharKind, HwpChar, Paragraph, char_kind, ctrl_char};
use hwp_model::{Control, Document, OpaqueRecord};
use regex::Regex;

use crate::segment_id::{SegmentPath, paragraph_id};

/// HWP5 `PARA_TEXT` tag (HWPTAG_BEGIN + 51, 한글문서파일형식 5.0 §4.3.2).
const PARA_TEXT: u16 = 0x10 + 51;

/// One PII class. The order of [`Category::ALL`] is the overlap priority: when two candidates
/// overlap, the earlier category wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// Resident or foreign registration number (주민등록번호/외국인등록번호).
    Rrn,
    /// 16-digit payment card number with a valid Luhn check digit.
    Card,
    /// Driver's license number (운전면허번호).
    Driver,
    /// Korean mobile or landline phone number.
    Phone,
    /// Bank account number; needs a bank or account label nearby unless accounts are loose.
    Account,
    /// Korean passport number.
    Passport,
    /// Email address.
    Email,
}

impl Category {
    /// Every category, in overlap-priority order.
    pub const ALL: [Category; 7] = [
        Category::Rrn,
        Category::Card,
        Category::Driver,
        Category::Phone,
        Category::Account,
        Category::Passport,
        Category::Email,
    ];

    /// The stable name used by `--only`, `--exclude` and the report.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Rrn => "rrn",
            Category::Card => "card",
            Category::Driver => "driver",
            Category::Phone => "phone",
            Category::Account => "account",
            Category::Passport => "passport",
            Category::Email => "email",
        }
    }

    /// Parses a stable name; `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        Category::ALL
            .into_iter()
            .find(|category| category.as_str() == name.trim())
    }
}

/// The enabled categories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorSet {
    enabled: Vec<Category>,
}

impl Default for DetectorSet {
    fn default() -> Self {
        Self {
            enabled: Category::ALL.to_vec(),
        }
    }
}

impl DetectorSet {
    /// `only` empty means every category; `exclude` then removes names. Unknown names and an
    /// empty result are errors. Names may be comma-separated inside one entry.
    pub fn from_lists(only: &[String], exclude: &[String]) -> Result<Self, String> {
        let parse_all = |items: &[String]| -> Result<Vec<Category>, String> {
            let mut out = Vec::new();
            for item in items {
                for name in item.split(',').filter(|name| !name.trim().is_empty()) {
                    let category = Category::parse(name).ok_or_else(|| {
                        format!(
                            "알 수 없는 검출 범주: {} (사용 가능: rrn, phone, email, card, account, passport, driver)",
                            name.trim()
                        )
                    })?;
                    out.push(category);
                }
            }
            Ok(out)
        };
        let only = parse_all(only)?;
        let exclude = parse_all(exclude)?;
        let enabled: Vec<Category> = Category::ALL
            .into_iter()
            .filter(|category| only.is_empty() || only.contains(category))
            .filter(|category| !exclude.contains(category))
            .collect();
        if enabled.is_empty() {
            return Err("활성화된 검출 범주가 없습니다 (--only/--exclude 확인)".to_string());
        }
        Ok(Self { enabled })
    }

    /// Whether `category` is enabled.
    pub fn contains(&self, category: Category) -> bool {
        self.enabled.contains(&category)
    }

    /// Enabled categories in priority order.
    pub fn categories(&self) -> &[Category] {
        &self.enabled
    }
}

/// Options for one redaction run.
#[derive(Debug, Clone)]
pub struct RedactOptions {
    pub detectors: DetectorSet,
    /// The replacement character; see [`validate_mask_char`].
    pub mask_char: char,
    /// When true (the default), an account number needs a bank or account label in the same
    /// paragraph, in the same table row or in the column's header cell.
    pub strict_accounts: bool,
}

impl Default for RedactOptions {
    fn default() -> Self {
        Self {
            detectors: DetectorSet::default(),
            mask_char: '*',
            strict_accounts: true,
        }
    }
}

/// Accepts a mask character only when masking stays width-preserving and the masked text can
/// never match a detector again: one UTF-16 unit, printable, not whitespace, not an ASCII letter
/// or digit and none of the characters the detectors treat as part of a value.
pub fn validate_mask_char(raw: &str) -> Result<char, String> {
    let mut chars = raw.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err("--mask-char는 문자 하나여야 합니다".to_string());
    };
    if c.len_utf16() != 1
        || c.is_control()
        || c.is_whitespace()
        || c.is_ascii_alphanumeric()
        || c.is_numeric()
        || "-._%+@".contains(c)
    {
        return Err(
            "--mask-char는 영문자·숫자·공백·제어문자·구분자(-._%+@)가 아닌 BMP 문자여야 합니다"
                .to_string(),
        );
    }
    Ok(c)
}

/// A match in one flat text, as char indices `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextHit {
    pub category: Category,
    pub start: usize,
    pub end: usize,
}

struct Patterns {
    rrn: Regex,
    card: Regex,
    driver: Regex,
    phone: Regex,
    account: Regex,
    passport: Regex,
    email: Regex,
    account_label: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: std::sync::OnceLock<Patterns> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        rrn: Regex::new(r"[0-9]{6}-?[0-9]{7}").expect("rrn regex"),
        card: Regex::new(r"[0-9]{4}[- ]?[0-9]{4}[- ]?[0-9]{4}[- ]?[0-9]{4}").expect("card regex"),
        driver: Regex::new(r"[0-9]{2}-[0-9]{2}-[0-9]{6}-[0-9]{2}").expect("driver regex"),
        // The parenthesized area code of official documents comes first: `(02)2100-1234`,
        // `02)2100-1234`, `(031) 123-4567`.
        phone: Regex::new(
            r"\(0[0-9]{1,2}\) ?[0-9]{3,4}[-. ][0-9]{4}|0[0-9]{1,2}\) ?[0-9]{3,4}[-. ][0-9]{4}|0[0-9]{1,2}[-. ]?[0-9]{3,4}[-. ]?[0-9]{4}",
        )
        .expect("phone regex"),
        account: Regex::new(r"[0-9]{2,6}(?:-[0-9]{2,7}){1,4}").expect("account regex"),
        passport: Regex::new(r"[MSRODG](?:[0-9]{8}|[0-9]{3}[A-Z][0-9]{4})")
            .expect("passport regex"),
        email: Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
            .expect("email regex"),
        account_label: Regex::new(
            r"(?i)계좌|은행|예금주|입금|이체|뱅크|농협|우체국|새마을금고|신협|수협|account|bank",
        )
        .expect("account label regex"),
    })
}

/// Whether the char at `i` continues a value across the match edge: an ASCII letter or digit,
/// or a `-`/`.` that is itself followed (outward) by a digit.
fn continues(text: &[char], i: Option<usize>, outward: isize) -> bool {
    let Some(i) = i else { return false };
    let Some(&c) = text.get(i) else { return false };
    if c.is_ascii_alphanumeric() {
        return true;
    }
    if c == '-' || c == '.' {
        let next = i as isize + outward;
        return next >= 0
            && text
                .get(next as usize)
                .is_some_and(|next| next.is_ascii_digit());
    }
    false
}

fn bounded(text: &[char], start: usize, end: usize) -> bool {
    // An opening parenthesis already separates the value from a preceding label (`TEL(02)...`).
    let left_open = text.get(start) == Some(&'(');
    (left_open || !continues(text, start.checked_sub(1), -1)) && !continues(text, Some(end), 1)
}

const AREA_CODES: [&str; 18] = [
    "02", "031", "032", "033", "041", "042", "043", "044", "051", "052", "053", "054", "055",
    "061", "062", "063", "064", "070",
];

/// A phone number written with a parenthesized area code: `(02)2100-1234`, `02)2100-1234`,
/// `(031) 123-4567`, `(010)1234-5678`.
fn valid_parenthesized_phone(m: &[char]) -> bool {
    let body: &[char] = if m.first() == Some(&'(') { &m[1..] } else { m };
    let Some(close) = body.iter().position(|c| *c == ')') else {
        return false;
    };
    let area: String = body[..close].iter().collect();
    let rest: Vec<u32> = digits(&body[close + 1..]);
    let mobile = matches!(area.as_str(), "010" | "011" | "016" | "017" | "018" | "019");
    if mobile {
        return rest.len() == 8 || (rest.len() == 7 && area != "010");
    }
    AREA_CODES.contains(&area.as_str())
}

/// Eight digits that read as a calendar date (`19xx`/`20xx`, valid month and day): a code like
/// `D20261125` is a dated reference, not a passport number.
fn looks_like_date(d: &[u32]) -> bool {
    if d.len() != 8 || !(d[0] == 1 && d[1] == 9 || d[0] == 2 && d[1] == 0) {
        return false;
    }
    valid_birth_date(&d[2..])
}

fn digits(chars: &[char]) -> Vec<u32> {
    chars.iter().filter_map(|c| c.to_digit(10)).collect()
}

fn valid_birth_date(d: &[u32]) -> bool {
    let month = d[2] * 10 + d[3];
    let day = d[4] * 10 + d[5];
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 29,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

/// The legacy RRN/FRN check digit (weights 2..9,2..5, mod 11). Numbers issued since October 2020
/// no longer carry it, so it is demanded only of the unseparated 13-digit form.
fn rrn_checksum_ok(d: &[u32]) -> bool {
    const WEIGHTS: [u32; 12] = [2, 3, 4, 5, 6, 7, 8, 9, 2, 3, 4, 5];
    let sum: u32 = d.iter().zip(WEIGHTS).map(|(a, w)| a * w).sum();
    let check = (11 - sum % 11) % 10;
    let frn_check = (13 - sum % 11) % 10;
    check == d[12] || (d[6] >= 5 && frn_check == d[12])
}

fn luhn_ok(d: &[u32]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &n)| {
            if i % 2 == 1 {
                let doubled = n * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                n
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn separators(chars: &[char]) -> Vec<char> {
    chars
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_alphanumeric())
        .collect()
}

fn validate(category: Category, m: &[char]) -> bool {
    match category {
        Category::Rrn => {
            let d = digits(m);
            let hyphen = m.contains(&'-');
            d.len() == 13
                && valid_birth_date(&d)
                && (1..=8).contains(&d[6])
                && (hyphen || rrn_checksum_ok(&d))
        }
        Category::Card => {
            let seps = separators(m);
            let consistent =
                seps.is_empty() || (seps.len() == 3 && seps.iter().all(|s| *s == seps[0]));
            let d = digits(m);
            consistent && d.len() == 16 && d[0] != 0 && luhn_ok(&d)
        }
        Category::Driver => {
            let d = digits(m);
            let region = d[0] * 10 + d[1];
            (11..=28).contains(&region)
        }
        Category::Phone => {
            if m.contains(&')') {
                return valid_parenthesized_phone(m);
            }
            let d = digits(m);
            let seps = separators(m);
            let mobile =
                d.len() >= 10 && d[0] == 0 && d[1] == 1 && matches!(d[2], 0 | 1 | 6 | 7 | 8 | 9);
            if mobile {
                let consistent = seps.is_empty() || (seps.len() == 2 && seps[0] == seps[1]);
                return consistent && (d.len() == 11 || (d.len() == 10 && d[2] != 0));
            }
            // Landlines need both separators, so a bare digit run is never read as a phone.
            if seps.len() != 2 || seps[0] != seps[1] {
                return false;
            }
            let area: String = m.iter().take_while(|c| c.is_ascii_digit()).collect();
            AREA_CODES.contains(&area.as_str())
        }
        Category::Account => (10..=14).contains(&digits(m).len()),
        Category::Passport => !looks_like_date(&digits(m)),
        Category::Email => true,
    }
}

fn regex_for(category: Category) -> &'static Regex {
    let p = patterns();
    match category {
        Category::Rrn => &p.rrn,
        Category::Card => &p.card,
        Category::Driver => &p.driver,
        Category::Phone => &p.phone,
        Category::Account => &p.account,
        Category::Passport => &p.passport,
        Category::Email => &p.email,
    }
}

/// Detects every enabled category in `text`. `context` is extra label text (a table row and its
/// column header) that may license an account number; the paragraph text itself always counts.
/// Returns non-overlapping hits sorted by start, as char indices into `text`.
pub fn detect(text: &str, context: &str, opts: &RedactOptions) -> Vec<TextHit> {
    let chars: Vec<char> = text.chars().collect();
    // byte offset -> char index
    let mut byte_to_char = BTreeMap::new();
    for (ci, (bi, _)) in text.char_indices().enumerate() {
        byte_to_char.insert(bi, ci);
    }
    byte_to_char.insert(text.len(), chars.len());
    let account_licensed = !opts.strict_accounts
        || patterns().account_label.is_match(text)
        || patterns().account_label.is_match(context);

    let mut accepted: Vec<TextHit> = Vec::new();
    for &category in opts.detectors.categories() {
        if category == Category::Account && !account_licensed {
            continue;
        }
        let re = regex_for(category);
        let mut at = 0usize;
        while at < text.len() {
            let Some(found) = re.find_at(text, at) else {
                break;
            };
            let start = byte_to_char[&found.start()];
            let end = byte_to_char[&found.end()];
            let ok = bounded(&chars, start, end) && validate(category, &chars[start..end]);
            let overlaps = accepted
                .iter()
                .any(|hit| hit.start < end && start < hit.end);
            if ok && !overlaps {
                accepted.push(TextHit {
                    category,
                    start,
                    end,
                });
                at = found.end();
            } else {
                // Retry one char later so a rejected candidate never hides a valid one inside it.
                let next = text[found.start()..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
                at = found.start() + next;
            }
        }
    }
    accepted.sort_by_key(|hit| hit.start);
    accepted
}

/// Relative positions inside a match that the mask replaces. Every position is an ASCII letter
/// or digit; separators and kept parts are never listed.
pub fn mask_positions(category: Category, m: &[char]) -> Vec<usize> {
    let digit_positions: Vec<usize> = (0..m.len()).filter(|&i| m[i].is_ascii_digit()).collect();
    match category {
        // Keep the birth date and the gender digit: 900101-1******.
        Category::Rrn => digit_positions[7..].to_vec(),
        // Keep the last four digits.
        Category::Card | Category::Account => {
            let keep = digit_positions.len().saturating_sub(4);
            digit_positions[..keep].to_vec()
        }
        // Keep the region code.
        Category::Driver => digit_positions[2..].to_vec(),
        Category::Phone => {
            let groups = digit_groups(m);
            if groups.len() >= 3 {
                groups[1..groups.len() - 1].concat()
            } else {
                // Unseparated mobile: keep the 3-digit prefix and the last four digits.
                let n = digit_positions.len();
                digit_positions[3..n - 4].to_vec()
            }
        }
        // Keep the leading letter.
        Category::Passport => (1..m.len()).collect(),
        Category::Email => {
            let at = m.iter().position(|c| *c == '@').unwrap_or(m.len());
            let local: Vec<usize> = (0..at).filter(|&i| m[i].is_ascii_alphanumeric()).collect();
            if local.len() >= 3 {
                local[1..].to_vec()
            } else {
                local
            }
        }
    }
}

fn digit_groups(m: &[char]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current = Vec::new();
    for (i, c) in m.iter().enumerate() {
        if c.is_ascii_digit() {
            current.push(i);
        } else if !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// Masks `text` in place for display-free tests and tools. Returns the hits.
pub fn mask_text(text: &str, context: &str, opts: &RedactOptions) -> (String, Vec<TextHit>) {
    let mut chars: Vec<char> = text.chars().collect();
    let hits = detect(text, context, opts);
    for hit in &hits {
        let span: Vec<char> = chars[hit.start..hit.end].to_vec();
        for pos in mask_positions(hit.category, &span) {
            chars[hit.start + pos] = opts.mask_char;
        }
    }
    (chars.into_iter().collect(), hits)
}

/// Where a paragraph sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Body,
    Cell,
    Caption,
    HeaderFooter,
    Note,
    Object,
    /// The command of a field control (a hyperlink URL, for example). The path addresses the
    /// control (`host paragraph path` + control index) and the offsets are UTF-16 units of the
    /// command; the paragraph id is the host paragraph's.
    FieldCommand,
    /// The accessible description of an object (picture, table, shape or text box). The path
    /// addresses the control and the offsets are UTF-16 units of the description; there is no
    /// paragraph, so the paragraph id is empty.
    ObjectDescription,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Body => "body",
            SegmentKind::Cell => "cell",
            SegmentKind::Caption => "caption",
            SegmentKind::HeaderFooter => "header_footer",
            SegmentKind::Note => "note",
            SegmentKind::Object => "object",
            SegmentKind::FieldCommand => "field_command",
            SegmentKind::ObjectDescription => "object_description",
        }
    }
}

/// A content-free address of one paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    /// `section.paragraph[.control.cell.paragraph | .control.seq ...]`, the segment-envelope
    /// path scheme. A caption appends `control` and the caption paragraph index to its host
    /// paragraph's path; such a path is a location, not a resolvable address.
    pub path: String,
    /// Segment id of the paragraph after masking.
    pub paragraph_id: String,
    /// Recursive document-order table index (the `--set-cell` numbering) for cell paragraphs.
    pub table_index: Option<usize>,
    pub row: Option<u16>,
    pub col: Option<u16>,
}

/// One masked occurrence: no text, only where and what kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub category: Category,
    pub segment: Segment,
    /// UTF-16 (WCHAR) offsets in the paragraph, controls counted at their stored width.
    pub start: u32,
    pub end: u32,
}

/// The result of [`redact_document`].
#[derive(Default)]
pub struct RedactOutcome {
    pub hits: Vec<Hit>,
    /// HWP5 raw `PARA_TEXT` records rewritten in place.
    pub raw_records_patched: usize,
    /// Masks applied inside HWP5 raw records, per category. A raw-backed control's text is also
    /// mirrored in its `paragraph_lists`, whose hits are already in [`Self::hits`] with an
    /// address, so these are not added to [`Self::counts`]: they record that the bytes the
    /// writer will emit were masked too.
    pub raw_counts: BTreeMap<Category, usize>,
    /// Whether the document holds pictures, whose pixels are never inspected.
    pub has_images: bool,
    originals: Vec<String>,
}

impl std::fmt::Debug for RedactOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the originals.
        f.debug_struct("RedactOutcome")
            .field("hits", &self.hits)
            .field("raw_records_patched", &self.raw_records_patched)
            .field("raw_counts", &self.raw_counts)
            .field("has_images", &self.has_images)
            .finish_non_exhaustive()
    }
}

impl RedactOutcome {
    /// Per-category counts of addressed hits.
    pub fn counts(&self) -> BTreeMap<Category, usize> {
        let mut counts = BTreeMap::new();
        for hit in &self.hits {
            *counts.entry(hit.category).or_insert(0) += 1;
        }
        counts
    }

    /// How many of the masked original values still occur in `text`. Returns a count only.
    pub fn originals_present_in(&self, text: &str) -> usize {
        self.originals
            .iter()
            .filter(|original| text.contains(original.as_str()))
            .count()
    }

    /// How many of the masked original values occur, UTF-8 or UTF-16LE encoded, in any of
    /// `blobs` (raw streams, package entries, a whole file). Returns a count only.
    pub fn originals_present_in_bytes(&self, blobs: &[&[u8]]) -> usize {
        let contains = |hay: &[u8], needle: &[u8]| {
            !needle.is_empty() && hay.windows(needle.len()).any(|window| window == needle)
        };
        self.originals
            .iter()
            .filter(|original| {
                let utf8 = original.as_bytes();
                let utf16: Vec<u8> = original.encode_utf16().flat_map(u16::to_le_bytes).collect();
                blobs
                    .iter()
                    .any(|blob| contains(blob, utf8) || contains(blob, &utf16))
            })
            .count()
    }
}

/// The detection view of one paragraph: flat chars and, per flat char, its index in
/// `Paragraph::chars`. Text maps 1:1; a non-breaking or fixed-width space reads as a space and a
/// hyphen control as `-`; every other control reads as a line break, which no value crosses.
fn flatten(para: &Paragraph) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut map = Vec::new();
    for (i, ch) in para.chars.iter().enumerate() {
        let c = match ch {
            HwpChar::Text(c) => *c,
            HwpChar::CharCtrl(code)
                if *code == ctrl_char::NB_SPACE || *code == ctrl_char::FW_SPACE =>
            {
                ' '
            }
            HwpChar::CharCtrl(code) if *code == ctrl_char::HYPHEN => '-',
            _ => '\n',
        };
        text.push(c);
        map.push(i);
    }
    (text, map)
}

/// Masks one paragraph's own chars. Returns `(category, wchar start, wchar end)` per hit.
fn redact_chars(
    para: &mut Paragraph,
    context: &str,
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<(Category, u32, u32)> {
    let (text, map) = flatten(para);
    let hits = detect(&text, context, opts);
    if hits.is_empty() {
        return Vec::new();
    }
    let flat: Vec<char> = text.chars().collect();
    let mut wpos = Vec::with_capacity(para.chars.len() + 1);
    let mut acc = 0u32;
    for ch in &para.chars {
        wpos.push(acc);
        acc += ch.wchar_width();
    }
    wpos.push(acc);
    let mut out = Vec::new();
    for hit in hits {
        let span = &flat[hit.start..hit.end];
        originals.push(span.iter().collect());
        for pos in mask_positions(hit.category, span) {
            let index = map[hit.start + pos];
            if let HwpChar::Text(c) = para.chars[index]
                && c.is_ascii_alphanumeric()
            {
                para.chars[index] = HwpChar::Text(opts.mask_char);
            }
        }
        let start = wpos[map[hit.start]];
        let end = wpos[map[hit.end - 1] + 1];
        out.push((hit.category, start, end));
    }
    para.line_segs.clear();
    out
}

struct Walker<'o> {
    opts: &'o RedactOptions,
    outcome: RedactOutcome,
    table_counter: usize,
}

struct Place {
    kind: SegmentKind,
    path: SegmentPath,
    table: Option<(usize, u16, u16)>,
    context: String,
}

impl Walker<'_> {
    fn paragraph(&mut self, para: &mut Paragraph, place: Place) -> bool {
        let found = redact_chars(para, &place.context, self.opts, &mut self.outcome.originals);
        let changed = !found.is_empty();
        if changed {
            let id = paragraph_id(&place.path, para);
            for (category, start, end) in found {
                self.outcome.hits.push(Hit {
                    category,
                    segment: Segment {
                        kind: place.kind,
                        path: place.path.to_string(),
                        paragraph_id: id.clone(),
                        table_index: place.table.map(|t| t.0),
                        row: place.table.map(|t| t.1),
                        col: place.table.map(|t| t.2),
                    },
                    start,
                    end,
                });
            }
        }
        let mut nested_changed = false;
        // Field commands (hyperlink URLs, click-here commands) are not display text: a value in
        // one survives in the CTRL_HEADER payload and the HWPX `Command` parameter otherwise.
        let mut command_hits = Vec::new();
        for (ci, control) in para.controls.iter_mut().enumerate() {
            if let Control::Generic(generic) = control
                && crate::field::is_field_ctrl_id(&generic.ctrl_id)
            {
                for found in
                    redact_field_command(&mut generic.data, self.opts, &mut self.outcome.originals)
                {
                    command_hits.push((ci, found));
                }
            }
        }
        if !command_hits.is_empty() {
            nested_changed = true;
            let id = paragraph_id(&place.path, para);
            for (ci, (category, start, end)) in command_hits {
                let mut path = place.path.clone();
                path.indices.push(ci);
                self.outcome.hits.push(Hit {
                    category,
                    segment: Segment {
                        kind: SegmentKind::FieldCommand,
                        path: path.to_string(),
                        paragraph_id: id.clone(),
                        table_index: None,
                        row: None,
                        col: None,
                    },
                    start,
                    end,
                });
            }
        }
        for (ci, control) in para.controls.iter_mut().enumerate() {
            let mut base = place.path.clone();
            base.indices.push(ci);
            nested_changed |= self.control(control, base);
        }
        changed || nested_changed
    }

    fn control(&mut self, control: &mut Control, base: SegmentPath) -> bool {
        let described = self.object_description(control, &base);
        described | self.control_content(control, base)
    }

    /// Masks the accessible description of an object (HWPX `hp:shapeComment`, HWP5 common
    /// object header description BSTR), which both writers emit but no text view shows.
    fn object_description(&mut self, control: &mut Control, base: &SegmentPath) -> bool {
        let opts = self.opts;
        let originals = &mut self.outcome.originals;
        let mut found = Vec::new();
        match control {
            Control::Picture(picture) => {
                if let Some(description) = &mut picture.description {
                    found.extend(mask_string(description, opts, originals));
                }
                let bytes =
                    mask_object_description_bytes(&mut picture.common_data, opts, originals);
                if found.is_empty() {
                    found.extend(bytes);
                }
            }
            Control::Table(table) => {
                found.extend(mask_object_description_bytes(
                    &mut table.common_data,
                    opts,
                    originals,
                ));
            }
            Control::Generic(generic) => {
                for shape in &mut generic.gso_shapes {
                    if let Some(description) = &mut shape.description {
                        found.extend(mask_string(description, opts, originals));
                    }
                }
                // Only an HWP5 `gso ` header carries the common object description BSTR.
                if &generic.ctrl_id == b"gso " {
                    let bytes = mask_object_description_bytes(&mut generic.data, opts, originals);
                    // A description read into both the model and the bytes is counted once.
                    if found.is_empty() {
                        found.extend(bytes);
                    }
                }
                if !found.is_empty() {
                    generic.hwpx_raw_xml = None;
                }
            }
            _ => {}
        }
        for (category, start, end) in &found {
            self.outcome.hits.push(Hit {
                category: *category,
                segment: Segment {
                    kind: SegmentKind::ObjectDescription,
                    path: base.to_string(),
                    paragraph_id: String::new(),
                    table_index: None,
                    row: None,
                    col: None,
                },
                start: *start,
                end: *end,
            });
        }
        !found.is_empty()
    }

    fn control_content(&mut self, control: &mut Control, base: SegmentPath) -> bool {
        match control {
            Control::Table(table) => {
                let index = self.table_counter;
                self.table_counter += 1;
                let mut changed = self.table(table, index, &base);
                if let Some(caption) = &mut table.caption {
                    changed |= self.caption(caption, &base);
                }
                changed
            }
            Control::Picture(picture) => {
                self.outcome.has_images = true;
                match &mut picture.caption {
                    Some(caption) => self.caption(caption, &base),
                    None => false,
                }
            }
            Control::Generic(generic) => {
                let kind = match &generic.ctrl_id {
                    b"head" | b"foot" => SegmentKind::HeaderFooter,
                    b"fn  " | b"en  " => SegmentKind::Note,
                    _ => SegmentKind::Object,
                };
                let mut changed = false;
                let mut seq = 0usize;
                for list in &mut generic.paragraph_lists {
                    for p in &mut list.paragraphs {
                        let mut path = base.clone();
                        path.indices.push(seq);
                        seq += 1;
                        changed |= self.paragraph(
                            p,
                            Place {
                                kind,
                                path,
                                table: None,
                                context: String::new(),
                            },
                        );
                    }
                }
                if let Some(caption) = &mut generic.caption {
                    changed |= self.caption(caption, &base);
                }
                if !generic.raw_children.is_empty() {
                    let records = redact_raw(
                        &mut generic.raw_children,
                        self.opts,
                        &mut self.outcome.originals,
                        &mut self.outcome.raw_counts,
                    );
                    self.outcome.raw_records_patched += records;
                    changed |= records > 0;
                }
                if changed {
                    generic.hwpx_raw_xml = None;
                }
                changed
            }
            Control::SectionDef(_) => false,
        }
    }

    fn caption(&mut self, caption: &mut Caption, base: &SegmentPath) -> bool {
        let mut changed = false;
        for (pi, p) in caption.paragraphs.iter_mut().enumerate() {
            let mut path = base.clone();
            path.indices.push(pi);
            changed |= self.paragraph(
                p,
                Place {
                    kind: SegmentKind::Caption,
                    path,
                    table: None,
                    context: String::new(),
                },
            );
        }
        changed
    }

    fn table(&mut self, table: &mut Table, index: usize, base: &SegmentPath) -> bool {
        let contexts: Vec<String> = table
            .cells
            .iter()
            .map(|cell| row_context(table, cell))
            .collect();
        let mut changed = false;
        for (ci, cell) in table.cells.iter_mut().enumerate() {
            let (row, col) = (cell.row, cell.col);
            for (pi, p) in cell.paragraphs.iter_mut().enumerate() {
                let mut path = base.clone();
                path.indices.push(ci);
                path.indices.push(pi);
                changed |= self.paragraph(
                    p,
                    Place {
                        kind: SegmentKind::Cell,
                        path,
                        table: Some((index, row, col)),
                        context: contexts[ci].clone(),
                    },
                );
            }
        }
        changed
    }
}

fn cell_text(cell: &Cell) -> String {
    cell.paragraphs
        .iter()
        .map(Paragraph::own_text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The other cells of the same row plus the column's first-row header cell.
fn row_context(table: &Table, target: &Cell) -> String {
    let mut out = String::new();
    for cell in &table.cells {
        let same_row = cell.row == target.row;
        let header =
            cell.row == 0 && cell.col <= target.col && target.col < cell.col + cell.col_span.max(1);
        if (same_row || header) && !std::ptr::eq(cell, target) {
            out.push_str(&cell_text(cell));
            out.push('\n');
        }
    }
    out
}

/// HWP5 `CTRL_HEADER`, `LIST_HEADER` and `TABLE` tags (HWPTAG_BEGIN + 55, 56, 61, §4.3).
const CTRL_HEADER: u16 = 0x10 + 55;
const LIST_HEADER: u16 = 0x10 + 56;
const TABLE: u16 = 0x10 + 61;

/// Rewrites HWP5 raw `PARA_TEXT` records in place. Returns the number of records changed.
///
/// A raw table (`TABLE` followed by cell `LIST_HEADER`s and their paragraphs, all siblings) is
/// masked cell by cell with the same row and column-header context the mirror walk uses, so an
/// account licensed only by its header is masked in both views alike.
fn redact_raw(
    records: &mut [OpaqueRecord],
    opts: &RedactOptions,
    originals: &mut Vec<String>,
    counts: &mut BTreeMap<Category, usize>,
) -> usize {
    let contexts = raw_table_contexts(records);
    let mut changed = 0;
    for (index, record) in records.iter_mut().enumerate() {
        if record.tag == PARA_TEXT {
            changed += mask_raw_para_text(record, "", opts, originals, counts);
        }
        // A field nested in a raw control keeps its command in its own CTRL_HEADER, after the
        // reversed control id.
        if record.tag == CTRL_HEADER && record.data.len() >= 4 {
            let mut ctrl_id = [
                record.data[0],
                record.data[1],
                record.data[2],
                record.data[3],
            ];
            ctrl_id.reverse();
            if crate::field::is_field_ctrl_id(&ctrl_id) {
                let found = redact_field_command(&mut record.data[4..], opts, originals);
                for (category, _, _) in &found {
                    *counts.entry(*category).or_insert(0) += 1;
                }
                changed += usize::from(!found.is_empty());
            }
        }
        if record.tag == PARA_HEADER {
            let context = contexts.get(&index).map_or("", String::as_str);
            for child in &mut record.children {
                if child.tag == PARA_TEXT {
                    changed += mask_raw_para_text(child, context, opts, originals, counts);
                }
            }
            for child in &mut record.children {
                if child.tag != PARA_TEXT {
                    changed += redact_raw(std::slice::from_mut(child), opts, originals, counts);
                }
            }
            continue;
        }
        changed += redact_raw(&mut record.children, opts, originals, counts);
    }
    changed
}

/// HWP5 `PARA_HEADER` tag (HWPTAG_BEGIN + 50).
const PARA_HEADER: u16 = 0x10 + 50;

fn mask_raw_para_text(
    record: &mut OpaqueRecord,
    context: &str,
    opts: &RedactOptions,
    originals: &mut Vec<String>,
    counts: &mut BTreeMap<Category, usize>,
) -> usize {
    let found = redact_para_text_bytes(&mut record.data, context, opts, originals);
    for category in &found {
        *counts.entry(*category).or_insert(0) += 1;
    }
    usize::from(!found.is_empty())
}

/// One raw table cell: its address, span and the sibling indices of its paragraphs.
struct RawCell {
    row: u16,
    col: u16,
    col_span: u16,
    text: String,
    paragraphs: Vec<usize>,
}

/// For a sibling list holding a raw table, the account-label context of every cell paragraph,
/// keyed by the paragraph's sibling index. Empty for any other list.
fn raw_table_contexts(records: &[OpaqueRecord]) -> BTreeMap<usize, String> {
    let mut out = BTreeMap::new();
    if !records.iter().any(|record| record.tag == TABLE) {
        return out;
    }
    let mut cells: Vec<RawCell> = Vec::new();
    let mut after_table = false;
    for (index, record) in records.iter().enumerate() {
        match record.tag {
            TABLE => after_table = true,
            // A cell LIST_HEADER: paragraph count, list attributes, col, row, col span (§4.3.9.1).
            LIST_HEADER if after_table && record.data.len() >= 14 => {
                let u16_at = |o: usize| u16::from_le_bytes([record.data[o], record.data[o + 1]]);
                cells.push(RawCell {
                    col: u16_at(8),
                    row: u16_at(10),
                    col_span: u16_at(12),
                    text: String::new(),
                    paragraphs: Vec::new(),
                });
            }
            LIST_HEADER => {}
            PARA_HEADER => {
                if let Some(cell) = cells.last_mut() {
                    cell.paragraphs.push(index);
                    for child in &record.children {
                        if child.tag == PARA_TEXT {
                            cell.text.push_str(&decode_para_text(&child.data).0);
                            cell.text.push(' ');
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (ci, target) in cells.iter().enumerate() {
        let mut context = String::new();
        for (oi, cell) in cells.iter().enumerate() {
            let same_row = cell.row == target.row;
            let header = cell.row == 0
                && cell.col <= target.col
                && target.col < cell.col + cell.col_span.max(1);
            if (same_row || header) && oi != ci {
                context.push_str(&cell.text);
                context.push('\n');
            }
        }
        for &paragraph in &target.paragraphs {
            out.insert(paragraph, context.clone());
        }
    }
    out
}

/// The detection view of a `PARA_TEXT` payload and, per flat char, the UTF-16 unit it came from
/// (`None` for controls and surrogates, which are never masked).
fn decode_para_text(data: &[u8]) -> (String, Vec<Option<usize>>) {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut text = String::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if u < 32 {
            match char_kind(u) {
                CharKind::Char => {
                    text.push(match u {
                        ctrl_char::NB_SPACE | ctrl_char::FW_SPACE => ' ',
                        ctrl_char::HYPHEN => '-',
                        _ => '\n',
                    });
                    map.push(None);
                    i += 1;
                }
                CharKind::Inline | CharKind::Extended => {
                    text.push('\n');
                    map.push(None);
                    i += 8;
                }
            }
        } else if (0xD800..0xE000).contains(&u) {
            text.push('\n');
            map.push(None);
            i += if (0xD800..0xDC00).contains(&u) { 2 } else { 1 };
        } else {
            text.push(char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'));
            map.push(Some(i));
            i += 1;
        }
    }
    (text, map)
}

/// Masks one `PARA_TEXT` payload (UTF-16LE, §4.3.2 with the table 6 control classes) in place,
/// unit for unit. `context` licenses account numbers as in [`detect`]. Returns the category of
/// each hit masked.
fn redact_para_text_bytes(
    data: &mut [u8],
    context: &str,
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<Category> {
    let (text, map) = decode_para_text(data);
    let hits = detect(&text, context, opts);
    let flat: Vec<char> = text.chars().collect();
    let mask = opts.mask_char as u32 as u16;
    for hit in &hits {
        let span = &flat[hit.start..hit.end];
        originals.push(span.iter().collect());
        for pos in mask_positions(hit.category, span) {
            if let Some(unit) = map[hit.start + pos] {
                data[unit * 2..unit * 2 + 2].copy_from_slice(&mask.to_le_bytes());
            }
        }
    }
    hits.into_iter().map(|hit| hit.category).collect()
}

/// Masks a UTF-16LE string in place, unit for unit. With `unescape`, a backslash escape (field
/// commands escape `:` and `;`) is read as the escaped char so a value split by an escape is still
/// seen whole; the backslash itself maps to no unit and is never masked. Returns
/// `(category, start, end)` per hit in UTF-16 units of `bytes`.
fn mask_utf16(
    bytes: &mut [u8],
    unescape: bool,
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<(Category, u32, u32)> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut text = String::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if unescape && u == u16::from(b'\\') && i + 1 < units.len() {
            i += 1;
            continue;
        }
        match char::from_u32(u32::from(u)) {
            Some(c) if !c.is_control() => {
                text.push(c);
                map.push(Some(i));
            }
            _ => {
                text.push('\n');
                map.push(None);
            }
        }
        i += 1;
    }
    let hits = detect(&text, "", opts);
    let flat: Vec<char> = text.chars().collect();
    let mask = (opts.mask_char as u32 as u16).to_le_bytes();
    let mut out = Vec::new();
    for hit in &hits {
        let span = &flat[hit.start..hit.end];
        originals.push(span.iter().collect());
        for pos in mask_positions(hit.category, span) {
            if let Some(unit) = map[hit.start + pos] {
                bytes[unit * 2..unit * 2 + 2].copy_from_slice(&mask);
            }
        }
        let mut units_in_hit = map[hit.start..hit.end].iter().flatten();
        let first = units_in_hit.next().copied();
        let last = units_in_hit.last().copied().or(first);
        if let (Some(first), Some(last)) = (first, last) {
            out.push((hit.category, first as u32, last as u32 + 1));
        }
    }
    out
}

/// Masks a model string (an object description) in place, char for char.
fn mask_string(
    value: &mut String,
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<(Category, u32, u32)> {
    let mut bytes: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let found = mask_utf16(&mut bytes, false, opts, originals);
    if !found.is_empty() {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        *value = String::from_utf16_lossy(&units);
    }
    found
}

/// Masks the description BSTR of an HWP5 common object header (`common` without the control id:
/// u16 length at byte 40, WCHARs from byte 42, 한글문서파일형식 5.0 §4.3.9.2 table 69).
fn mask_object_description_bytes(
    common: &mut [u8],
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<(Category, u32, u32)> {
    let Some(len) = common
        .get(40..42)
        .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
    else {
        return Vec::new();
    };
    match common.get_mut(42..42 + len * 2) {
        Some(bytes) => mask_utf16(bytes, false, opts, originals),
        None => Vec::new(),
    }
}

/// Masks a field command (the `%hlk` URL of a hyperlink, a click-here command and so on) held in
/// a field control's `CTRL_HEADER` payload: attributes(4), extra(1), length(2), WCHAR[length],
/// id(4) (§4.3.10.4). Both writers emit the command from these bytes (HWP5 as the payload, HWPX as
/// the `Command` string parameter), so masking them unit for unit covers both outputs. The
/// instance id, derived from the command when the field was created, is re-derived from the
/// masked command so it cannot be used to confirm a guessed original. Returns
/// `(category, start, end)` per hit in UTF-16 units of the command.
fn redact_field_command(
    data: &mut [u8],
    opts: &RedactOptions,
    originals: &mut Vec<String>,
) -> Vec<(Category, u32, u32)> {
    if data.len() < 7 {
        return Vec::new();
    }
    let len = usize::from(u16::from_le_bytes([data[5], data[6]]));
    let Some(end) = 7usize.checked_add(len * 2).filter(|&end| end <= data.len()) else {
        return Vec::new();
    };
    let out = mask_utf16(&mut data[7..end], true, opts, originals);
    if !out.is_empty() && end + 4 <= data.len() {
        let masked: Vec<u16> = data[7..end]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let id = crate::field::field_instance_id(&String::from_utf16_lossy(&masked));
        data[end..end + 4].copy_from_slice(&id.to_le_bytes());
    }
    out
}

/// Detects and masks every enabled category in the document, in place.
pub fn redact_document(doc: &mut Document, opts: &RedactOptions) -> RedactOutcome {
    let mut walker = Walker {
        opts,
        outcome: RedactOutcome::default(),
        table_counter: 0,
    };
    for (si, section) in doc.sections.iter_mut().enumerate() {
        for (pi, para) in section.paragraphs.iter_mut().enumerate() {
            walker.paragraph(
                para,
                Place {
                    kind: SegmentKind::Body,
                    path: SegmentPath {
                        section: si,
                        indices: vec![pi],
                    },
                    table: None,
                    context: String::new(),
                },
            );
        }
    }
    let mut outcome = walker.outcome;
    outcome.has_images |= !doc.bin_streams.is_empty();
    outcome
}

/// Rescans a document without changing it: per-category counts of remaining hits.
pub fn scan_document(doc: &Document, opts: &RedactOptions) -> BTreeMap<Category, usize> {
    let mut copy = doc.clone();
    let outcome = redact_document(&mut copy, opts);
    let mut counts = outcome.counts();
    // A raw record the mirror does not cover still counts as a remaining hit.
    for (category, n) in outcome.raw_counts {
        let entry = counts.entry(category).or_insert(0);
        *entry = (*entry).max(n);
    }
    counts
}

/// Rescans plain text (for example a preview stream): per-category counts.
pub fn scan_text(text: &str, opts: &RedactOptions) -> BTreeMap<Category, usize> {
    let mut counts = BTreeMap::new();
    for line in text.split('\n') {
        for hit in detect(line, "", opts) {
            *counts.entry(hit.category).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use hwp_model::ids::CharShapeId;

    fn opts() -> RedactOptions {
        RedactOptions::default()
    }

    fn masked(text: &str) -> String {
        mask_text(text, "", &opts()).0
    }

    fn cats(text: &str) -> Vec<&'static str> {
        detect(text, "", &opts())
            .into_iter()
            .map(|hit| hit.category.as_str())
            .collect()
    }

    /// A synthetic 13-digit RRN body with a valid legacy check digit.
    fn rrn_with_checksum(first12: &str) -> String {
        let d: Vec<u32> = first12.chars().map(|c| c.to_digit(10).unwrap()).collect();
        let weights = [2, 3, 4, 5, 6, 7, 8, 9, 2, 3, 4, 5];
        let sum: u32 = d.iter().zip(weights).map(|(a, w)| a * w).sum();
        format!("{first12}{}", (11 - sum % 11) % 10)
    }

    #[test]
    fn rrn_hyphenated_masks_after_gender_digit() {
        assert_eq!(
            masked("주민번호 900101-1234567 입니다"),
            "주민번호 900101-1****** 입니다"
        );
        assert_eq!(cats("900101-3234567"), ["rrn"]);
    }

    #[test]
    fn rrn_rejects_bad_dates_and_gender() {
        assert!(cats("901301-1234567").is_empty(), "month 13");
        assert!(cats("900230-1234567").is_empty(), "Feb 30");
        assert!(cats("900101-9234567").is_empty(), "gender 9");
    }

    #[test]
    fn rrn_bare_needs_checksum() {
        let good = rrn_with_checksum("900101123456");
        assert_eq!(cats(&good), ["rrn"]);
        let last = good.chars().last().unwrap().to_digit(10).unwrap();
        let bad = format!("{}{}", &good[..12], (last + 1) % 10);
        assert!(!cats(&bad).contains(&"rrn"));
    }

    #[test]
    fn phone_masks_middle_group() {
        assert_eq!(masked("연락처: 010-1234-5678"), "연락처: 010-****-5678");
        assert_eq!(masked("01012345678"), "010****5678");
        assert_eq!(masked("02-123-4567"), "02-***-4567");
        assert_eq!(masked("031.123.4567"), "031.***.4567");
    }

    #[test]
    fn phone_rejects_mixed_separators_and_bare_landline() {
        assert!(cats("010-1234.5678").is_empty());
        assert!(cats("0212345678").is_empty());
        assert!(cats("099-123-4567").is_empty(), "unknown area code");
    }

    #[test]
    fn email_keeps_first_char_and_domain() {
        assert_eq!(
            masked("메일 hong.gd@example.com 으로"),
            "메일 h***.**@example.com 으로"
        );
        assert_eq!(masked("ab@example.org"), "**@example.org");
    }

    #[test]
    fn card_requires_luhn() {
        assert_eq!(masked("4111-1111-1111-1111"), "****-****-****-1111");
        assert_eq!(masked("4111111111111111"), "************1111");
        assert!(cats("4111-1111-1111-1112").is_empty());
        assert!(cats("4111-1111 1111-1111").is_empty(), "mixed separators");
    }

    #[test]
    fn account_needs_label_unless_loose() {
        assert!(cats("번호 123-456-789012").is_empty());
        assert_eq!(
            masked("입금 계좌: 123-456-789012"),
            "입금 계좌: ***-***-**9012"
        );
        let loose = RedactOptions {
            strict_accounts: false,
            ..opts()
        };
        assert_eq!(detect("번호 123-456-789012", "", &loose).len(), 1);
        assert_eq!(detect("123-456-789012", "예금주 은행", &opts()).len(), 1);
    }

    #[test]
    fn passport_and_driver() {
        assert_eq!(masked("여권 M12345678"), "여권 M********");
        assert_eq!(masked("여권 M123A4567"), "여권 M********");
        assert_eq!(masked("면허 11-23-456789-01"), "면허 11-**-******-**");
        assert!(cats("99-23-456789-01").is_empty(), "region out of range");
        assert!(cats("XM12345678").is_empty(), "embedded in a word");
    }

    #[test]
    fn budget_and_date_numbers_are_not_pii() {
        for text in [
            "예산 1,234,567,000원",
            "2026-11-25",
            "2026.11.25.",
            "제2026-123호",
            "사업비 12,345천원",
            "문서번호 2026-00123",
            "1234-5678",
            "버전 1.2.3",
        ] {
            assert!(cats(text).is_empty(), "{text}");
        }
        // Even with an account label, a date is too short to be an account.
        assert!(detect("계좌 개설일 2026-11-25", "", &opts()).is_empty());
    }

    #[test]
    fn digit_runs_are_not_split_into_matches() {
        assert!(cats("9010-1234-5678").is_empty());
        assert!(cats("010-1234-56789").is_empty());
        assert!(cats("1900101-1234567").is_empty());
    }

    #[test]
    fn masked_text_rescans_clean() {
        let text = "900101-1234567 010-1234-5678 a.b.c@example.com 4111-1111-1111-1111 \
                    계좌 123-456-789012 M12345678 11-23-456789-01";
        let (out, hits) = mask_text(text, "", &opts());
        assert_eq!(hits.len(), 7);
        assert!(detect(&out, "", &opts()).is_empty());
        assert_eq!(out.chars().count(), text.chars().count());
    }

    #[test]
    fn only_and_exclude() {
        let set = DetectorSet::from_lists(&["rrn,phone".into()], &[]).unwrap();
        assert_eq!(set.categories(), [Category::Rrn, Category::Phone]);
        let set = DetectorSet::from_lists(&[], &["email".into()]).unwrap();
        assert!(!set.contains(Category::Email));
        assert!(DetectorSet::from_lists(&["nope".into()], &[]).is_err());
        assert!(DetectorSet::from_lists(&["rrn".into()], &["rrn".into()]).is_err());
    }

    #[test]
    fn mask_char_validation() {
        assert_eq!(validate_mask_char("*").unwrap(), '*');
        assert_eq!(validate_mask_char("●").unwrap(), '●');
        for bad in ["", "**", "a", "1", "-", "@", " ", "\u{1F600}"] {
            assert!(validate_mask_char(bad).is_err(), "{bad:?}");
        }
    }

    fn text_para(segments: &[&str], boundaries: &[u32]) -> Paragraph {
        let mut para = Paragraph::default();
        for s in segments {
            for c in s.chars() {
                para.chars.push(HwpChar::Text(c));
            }
        }
        para.char_shape_runs = boundaries
            .iter()
            .enumerate()
            .map(|(i, &pos)| (pos, CharShapeId(i as u16)))
            .collect();
        para
    }

    fn tab() -> HwpChar {
        HwpChar::InlineCtrl {
            code: ctrl_char::TAB,
            payload: vec![0; 12],
        }
    }

    /// #543 invariant: every placement of three run boundaries inside and around a value, with
    /// an 8-WCHAR control before it, after it or both. Checks: char count and widths unchanged,
    /// runs and controls untouched, only the masked positions changed, the reported WCHAR range
    /// equals the value's range in both coordinate spaces, ranges are ordered and disjoint, and
    /// the result rescans clean.
    #[test]
    fn exhaustive_run_boundary_invariant() {
        let value = "010-1234-5678";
        let value_len = value.chars().count();
        let expected_mask = "010-****-5678";
        for layout in 0..4u8 {
            let lead_tab = layout & 1 == 1;
            let trail_tab = layout & 2 == 2;
            // prefix "a " (+tab) value (+tab) " b" then a second value
            let mut base = Paragraph::default();
            base.chars.push(HwpChar::Text('가'));
            base.chars.push(HwpChar::Text(' '));
            if lead_tab {
                base.chars.push(tab());
            }
            let value_start = base.chars.len();
            for c in value.chars() {
                base.chars.push(HwpChar::Text(c));
            }
            if trail_tab {
                base.chars.push(tab());
            }
            base.chars.push(HwpChar::Text(' '));
            let second_start = base.chars.len();
            for c in "02-123-4567".chars() {
                base.chars.push(HwpChar::Text(c));
            }
            base.chars.push(HwpChar::CharCtrl(ctrl_char::PARA_BREAK));
            let widths: Vec<u32> = base.chars.iter().map(HwpChar::wchar_width).collect();
            let w_at = |i: usize| widths[..i].iter().sum::<u32>();
            let total_w = w_at(widths.len());
            let value_w = (w_at(value_start), w_at(value_start + value_len));
            let second_w = (w_at(second_start), w_at(second_start + 11));
            // Three boundaries chosen from every WCHAR offset in [value start - 2, value end + 2].
            let lo = value_w.0.saturating_sub(2);
            let hi = (value_w.1 + 2).min(total_w - 1);
            for a in lo..=hi {
                for b in a + 1..=hi {
                    for c in b + 1..=hi {
                        let mut para = base.clone();
                        para.char_shape_runs = vec![
                            (0, CharShapeId(0)),
                            (a, CharShapeId(1)),
                            (b, CharShapeId(2)),
                            (c, CharShapeId(3)),
                        ];
                        para.char_shape_runs.dedup_by_key(|r| r.0);
                        let before = para.clone();
                        let mut originals = Vec::new();
                        let found = redact_chars(&mut para, "", &opts(), &mut originals);
                        assert_eq!(found.len(), 2, "layout {layout} runs {a},{b},{c}");
                        assert_eq!((found[0].1, found[0].2), value_w);
                        assert_eq!((found[1].1, found[1].2), second_w);
                        assert!(found[0].2 <= found[1].1, "ordered and disjoint");
                        assert_eq!(para.char_shape_runs, before.char_shape_runs);
                        assert_eq!(para.controls, before.controls);
                        assert_eq!(para.chars.len(), before.chars.len());
                        let after_w: Vec<u32> =
                            para.chars.iter().map(HwpChar::wchar_width).collect();
                        assert_eq!(after_w, widths);
                        for (i, (x, y)) in before.chars.iter().zip(&para.chars).enumerate() {
                            let in_value = (value_start..value_start + value_len).contains(&i);
                            let in_second = (second_start..second_start + 11).contains(&i);
                            if !(in_value || in_second) {
                                assert_eq!(x, y, "char {i} outside a value changed");
                            }
                        }
                        let got: String = para.chars[value_start..value_start + value_len]
                            .iter()
                            .map(|ch| match ch {
                                HwpChar::Text(c) => *c,
                                _ => '?',
                            })
                            .collect();
                        assert_eq!(got, expected_mask);
                        let (text, _) = flatten(&para);
                        assert!(detect(&text, "", &opts()).is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn control_inside_value_breaks_the_match() {
        let mut para = text_para(&["010-1234"], &[0]);
        para.chars.push(tab());
        for c in "-5678".chars() {
            para.chars.push(HwpChar::Text(c));
        }
        let mut originals = Vec::new();
        assert!(redact_chars(&mut para, "", &opts(), &mut originals).is_empty());
    }

    #[test]
    fn para_text_bytes_masked_unit_for_unit() {
        let mut units: Vec<u16> = "전화 ".encode_utf16().collect();
        // an extended control (8 units) before the value
        units.extend([
            ctrl_char::OBJECT,
            0x6C74,
            0x2062,
            0,
            0,
            0,
            0,
            ctrl_char::OBJECT,
        ]);
        units.extend("010-1234-5678".encode_utf16());
        units.push(ctrl_char::PARA_BREAK);
        let mut data: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let before_len = data.len();
        let mut originals = Vec::new();
        assert_eq!(
            redact_para_text_bytes(&mut data, "", &opts(), &mut originals),
            [Category::Phone]
        );
        assert_eq!(data.len(), before_len);
        let after: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(&after[..11], &units[..11], "prefix and control untouched");
        assert_eq!(String::from_utf16_lossy(&after[11..24]), "010-****-5678");
    }

    #[test]
    fn document_walk_reports_cells_without_text() {
        let mut cell_para = text_para(&["123-456-789012"], &[0]);
        cell_para
            .chars
            .push(HwpChar::CharCtrl(ctrl_char::PARA_BREAK));
        let label_para = text_para(&["입금 계좌"], &[0]);
        let mk_cell = |row, col, p: Paragraph| Cell {
            list_attr: 0,
            col,
            row,
            col_span: 1,
            row_span: 1,
            width: hwp_model::HwpUnit(0),
            height: hwp_model::HwpUnit(0),
            margins: [0; 4],
            border_fill: Default::default(),
            header_tail: Vec::new(),
            paragraphs: vec![p],
        };
        let table = Table {
            common_data: Vec::new(),
            placement: None,
            attr: 0,
            rows: 1,
            cols: 2,
            cell_spacing: 0,
            inner_margins: [0; 4],
            row_cell_counts: vec![2],
            border_fill: Default::default(),
            table_tail: Vec::new(),
            cells: vec![mk_cell(0, 0, label_para), mk_cell(0, 1, cell_para)],
            caption: None,
            extras: Vec::new(),
        };
        let mut host = text_para(&["주민 900101-1234567"], &[0]);
        host.controls.push(Control::Table(table));
        let mut doc = Document::default();
        doc.sections.push(hwp_model::Section {
            paragraphs: vec![host],
            extras: Vec::new(),
        });
        let outcome = redact_document(&mut doc, &opts());
        assert_eq!(outcome.hits.len(), 2);
        let cell_hit = &outcome.hits[1];
        assert_eq!(cell_hit.category, Category::Account);
        assert_eq!(cell_hit.segment.kind, SegmentKind::Cell);
        assert_eq!(cell_hit.segment.path, "0.0.0.1.0");
        assert_eq!(cell_hit.segment.table_index, Some(0));
        assert_eq!(
            (cell_hit.segment.row, cell_hit.segment.col),
            (Some(0), Some(1))
        );
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("1234567") && !debug.contains("789012"));
        assert_eq!(outcome.originals_present_in(&doc.plain_text()), 0);
        assert!(scan_document(&doc, &opts()).is_empty());
    }

    #[test]
    fn phone_parenthesized_area_code() {
        assert_eq!(masked("전화 (02)2100-1234"), "전화 (02)****-1234");
        assert_eq!(masked("전화 02)2100-1234"), "전화 02)****-1234");
        assert_eq!(masked("(031) 123-4567"), "(031) ***-4567");
        assert_eq!(masked("TEL(010)1234-5678"), "TEL(010)****-5678");
        assert!(cats("(099)123-4567").is_empty(), "unknown area code");
        assert!(cats("(02)2100").is_empty(), "no subscriber group");
    }

    #[test]
    fn passport_rejects_dated_codes() {
        assert!(cats("D20261125").is_empty());
        assert!(cats("S19991231").is_empty());
        assert_eq!(cats("M12345678"), ["passport"]);
    }

    fn field_data(command: &str) -> Vec<u8> {
        crate::field::make_field_command_data(b"%hlk", command)
    }

    fn command_of(data: &[u8]) -> String {
        let len = usize::from(u16::from_le_bytes([data[5], data[6]]));
        decode_utf16le_for_test(&data[7..7 + len * 2])
    }

    fn decode_utf16le_for_test(b: &[u8]) -> String {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn hyperlink_command_is_masked_in_place() {
        let command = "mailto\\:hong.gildong@example.com;1;0;0;";
        let mut data = field_data(command);
        let len = data.len();
        let id_before = data[len - 8..len - 4].to_vec();
        let mut originals = Vec::new();
        let found = redact_field_command(&mut data, &opts(), &mut originals);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, Category::Email);
        assert_eq!(data.len(), len, "width preserved");
        assert_eq!(
            command_of(&data),
            "mailto\\:h***.*******@example.com;1;0;0;"
        );
        assert_ne!(data[len - 8..len - 4], id_before[..], "id re-derived");
        // A paragraph walk reaches it and reports a field_command hit.
        let mut para = text_para(&["메일"], &[0]);
        para.controls
            .push(Control::Generic(hwp_model::control::GenericControl {
                ctrl_id: *b"%hlk",
                data: field_data(command),
                paragraph_lists: Vec::new(),
                extras: Vec::new(),
                raw_children: Vec::new(),
                gso_shapes: Vec::new(),
                equation: None,
                chart: None,
                column_def: None,
                caption: None,
                hwpx_raw_xml: None,
                container_box: None,
            }));
        let mut doc = Document::default();
        doc.sections.push(hwp_model::Section {
            paragraphs: vec![para],
            extras: Vec::new(),
        });
        let outcome = redact_document(&mut doc, &opts());
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].segment.kind, SegmentKind::FieldCommand);
        assert_eq!(outcome.hits[0].segment.path, "0.0.0");
        assert!(scan_document(&doc, &opts()).is_empty());
    }

    fn record(tag: u16, data: Vec<u8>, children: Vec<OpaqueRecord>) -> OpaqueRecord {
        OpaqueRecord {
            tag,
            data,
            children,
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn cell_header(col: u16, row: u16) -> Vec<u8> {
        let mut data = vec![0u8; 46];
        data[0] = 1;
        data[8..10].copy_from_slice(&col.to_le_bytes());
        data[10..12].copy_from_slice(&row.to_le_bytes());
        data[12..14].copy_from_slice(&1u16.to_le_bytes());
        data[14..16].copy_from_slice(&1u16.to_le_bytes());
        data
    }

    #[test]
    fn raw_table_cells_use_header_context() {
        // A raw text box holding a 2x2 table: header row "계좌번호", data row an account.
        let para = |text: &str| {
            record(
                PARA_HEADER,
                vec![0; 22],
                vec![record(PARA_TEXT, utf16(text), Vec::new())],
            )
        };
        let table = record(
            CTRL_HEADER,
            b" lbt".to_vec(),
            vec![
                record(TABLE, vec![0; 24], Vec::new()),
                record(LIST_HEADER, cell_header(0, 0), Vec::new()),
                para("계좌번호"),
                record(LIST_HEADER, cell_header(1, 0), Vec::new()),
                para("비고"),
                record(LIST_HEADER, cell_header(0, 1), Vec::new()),
                para("123-456-789012"),
                record(LIST_HEADER, cell_header(1, 1), Vec::new()),
                para("없음"),
            ],
        );
        let mut records = vec![record(
            PARA_HEADER,
            vec![0; 22],
            vec![record(PARA_TEXT, utf16("표"), Vec::new()), table],
        )];
        let mut originals = Vec::new();
        let mut counts = BTreeMap::new();
        assert_eq!(
            redact_raw(&mut records, &opts(), &mut originals, &mut counts),
            1
        );
        assert_eq!(counts.get(&Category::Account), Some(&1));
        let cell = &records[0].children[1].children[6].children[0].data;
        assert_eq!(decode_utf16le_for_test(cell), "***-***-**9012");
    }

    #[test]
    fn raw_nested_field_command_is_masked() {
        let mut header = b"klh%".to_vec();
        header.extend(field_data("mailto\\:hong.gildong@example.com;1;0;0;"));
        let mut records = vec![record(
            PARA_HEADER,
            vec![0; 22],
            vec![record(CTRL_HEADER, header, Vec::new())],
        )];
        let mut originals = Vec::new();
        let mut counts = BTreeMap::new();
        assert_eq!(
            redact_raw(&mut records, &opts(), &mut originals, &mut counts),
            1
        );
        let data = &records[0].children[0].data[4..];
        assert!(!command_of(data).contains("hong.gildong"));
    }
}

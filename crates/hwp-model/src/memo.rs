//! Memos (comments), GB-7.
//!
//! A memo is an anchor field plus a body. Both formats store the anchor as a field whose
//! Command string is `MEMO/<shape>/<number>/<filetime-lo>/<filetime-hi>/<author>/<rest>`
//! (measured on four Hancom-saved HWP5 and HWPX pairs). The body lives elsewhere:
//!
//! - HWP5: the anchor is FIELD_START `%%me` whose CTRL_HEADER ctrl_id reads `%unk`. Every memo
//!   body is a `MEMO_LIST` (u32 memo number) + `LIST_HEADER` + paragraphs triple among the
//!   extra records of the **last paragraph of the section**, whichever paragraph holds the memo.
//! - HWPX: `hp:fieldBegin type="MEMO"` carries the body inline as `hp:subList`, which the
//!   reader keeps as the field's `paragraph_lists`.
//!
//! This module derives the structured view without changing the IR, so the HWP5 identity
//! round-trip keeps its records untouched. The creation time is the UTC FILETIME from the
//! Command string; HWPX's `CreateDateTime` parameter is local wall-clock time labelled `Z`
//! and is never used.

use serde::Serialize;

use crate::control::Control;
use crate::document::{Document, Section};
use crate::opaque::OpaqueRecord;
use crate::paragraph::{HwpChar, Paragraph};

/// HWPTAG_MEMO_LIST (HWPTAG_BEGIN + 77).
pub const MEMO_LIST_TAG: u16 = 0x5D;
/// HWPTAG_LIST_HEADER (HWPTAG_BEGIN + 56).
pub const LIST_HEADER_TAG: u16 = 0x48;
/// HWPTAG_PARA_HEADER (HWPTAG_BEGIN + 50).
pub const PARA_HEADER_TAG: u16 = 0x42;
/// HWPTAG_PARA_TEXT (HWPTAG_BEGIN + 51).
pub const PARA_TEXT_TAG: u16 = 0x43;

/// One memo as read from either format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Memo {
    /// Section index (0-based).
    pub section: usize,
    /// 1-based memo number from the Command string (HWP5 `MEMO_LIST` carries the same value).
    pub number: u32,
    /// Author account name. Personal data: callers print it only on explicit request.
    pub author: String,
    /// Creation time as a Windows FILETIME (100 ns ticks since 1601-01-01 UTC).
    pub created_filetime: Option<u64>,
    /// Text the memo is anchored to (between the field start and end).
    pub anchor_text: String,
    /// Memo body text, one string per body paragraph.
    pub body: Vec<String>,
}

impl Memo {
    /// Creation time as RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`), when the FILETIME is present.
    pub fn created_utc(&self) -> Option<String> {
        self.created_filetime.map(filetime_to_rfc3339)
    }
}

/// Parsed `MEMO/...` Command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoCommand {
    pub number: u32,
    pub filetime: Option<u64>,
    pub author: String,
}

/// Parses a memo field's Command string, or `None` when it is not a memo command.
pub fn parse_memo_command(command: &str) -> Option<MemoCommand> {
    let mut parts = command.split('/');
    if parts.next()? != "MEMO" {
        return None;
    }
    let _shape = parts.next()?;
    let number = parts.next()?.parse().ok()?;
    let lo: Option<u32> = parts.next().and_then(|p| p.parse().ok());
    let hi: Option<u32> = parts.next().and_then(|p| p.parse().ok());
    let filetime = match (lo, hi) {
        (Some(lo), Some(hi)) => Some(u64::from(hi) << 32 | u64::from(lo)),
        _ => None,
    };
    let author = parts.next().unwrap_or_default().to_string();
    Some(MemoCommand {
        number,
        filetime,
        author,
    })
}

/// Shape id every measured memo command carries (`MEMO/65535/...`).
pub const MEMO_SHAPE_ID: u16 = 65535;

/// Everything after the author in a measured memo command (`\;;` in all four fixtures).
pub const MEMO_COMMAND_TAIL: &str = "\\;;";

/// Builds the field command for a memo, the inverse of [`parse_memo_command`].
///
/// Measured shape (07 L3): `MEMO/65535/<number>/<FILETIME low>/<FILETIME high>/<author>/...`.
/// The FILETIME half is UTC. A missing creation time is written as zero ticks, which parses back as
/// `Some(0)` (1601-01-01), not as an absent time.
pub fn build_memo_command(number: u32, filetime: Option<u64>, author: &str) -> String {
    let ft = filetime.unwrap_or(0);
    format!(
        "MEMO/{MEMO_SHAPE_ID}/{number}/{}/{}/{author}/{MEMO_COMMAND_TAIL}",
        ft as u32,
        (ft >> 32) as u32
    )
}

/// The current time as a Windows FILETIME (100 ns ticks since 1601-01-01 UTC).
pub fn filetime_now() -> u64 {
    /// Seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (elapsed.as_secs() + EPOCH_DIFF_SECS) * 10_000_000 + u64::from(elapsed.subsec_nanos()) / 100
}

/// HWP5 char-block payload for a memo field begin.
///
/// Measured on every memo in the four fixtures: the reversed `%%me` control id followed by zeroed
/// info bytes. The block itself is `[code][this payload][code]` (07 L1).
pub fn hwp5_field_begin_payload() -> [u8; 12] {
    let mut payload = [0u8; 12];
    payload[..4].copy_from_slice(b"em%%");
    payload
}

/// HWP5 char-block payload for a memo field end (07 L4).
///
/// `em%` + NUL, a flag byte (`0x05` on the last memo of the paragraph, `0x01` on earlier ones),
/// `ff ff 00`, then the memo number. The flag is independent of the anchor's span length.
pub fn hwp5_field_end_payload(number: u32, last: bool) -> [u8; 12] {
    let mut payload = [0u8; 12];
    payload[..4].copy_from_slice(b"em%\0");
    payload[4] = if last { 0x05 } else { 0x01 };
    payload[5] = 0xff;
    payload[6] = 0xff;
    payload[7] = 0x00;
    payload[8..12].copy_from_slice(&number.to_le_bytes());
    payload
}

/// Converts a FILETIME to RFC 3339 UTC with second precision.
pub fn filetime_to_rfc3339(filetime: u64) -> String {
    // Seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF: i64 = 11_644_473_600;
    let secs = (filetime / 10_000_000) as i64 - EPOCH_DIFF;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Field command data without ctrl_id: attributes(4), extra(1), length(2), WCHAR[length], ...
fn field_command(data: &[u8]) -> Option<String> {
    let len = usize::from(u16::from_le_bytes([*data.get(5)?, *data.get(6)?]));
    let bytes = data.get(7..7 + len * 2)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Whether a generic control is a memo anchor (`%unk`/`%%me` with a `MEMO/` Command string).
pub fn is_memo_anchor(g: &crate::control::GenericControl) -> bool {
    matches!(&g.ctrl_id, b"%unk" | b"%%me")
        && field_command(&g.data).is_some_and(|c| c.starts_with("MEMO/"))
}

/// A memo anchor's parsed Command together with the raw string, for the writers that synthesize
/// a container from the IR.
pub fn memo_anchor_command(g: &crate::control::GenericControl) -> Option<(MemoCommand, String)> {
    if !is_memo_anchor(g) {
        return None;
    }
    let command = field_command(&g.data)?;
    parse_memo_command(&command).map(|parsed| (parsed, command))
}

/// The memo number a memo anchor carries (07 L3).
pub fn memo_number(g: &crate::control::GenericControl) -> Option<u32> {
    memo_anchor_command(g).map(|(parsed, _)| parsed.number)
}

/// Whether a field control is a memo anchor: HWP5 `%unk`/`%%me` or HWPX `MEMO`, recognized by
/// its `MEMO/` Command string so a genuine `%unk` field of another kind never matches.
fn memo_command_of(control: &Control) -> Option<MemoCommand> {
    let Control::Generic(g) = control else {
        return None;
    };
    if !matches!(&g.ctrl_id, b"%unk" | b"%%me") {
        return None;
    }
    parse_memo_command(&field_command(&g.data)?)
}

fn paragraph_text(p: &Paragraph) -> String {
    let mut s = String::new();
    for ch in &p.chars {
        crate::text::char_text(ch, &mut s);
    }
    s
}

/// HWP5 PARA_TEXT decoding for memo bodies kept as opaque records: text characters only.
fn opaque_para_text(para: &OpaqueRecord) -> String {
    let Some(text) = para.children.iter().find(|c| c.tag == PARA_TEXT_TAG) else {
        return String::new();
    };
    let units: Vec<u16> = text
        .data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Lift each unit into the shared `HwpChar` form and project it with `text::char_text`, the
    // mapping every text path uses, so a body read from HWP5 records keeps its line breaks,
    // tabs, hyphens and nb/fw spaces exactly as the same body read from HWPX does.
    let mut out = String::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        let ch = match crate::paragraph::char_kind(u) {
            crate::paragraph::CharKind::Char if u < 32 => {
                i += 1;
                HwpChar::CharCtrl(u)
            }
            crate::paragraph::CharKind::Char => {
                // A surrogate pair is one character; a lone surrogate becomes U+FFFD.
                let (c, width) = match char::decode_utf16(units[i..].iter().copied()).next() {
                    Some(Ok(c)) => (c, c.len_utf16()),
                    _ => (char::REPLACEMENT_CHARACTER, 1),
                };
                i += width;
                HwpChar::Text(c)
            }
            crate::paragraph::CharKind::Inline => {
                i += 8;
                HwpChar::InlineCtrl {
                    code: u,
                    payload: Vec::new(),
                }
            }
            crate::paragraph::CharKind::Extended => {
                i += 8;
                continue;
            }
        };
        crate::text::char_text(&ch, &mut out);
    }
    out
}

/// HWP5 memo bodies from the extras of a section's last paragraph: number -> body paragraphs.
fn hwp5_bodies(section: &Section) -> Vec<(u32, Vec<String>)> {
    let Some(last) = section.paragraphs.last() else {
        return Vec::new();
    };
    let mut out: Vec<(u32, Vec<String>)> = Vec::new();
    for extra in &last.extras {
        match extra.tag {
            MEMO_LIST_TAG if extra.data.len() >= 4 => {
                let n = u32::from_le_bytes([
                    extra.data[0],
                    extra.data[1],
                    extra.data[2],
                    extra.data[3],
                ]);
                out.push((n, Vec::new()));
            }
            PARA_HEADER_TAG => {
                if let Some((_, body)) = out.last_mut() {
                    body.push(opaque_para_text(extra));
                }
            }
            _ => {}
        }
    }
    out
}

/// Every memo in the document, in the order its anchor sits in the text.
///
/// A paragraph is walked character by character, and a control is entered where its block sits,
/// so a memo anchored in a table that precedes a direct anchor in the same paragraph is reported
/// first. Within one control the order is [`nested_paragraph_lists`]'s: table cells, then the
/// caption.
pub fn memos(doc: &Document) -> Vec<Memo> {
    let mut out = Vec::new();
    for (si, section) in doc.sections.iter().enumerate() {
        let bodies = hwp5_bodies(section);
        for para in &section.paragraphs {
            collect_paragraph(si, para, &bodies, &mut out);
        }
    }
    out
}

fn collect_paragraph(
    si: usize,
    para: &Paragraph,
    bodies: &[(u32, Vec<String>)],
    out: &mut Vec<Memo>,
) {
    // Anchor text: characters between a memo FIELD_START and its FIELD_END (depth-aware).
    let mut open: Vec<(usize, String)> = Vec::new(); // (index into out, accumulated text)
    // A control is visited where its block sits in the text, so a memo inside a table that
    // precedes a direct anchor is reported first. A control the text never references keeps the
    // after-the-text position it had before.
    let mut visited = vec![false; para.controls.len()];
    for ch in &para.chars {
        match ch {
            HwpChar::ExtCtrl {
                code, ctrl_index, ..
            } => {
                let control = ctrl_index.and_then(|i| para.controls.get(i as usize));
                if *code == 3 {
                    collect_field_start(si, control, bodies, out, &mut open);
                }
                if let (Some(index), Some(control)) = (ctrl_index, control) {
                    if let Some(slot) = visited.get_mut(*index as usize) {
                        *slot = true;
                    }
                    collect_nested(si, control, bodies, out);
                }
            }
            HwpChar::InlineCtrl { code: 4, .. } => {
                if let Some((idx, text)) = open.pop() {
                    // A field nested in a memo's range (a hyperlink, say) is part of the
                    // memo's anchored text, so a closing child hands its text to its parent.
                    if let Some((_, parent)) = open.last_mut() {
                        parent.push_str(&text);
                    }
                    if idx != usize::MAX {
                        out[idx].anchor_text = text;
                    }
                }
            }
            other => {
                if let Some((_, text)) = open.last_mut() {
                    crate::text::char_text(other, text);
                }
            }
        }
    }
    for (i, control) in para.controls.iter().enumerate() {
        if !visited.get(i).copied().unwrap_or(true) {
            collect_nested(si, control, bodies, out);
        }
    }
}

/// Records the memo a field start opens, if the control behind it is one, and pushes its range
/// onto the anchor-text stack.
fn collect_field_start(
    si: usize,
    control: Option<&Control>,
    bodies: &[(u32, Vec<String>)],
    out: &mut Vec<Memo>,
    open: &mut Vec<(usize, String)>,
) {
    match control.and_then(|c| memo_command_of(c).map(|m| (c, m))) {
        Some((control, cmd)) => {
            let inline_body = match control {
                Control::Generic(g) => g
                    .paragraph_lists
                    .iter()
                    .flat_map(|l| l.paragraphs.iter().map(paragraph_text))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let body = if inline_body.is_empty() {
                bodies
                    .iter()
                    .find(|(n, _)| *n == cmd.number)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default()
            } else {
                inline_body
            };
            out.push(Memo {
                section: si,
                number: cmd.number,
                author: cmd.author,
                created_filetime: cmd.filetime,
                anchor_text: String::new(),
                body,
            });
            open.push((out.len() - 1, String::new()));
        }
        None => open.push((usize::MAX, String::new())),
    }
}

/// Collects the memos of every paragraph list `control` holds, in that control's own order.
fn collect_nested(
    si: usize,
    control: &Control,
    bodies: &[(u32, Vec<String>)],
    out: &mut Vec<Memo>,
) {
    for list in nested_paragraph_lists(control) {
        for p in list {
            collect_paragraph(si, p, bodies, out);
        }
    }
}

/// Every paragraph list nested in `control` that can anchor a memo: table cells, the caption
/// of a table, picture or generic object, and a generic control's paragraph lists. A memo
/// anchor's own lists are its body, not document content, and are left out. Each memo
/// traversal (reading here, id reservation in the HWPX writer) walks through this one helper,
/// so a container kind cannot be covered by one and missed by another.
pub fn nested_paragraph_lists(control: &Control) -> Vec<&[Paragraph]> {
    let mut lists: Vec<&[Paragraph]> = Vec::new();
    let caption = match control {
        Control::Table(t) => {
            lists.extend(t.cells.iter().map(|c| c.paragraphs.as_slice()));
            t.caption.as_ref()
        }
        Control::Picture(p) => p.caption.as_ref(),
        Control::Generic(g) => {
            if memo_command_of(control).is_none() {
                lists.extend(g.paragraph_lists.iter().map(|l| l.paragraphs.as_slice()));
            }
            g.caption.as_ref()
        }
        _ => None,
    };
    if let Some(c) = caption {
        lists.push(c.paragraphs.as_slice());
    }
    lists
}

/// The mutable twin of [`nested_paragraph_lists`], for the walks that rewrite what they find: an
/// op-created memo's body being drained out of its own lists, or the site of a memo to edit or
/// remove. Both build their list from the same container kinds, so a memo inside a table cell, a
/// caption or a nested list cannot be seen by one walk and missed by the other.
pub fn nested_paragraph_lists_mut(control: &mut Control) -> Vec<&mut [Paragraph]> {
    // Read the command before taking a mutable borrow of the control's fields.
    let is_memo = memo_command_of(control).is_some();
    let mut lists: Vec<&mut [Paragraph]> = Vec::new();
    match control {
        Control::Table(t) => {
            lists.extend(t.cells.iter_mut().map(|c| c.paragraphs.as_mut_slice()));
            if let Some(c) = t.caption.as_mut() {
                lists.push(c.paragraphs.as_mut_slice());
            }
        }
        Control::Picture(p) => {
            if let Some(c) = p.caption.as_mut() {
                lists.push(c.paragraphs.as_mut_slice());
            }
        }
        Control::Generic(g) => {
            if !is_memo {
                lists.extend(
                    g.paragraph_lists
                        .iter_mut()
                        .map(|l| l.paragraphs.as_mut_slice()),
                );
            }
            if let Some(c) = g.caption.as_mut() {
                lists.push(c.paragraphs.as_mut_slice());
            }
        }
        _ => {}
    }
    lists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BorderFillId, Cell, GenericControl, HwpUnit, Table};

    /// An HWP5 memo body paragraph keeps line breaks, tabs, hyphens and nb/fw spaces through
    /// the shared character mapping, the same text the HWPX path produces; extended controls
    /// and the paragraph end contribute nothing.
    #[test]
    fn hwp5_body_text_keeps_controls_through_the_shared_mapping() {
        let mut units: Vec<u16> = Vec::new();
        units.extend("가".encode_utf16());
        units.push(10); // line break
        units.extend("나".encode_utf16());
        units.extend([9, 0, 0, 0, 0, 0, 0, 9]); // tab (inline, 8 units)
        units.extend("다".encode_utf16());
        units.push(24); // hyphen
        units.push(30); // nb space
        units.push(31); // fw space
        units.extend([11, 0, 0, 0, 0, 0, 0, 11]); // extended control (table), no text
        units.extend("😀".encode_utf16()); // surrogate pair
        units.push(13); // paragraph end
        let data = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let para = OpaqueRecord {
            tag: 0x42,
            data: Vec::new(),
            children: vec![OpaqueRecord {
                tag: PARA_TEXT_TAG,
                data,
                children: Vec::new(),
            }],
        };
        let mut expected = String::new();
        for ch in [
            HwpChar::Text('가'),
            HwpChar::CharCtrl(10),
            HwpChar::Text('나'),
            HwpChar::InlineCtrl {
                code: 9,
                payload: Vec::new(),
            },
            HwpChar::Text('다'),
            HwpChar::CharCtrl(24),
            HwpChar::CharCtrl(30),
            HwpChar::CharCtrl(31),
            HwpChar::Text('😀'),
            HwpChar::CharCtrl(13),
        ] {
            crate::text::char_text(&ch, &mut expected);
        }
        assert_eq!(opaque_para_text(&para), expected);
        assert_eq!(expected, "가\n나\t다-  😀");
    }

    #[test]
    fn parses_a_memo_command() {
        let c = parse_memo_command("MEMO/65535/2/4085581696/31292974/someone/\\;;").unwrap();
        assert_eq!(c.number, 2);
        assert_eq!(c.author, "someone");
        assert_eq!(c.filetime, Some(31_292_974u64 << 32 | 4_085_581_696));
        assert!(parse_memo_command("HYPERLINK/x").is_none());
        assert!(parse_memo_command("MEMO/65535/x").is_none());
    }

    #[test]
    fn filetime_converts_to_utc() {
        // 2026-11-27T05:20:01Z, the measured second memo of a Hancom-saved file.
        let c = parse_memo_command("MEMO/65535/2/4085581696/31292974/a/b").unwrap();
        assert_eq!(
            filetime_to_rfc3339(c.filetime.unwrap()),
            "2026-11-27T05:20:01Z"
        );
        assert_eq!(
            filetime_to_rfc3339(116_444_736_000_000_000),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn build_memo_command_round_trips_through_parse() {
        // The first memo of fixtures/hwp5/memo3.hwp, byte for byte.
        let command = build_memo_command(1, Some(31_292_991u64 << 32 | 2_485_147_664u64), "yj.lee");
        assert_eq!(command, "MEMO/65535/1/2485147664/31292991/yj.lee/\\;;");
        let parsed = parse_memo_command(&command).unwrap();
        assert_eq!(parsed.number, 1);
        assert_eq!(parsed.author, "yj.lee");
        assert_eq!(
            parsed.filetime,
            Some(31_292_991u64 << 32 | 2_485_147_664u64)
        );
    }

    #[test]
    fn build_memo_command_writes_zero_time_as_absent() {
        let command = build_memo_command(3, None, "someone");
        assert_eq!(command, "MEMO/65535/3/0/0/someone/\\;;");
        assert_eq!(parse_memo_command(&command).unwrap().filetime, Some(0));
    }

    #[test]
    fn hwp5_field_begin_payload_matches_the_measured_block() {
        // fixtures/hwp5/memo3.hwp, both memos: `em%%` then eight zero bytes.
        assert_eq!(
            hwp5_field_begin_payload(),
            [0x65, 0x6d, 0x25, 0x25, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn hwp5_field_end_payload_flags_the_last_memo() {
        // fixtures/hwp5/memo3.hwp holds two memos: the first ends 01, the second 05.
        assert_eq!(
            hwp5_field_end_payload(1, false),
            [0x65, 0x6d, 0x25, 0x00, 0x01, 0xff, 0xff, 0x00, 1, 0, 0, 0]
        );
        assert_eq!(
            hwp5_field_end_payload(2, true),
            [0x65, 0x6d, 0x25, 0x00, 0x05, 0xff, 0xff, 0x00, 2, 0, 0, 0]
        );
    }

    #[test]
    fn filetime_now_reads_back_as_a_utc_instant() {
        let now = filetime_now();
        // Only the stable half is pinned: the value is past 1970 and formats as a 21st-century
        // instant, so the epoch arithmetic cannot silently drift to the wrong era.
        assert!(now > 116_444_736_000_000_000, "after 1970");
        assert!(filetime_to_rfc3339(now).starts_with("20"));
    }

    fn memo_anchor(number: u32) -> Control {
        let units: Vec<u16> = build_memo_command(number, None, "someone")
            .encode_utf16()
            .collect();
        let mut data = vec![0u8; 5];
        data.extend_from_slice(&(units.len() as u16).to_le_bytes());
        for unit in units {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        Control::Generic(GenericControl {
            ctrl_id: *b"%unk",
            data,
            paragraph_lists: Vec::new(),
            extras: Vec::new(),
            raw_children: Vec::new(),
            gso_shapes: Vec::new(),
            equation: None,
            column_def: None,
            caption: None,
            hwpx_raw_xml: None,
            chart: None,
            container_box: None,
        })
    }

    /// A paragraph holding one memo anchored to `text`, the anchor at control index 0.
    fn anchored_paragraph(text: &str, number: u32) -> Paragraph {
        let mut chars = vec![HwpChar::ExtCtrl {
            code: 3,
            ctrl_id: *b"%unk",
            payload: hwp5_field_begin_payload().to_vec(),
            ctrl_index: Some(0),
        }];
        chars.extend(text.chars().map(HwpChar::Text));
        chars.push(HwpChar::InlineCtrl {
            code: 4,
            payload: hwp5_field_end_payload(number, true).to_vec(),
        });
        Paragraph {
            chars,
            controls: vec![memo_anchor(number)],
            ..Paragraph::default()
        }
    }

    fn one_cell_table(para: Paragraph) -> Control {
        Control::Table(Table {
            common_data: Vec::new(),
            placement: None,
            attr: 0,
            rows: 1,
            cols: 1,
            cell_spacing: 0,
            inner_margins: [0; 4],
            row_cell_counts: vec![1],
            border_fill: BorderFillId(0),
            table_tail: Vec::new(),
            cells: vec![Cell {
                list_attr: 0,
                col: 0,
                row: 0,
                col_span: 1,
                row_span: 1,
                width: HwpUnit(100),
                height: HwpUnit(100),
                margins: [0; 4],
                border_fill: BorderFillId(0),
                header_tail: Vec::new(),
                paragraphs: vec![para],
            }],
            caption: None,
            extras: Vec::new(),
        })
    }

    /// A table's cell memo sits before a direct anchor in the same paragraph's text, so the
    /// numbers must come back in that order rather than every direct anchor first.
    #[test]
    fn memos_follow_the_anchor_position_inside_a_paragraph() {
        let para = Paragraph {
            chars: vec![
                HwpChar::ExtCtrl {
                    code: 11,
                    ctrl_id: *b"tbl ",
                    payload: Vec::new(),
                    ctrl_index: Some(0),
                },
                HwpChar::ExtCtrl {
                    code: 3,
                    ctrl_id: *b"%unk",
                    payload: hwp5_field_begin_payload().to_vec(),
                    ctrl_index: Some(1),
                },
                HwpChar::InlineCtrl {
                    code: 4,
                    payload: hwp5_field_end_payload(2, true).to_vec(),
                },
            ],
            controls: vec![one_cell_table(anchored_paragraph("셀", 1)), memo_anchor(2)],
            ..Paragraph::default()
        };
        let doc = Document {
            sections: vec![Section {
                paragraphs: vec![para],
                extras: Vec::new(),
            }],
            ..Document::default()
        };
        let numbers: Vec<u32> = memos(&doc).iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![1, 2]);
    }
}

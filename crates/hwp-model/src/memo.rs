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

/// Every memo in the document, in body order.
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
    for ch in &para.chars {
        match ch {
            HwpChar::ExtCtrl {
                code: 3,
                ctrl_index,
                ..
            } => {
                let control = ctrl_index.and_then(|i| para.controls.get(i as usize));
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
    for control in &para.controls {
        let lists: &[crate::control::ParagraphList] = match control {
            Control::Table(t) => {
                for cell in &t.cells {
                    for p in &cell.paragraphs {
                        collect_paragraph(si, p, bodies, out);
                    }
                }
                continue;
            }
            Control::Generic(g) if memo_command_of(control).is_none() => &g.paragraph_lists,
            _ => continue,
        };
        for list in lists {
            for p in &list.paragraphs {
                collect_paragraph(si, p, bodies, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

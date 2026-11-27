//! Read-only HWP 3.0 ingestion. Layout references are to HWP 3.0/HWPML, Part I.

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use hwp_model::{
    BinRef, BinStream, BorderFillId, Caption, CaptionDirection, CaptionSide, Cell, CharShape,
    CharShapeId, Control, Document, FaceName, GsoPlacement, HwpChar, HwpUnit, PageDef, ParaShape,
    ParaShapeId, Paragraph, Picture, Section, SectionDef, Style, StyleId, Table, ctrl_char,
};

use crate::{Hwp5Error, ReadResult, codec::ByteReader, error::Result};

pub const SIGNATURE: &[u8; 30] = b"HWP Document File V3.00 \x1a\x01\x02\x03\x04\x05";
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PARAGRAPHS: usize = 50_000;
const MAX_CHARACTERS: usize = 2_000_000;
const MAX_GRID: usize = 512;

pub fn read_document(path: &Path) -> Result<ReadResult> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    read_bytes(&bytes)
}

pub fn read_bytes(bytes: &[u8]) -> Result<ReadResult> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(byte_limit());
    }
    let mut src = ByteReader::new(bytes);
    if src.read_bytes(SIGNATURE.len())? != SIGNATURE {
        return Err(invalid("HWP 3.00 signature mismatch"));
    }

    // §§3.2–3.4. The document-info offsets are relative to the signature end.
    let info = src.read_bytes(128)?;
    if word(info, 96) != 0 {
        return Err(Hwp5Error::LegacyPasswordProtected);
    }
    let summary = src.read_bytes(1008)?;
    src.read_bytes(usize::from(word(info, 126)))?;
    let payload = src.take_rest();
    let decompressed;
    let body = if info[124] == 0 {
        payload
    } else {
        // §§1, 3: a single compression stream. Historical writer framing
        // cannot be verified locally. Recognize a gzip header explicitly.
        let decoder: Box<dyn Read> = if payload.starts_with(&[0x1f, 0x8b]) {
            Box::new(flate2::read::GzDecoder::new(payload))
        } else {
            Box::new(flate2::read::DeflateDecoder::new(payload))
        };
        let mut data = Vec::new();
        decoder
            .take(MAX_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|source| Hwp5Error::Decompress {
                stream: "HWP 3.0 body".into(),
                source,
            })?;
        if data.len() as u64 > MAX_BYTES {
            return Err(byte_limit());
        }
        decompressed = data;
        &decompressed
    };
    let mut parser = Parser::new(body);
    parser.tables()?;
    let mut paragraphs = parser.paragraphs(0)?;
    if parser.document.header.char_shapes.is_empty() {
        parser.document.header.char_shapes.push(CharShape {
            base_size: 1000,
            ratios: [100; 7],
            rel_sizes: [100; 7],
            shade_color: u32::MAX,
            ..Default::default()
        });
    }
    if parser.document.header.para_shapes.is_empty() {
        parser
            .document
            .header
            .para_shapes
            .push(ParaShape::default());
    }
    let images = parser.images()?;
    bind_images(&mut paragraphs, &images)?;
    let mut document = parser.document;
    document.meta.source_format = "hwp3".into();
    document.meta.source_version = "3.00".into();
    document.metadata.title = summary_field(summary, 0);
    document.metadata.subject = summary_field(summary, 1);
    document.metadata.author = summary_field(summary, 2);
    document.metadata.keywords = summary_field(summary, 4);
    document.header.properties.section_count = 1;
    document.header.properties.start_numbers =
        [word(info, 98).max(1), word(info, 100).max(1), 1, 1, 1, 1];
    let mut page = PageDef {
        width: unit(word(info, 8)),
        height: unit(word(info, 6)),
        margin_left: unit(word(info, 14)),
        margin_right: unit(word(info, 16)),
        margin_top: unit(word(info, 10)),
        margin_bottom: unit(word(info, 12)),
        margin_header: unit(word(info, 18)),
        margin_footer: unit(word(info, 20)),
        gutter: unit(word(info, 22)),
        attr: u32::from(info[5] != 0),
    };
    if page.width.0 == 0 || page.height.0 == 0 {
        page.width = HwpUnit(59_528);
        page.height = HwpUnit(84_189);
    }
    if paragraphs.is_empty() {
        paragraphs.push(empty_paragraph());
    }
    let first = &mut paragraphs[0];
    let section_def = Control::SectionDef(SectionDef {
        data: Vec::new(),
        page: Some(page),
        extras: Vec::new(),
        secpr_raw_children: Vec::new(),
        footnote_shape_raw: None,
        endnote_shape_raw: None,
        page_border_fills_raw: Vec::new(),
    });
    let marker = control_marker(&section_def, ctrl_char::SECTION_COLUMN_DEF, 0);
    for ch in &mut first.chars {
        if let HwpChar::ExtCtrl {
            ctrl_index: Some(index),
            ..
        } = ch
        {
            *index += 1;
        }
    }
    // The marker shares the first run's style, so only later runs move.
    for (position, _) in first.char_shape_runs.iter_mut().skip(1) {
        *position += marker.wchar_width();
    }
    first.chars.insert(0, marker);
    first.controls.insert(0, section_def);
    document.sections.push(Section {
        paragraphs,
        extras: Vec::new(),
    });
    Ok(ReadResult {
        document,
        warnings: if parser.omissions == 0 {
            Vec::new()
        } else {
            vec![format!(
                "HWP 3.0에서 지원하지 않는 문자 또는 개체 {}개를 생략하거나 대체했습니다",
                parser.omissions
            )]
        },
        unwrapped_distribution: false,
    })
}

fn invalid(message: &str) -> Hwp5Error {
    Hwp5Error::MalformedRecord(format!("HWP 3.0: {message}"))
}

fn byte_limit() -> Hwp5Error {
    Hwp5Error::ResourceLimitExceeded {
        resource: "HWP 3.0".into(),
        limit: MAX_BYTES,
    }
}

fn structure_limit(resource: &str, limit: usize) -> Hwp5Error {
    Hwp5Error::StructureLimitExceeded {
        resource: format!("HWP 3.0 {resource}"),
        limit,
    }
}

// Callers supply fixed-size field blocks read with ByteReader.
fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn unit(legacy: u16) -> HwpUnit {
    HwpUnit(i32::from(legacy) * 4)
}

fn summary_field(summary: &[u8], index: usize) -> Option<String> {
    let text: String = summary[index * 112..(index + 1) * 112]
        .chunks_exact(2)
        .map(|bytes| word(bytes, 0))
        .take_while(|&code| code != 0)
        .map(|code| decode_hchar(code).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    (!text.trim().is_empty()).then_some(text)
}

/// Modern Johab syllable slots, indexed by the corresponding Unicode syllable component.
const VOWELS: [u16; 21] = [
    3, 4, 5, 6, 7, 10, 11, 12, 13, 14, 15, 18, 19, 20, 21, 22, 23, 26, 27, 28, 29,
];
const FINALS: [u16; 28] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29,
];

fn decode_hchar(code: u16) -> Option<char> {
    if (32..=126).contains(&code) {
        return char::from_u32(u32::from(code));
    }
    if code & 0x8000 != 0 {
        let initial_slot = (code >> 10) & 31;
        let vowel = VOWELS.iter().position(|&slot| slot == ((code >> 5) & 31));
        if code & 31 == 1 && initial_slot == 1 {
            return vowel.and_then(|i| char::from_u32(0x314f + i as u32));
        }
        let initial = initial_slot.checked_sub(2)?;
        if initial >= 19 {
            return None;
        }
        if ((code >> 5) & 31) == 2 && code & 31 == 1 {
            return "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ"
                .chars()
                .nth(usize::from(initial));
        }
        let final_ = FINALS.iter().position(|&slot| slot == (code & 31))?;
        return char::from_u32(
            0xac00 + u32::from(initial) * 588 + vowel? as u32 * 28 + final_ as u32,
        );
    }
    // The supported HWP Hanja interval indexes the KS X 1001 ideograph rows.
    if let Some(index) = code.checked_sub(0x4000).filter(|&index| index < 52 * 94) {
        let bytes = [0xca + (index / 94) as u8, 0xa1 + (index % 94) as u8];
        return encoding_rs::EUC_KR
            .decode_without_bom_handling_and_without_replacement(&bytes)?
            .chars()
            .next();
    }
    None
}

fn kchar(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut bytes = bytes.iter().copied().take_while(|&b| b != 0);
    while let Some(first) = bytes.next() {
        let code = if first < 128 {
            u16::from(first)
        } else {
            let Some(second) = bytes.next() else {
                text.push(char::REPLACEMENT_CHARACTER);
                break;
            };
            u16::from_be_bytes([first, second])
        };
        text.push(decode_hchar(code).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    text
}

fn empty_paragraph() -> Paragraph {
    Paragraph {
        char_shape_runs: vec![(0, CharShapeId(0))],
        ..Default::default()
    }
}

fn control_marker(control: &Control, code: u16, index: u32) -> HwpChar {
    let id = control.ctrl_id();
    let mut payload = vec![0; 12];
    payload[..4].copy_from_slice(&[id[3], id[2], id[1], id[0]]);
    HwpChar::ExtCtrl {
        code,
        ctrl_id: id,
        payload,
        ctrl_index: Some(index),
    }
}

fn add_control(paragraph: &mut Paragraph, control: Control, code: u16) {
    paragraph.chars.push(control_marker(
        &control,
        code,
        paragraph.controls.len() as u32,
    ));
    paragraph.controls.push(control);
}

struct Parser<'a> {
    input: ByteReader<'a>,
    document: Document,
    character_shapes: BTreeMap<[u8; 31], CharShapeId>,
    paragraph_shapes: BTreeMap<[u8; 187], ParaShapeId>,
    remaining_paragraphs: usize,
    remaining_characters: usize,
    omissions: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            input: ByteReader::new(bytes),
            document: Document::default(),
            character_shapes: BTreeMap::new(),
            paragraph_shapes: BTreeMap::new(),
            remaining_paragraphs: MAX_PARAGRAPHS,
            remaining_characters: MAX_CHARACTERS,
            omissions: 0,
        }
    }

    fn tables(&mut self) -> Result<()> {
        // §§3.5–3.6.
        for fonts in &mut self.document.header.fonts {
            let count = usize::from(self.input.read_u16()?);
            if count > 256 {
                return Err(structure_limit("font table", 256));
            }
            for _ in 0..count {
                fonts.push(FaceName {
                    name: kchar(self.input.read_bytes(40)?),
                    ..Default::default()
                });
            }
            if fonts.is_empty() {
                fonts.push(FaceName {
                    name: "바탕".into(),
                    ..Default::default()
                });
            }
        }
        let count = self.input.read_u16()?;
        if count > 256 {
            return Err(structure_limit("style table", 256));
        }
        for _ in 0..count {
            let name = kchar(self.input.read_bytes(20)?);
            let char_shape = self.read_char_shape()?;
            let para_shape = self.read_para_shape()?;
            self.document.header.styles.push(Style {
                english_name: name.clone(),
                name,
                char_shape,
                para_shape,
                ..Default::default()
            });
        }
        if count == 0 {
            self.document.header.styles.push(Style {
                name: "바탕글".into(),
                ..Default::default()
            });
        }
        Ok(())
    }

    fn read_char_shape(&mut self) -> Result<CharShapeId> {
        let bytes = self.input.read_bytes(31)?;
        if let Some(&id) = self.character_shapes.get(bytes) {
            return Ok(id);
        }
        let id = CharShapeId(
            u16::try_from(self.character_shapes.len())
                .map_err(|_| structure_limit("character shapes", 65_536))?,
        );
        const COLORS: [u32; 8] = [
            0, 0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
        ];
        let shape = CharShape {
            base_size: match word(bytes, 0) {
                0 => 1000,
                size => unit(size).0,
            },
            face_ids: std::array::from_fn(|slot| {
                let index = usize::from(bytes[2 + slot]);
                if index < self.document.header.fonts[slot].len() {
                    index as u16
                } else {
                    0
                }
            }),
            ratios: std::array::from_fn(|slot| {
                if bytes[9 + slot] == 0 {
                    100
                } else {
                    bytes[9 + slot].clamp(50, 200)
                }
            }),
            spacings: std::array::from_fn(|slot| (bytes[16 + slot] as i8).clamp(-50, 50)),
            rel_sizes: [100; 7],
            attr: u32::from(bytes[26] & 7),
            text_color: COLORS.get(usize::from(bytes[24])).copied().unwrap_or(0),
            shade_color: u32::MAX,
            ..Default::default()
        };
        self.document.header.char_shapes.push(shape);
        self.character_shapes
            .insert(bytes.try_into().expect("fixed length"), id);
        Ok(id)
    }

    fn read_para_shape(&mut self) -> Result<ParaShapeId> {
        let bytes = self.input.read_bytes(187)?;
        if let Some(&id) = self.paragraph_shapes.get(bytes) {
            return Ok(id);
        }
        let id = ParaShapeId(
            u16::try_from(self.paragraph_shapes.len())
                .map_err(|_| structure_limit("paragraph shapes", 65_536))?,
        );
        let spacing = word(bytes, 6);
        let absolute = spacing & 0x8000 != 0;
        // The shared paragraph shape stores lengths at twice HWPUNIT.
        let spacing = if absolute {
            unit(spacing & 0x7fff).0 * 2
        } else {
            i32::from(spacing)
        };
        self.document.header.para_shapes.push(ParaShape {
            attr1: u32::from(absolute),
            margin_left: unit(word(bytes, 0)).0 * 2,
            margin_right: unit(word(bytes, 2)).0 * 2,
            indent: i32::from(word(bytes, 4) as i16) * 8,
            spacing_bottom: unit(word(bytes, 8)).0 * 2,
            line_spacing: spacing,
            line_spacing_old: spacing,
            line_spacing_type: u8::from(absolute),
            ..Default::default()
        });
        self.paragraph_shapes
            .insert(bytes.try_into().expect("fixed length"), id);
        Ok(id)
    }

    fn count_paragraph(&mut self) -> Result<()> {
        self.remaining_paragraphs = self
            .remaining_paragraphs
            .checked_sub(1)
            .ok_or_else(|| structure_limit("paragraphs", MAX_PARAGRAPHS))?;
        Ok(())
    }

    fn paragraphs(&mut self, depth: usize) -> Result<Vec<Paragraph>> {
        if depth >= 16 {
            return Err(structure_limit("paragraph nesting", 16));
        }
        let mut paragraphs = Vec::new();
        let mut previous = None;
        loop {
            // §§4.1–4.4. Empty terminators retain the fixed paragraph header.
            let head = self.input.read_bytes(12)?;
            let count = usize::from(word(head, 1));
            let lines = usize::from(word(head, 3));
            if count == 0 {
                self.input.read_bytes(31)?;
                break;
            }
            self.count_paragraph()?;
            self.remaining_characters = self
                .remaining_characters
                .checked_sub(count)
                .ok_or_else(|| structure_limit("characters", MAX_CHARACTERS))?;
            let representative = self.read_char_shape()?;
            let shape = if head[0] == 0 {
                self.read_para_shape()?
            } else {
                previous.ok_or_else(|| invalid("paragraph reuses a missing shape"))?
            };
            previous = Some(shape);
            self.input.read_bytes(lines * 14)?;
            let mut shapes = Vec::new();
            if head[5] != 0 {
                let mut last = representative;
                for _ in 0..count {
                    if self.input.read_u8()? != 1 {
                        last = self.read_char_shape()?;
                    }
                    shapes.push(last);
                }
            }
            let style = if usize::from(head[11]) < self.document.header.styles.len() {
                head[11]
            } else {
                0
            };
            let mut para = Paragraph {
                para_shape: shape,
                style: StyleId(u16::from(style)),
                ..Default::default()
            };
            para.header.break_type = ((head[6] & 2) << 1) | ((head[6] & 1) << 3);
            let mut position = 0u32;
            let mut legacy = 0;
            while legacy < count {
                let id = shapes.get(legacy).copied().unwrap_or(representative);
                let code = self.input.read_u16()?;
                if code == 13 {
                    if legacy + 1 != count {
                        return Err(invalid("paragraph terminator precedes the declared length"));
                    }
                    break;
                }
                let before = para.chars.len();
                let width = self.character(&mut para, code, depth)?;
                legacy += width;
                if legacy >= count {
                    return Err(invalid(
                        "paragraph exceeds declared length or has no terminator",
                    ));
                }
                if before != para.chars.len()
                    && para
                        .char_shape_runs
                        .last()
                        .is_none_or(|&(_, previous)| previous != id)
                {
                    para.char_shape_runs.push((position, id));
                }
                position += para.chars[before..]
                    .iter()
                    .map(HwpChar::wchar_width)
                    .sum::<u32>();
            }
            if para.char_shape_runs.is_empty() {
                para.char_shape_runs.push((0, representative));
            }
            paragraphs.push(para);
        }
        Ok(paragraphs)
    }

    fn character(&mut self, para: &mut Paragraph, code: u16, depth: usize) -> Result<usize> {
        match code {
            32.. => {
                let ch = decode_hchar(code).unwrap_or_else(|| {
                    self.omissions += 1;
                    char::REPLACEMENT_CHARACTER
                });
                para.chars.push(HwpChar::Text(ch));
                Ok(1)
            }
            9 => {
                self.marker(code, 4)?;
                para.chars.push(HwpChar::InlineCtrl {
                    code: ctrl_char::TAB,
                    payload: vec![0; 12],
                });
                Ok(4)
            }
            10 | 11 => {
                self.marker(code, 4)?;
                let object = if code == 10 {
                    self.table(depth + 1)?
                } else {
                    self.picture(depth + 1)?
                };
                if let Some(object) = object {
                    add_control(para, object, ctrl_char::OBJECT);
                }
                Ok(4)
            }
            ctrl_char::HYPHEN | ctrl_char::NB_SPACE | ctrl_char::FW_SPACE => {
                let width = if code == ctrl_char::HYPHEN { 3 } else { 2 };
                self.marker(code, width)?;
                para.chars.push(HwpChar::CharCtrl(code));
                Ok(width)
            }
            15..=17 => {
                self.marker(code, 4)?;
                self.input.read_bytes(match code {
                    15 => 8,
                    16 => 10,
                    _ => 14,
                })?;
                // These containers have a nested list even though their placement is outside T2.
                self.paragraphs(depth + 1)?;
                self.omissions += 1;
                Ok(4)
            }
            _ => Err(invalid(&format!("unsupported special character {code}"))),
        }
    }

    fn marker(&mut self, code: u16, width: usize) -> Result<()> {
        self.input.read_bytes((width - 2) * 2)?;
        if self.input.read_u16()? != code {
            return Err(invalid("wrong closing special-character marker"));
        }
        Ok(())
    }

    fn table(&mut self, depth: usize) -> Result<Option<Control>> {
        // §10.6.
        let info = self.input.read_bytes(84)?;
        if word(info, 16) != 10 {
            return Err(invalid("table header marker mismatch"));
        }
        let count = usize::from(word(info, 80));
        if count == 0 || count > 4096 {
            return Err(structure_limit("table cells", 4096));
        }
        let entries = self.input.read_bytes(count * 27)?;
        let mut content = Vec::with_capacity(count);
        for _ in 0..count {
            let mut paragraphs = self.paragraphs(depth)?;
            if paragraphs.is_empty() {
                self.count_paragraph()?;
                paragraphs.push(empty_paragraph());
            }
            content.push(paragraphs);
        }
        let caption = self.paragraphs(depth)?;
        if word(info, 78) != 0 {
            self.omissions += 1;
            return Ok(None);
        }
        let cells = grid_cells(entries, content)?;
        let mut rows = 0u16;
        let mut cols = 0u16;
        for cell in &cells {
            rows = rows.max(cell.row + cell.row_span);
            cols = cols.max(cell.col + cell.col_span);
        }
        let mut row_cell_counts = vec![0; usize::from(rows)];
        for cell in &cells {
            row_cell_counts[usize::from(cell.row)] += 1;
        }
        Ok(Some(Control::Table(Table {
            rows,
            cols,
            cells,
            row_cell_counts,
            placement: Some(GsoPlacement {
                treat_as_char: true,
                flow_with_text: true,
                vert_rel_to: 2,
                horz_rel_to: 3,
                width: unit(word(info, 42)).0,
                height: unit(word(info, 44)).0,
                ..Default::default()
            }),
            caption: make_caption(caption),
            common_data: Vec::new(),
            attr: 0,
            cell_spacing: 0,
            inner_margins: [0; 4],
            border_fill: BorderFillId(0),
            table_tail: Vec::new(),
            extras: Vec::new(),
        })))
    }

    fn picture(&mut self, depth: usize) -> Result<Option<Control>> {
        // §§8.1, 10.7. Resolve embedded names through the document's own tag list.
        let info = self.input.read_bytes(348)?;
        if word(info, 16) != 11 {
            return Err(invalid("picture header marker mismatch"));
        }
        self.input.read_bytes(dword(info, 0) as usize)?;
        let caption = self.paragraphs(depth)?;
        if info[74] != 2 {
            self.omissions += 1;
            return Ok(None);
        }
        let name = kchar(&info[83..339]);
        if name.is_empty() {
            return Err(invalid("embedded picture has no name"));
        }
        Ok(Some(Control::Picture(Picture {
            width: unit(word(info, 42)),
            height: unit(word(info, 44)),
            treat_as_char: true,
            caption: make_caption(caption),
            bin_ref: BinRef::ItemRef(name),
            common_data: Vec::new(),
            z_order: 0,
            vert_offset: 0,
            horz_offset: 0,
            description: None,
            crop: None,
            flip: 0,
            rotation: None,
            brightness: info[339] as i8,
            contrast: info[340] as i8,
            effect_flags: 0,
            effects_raw: Vec::new(),
            extras: Vec::new(),
        })))
    }

    fn images(&mut self) -> Result<BTreeMap<String, Option<String>>> {
        // §3.8: size-delimited additions; §8.1: embedded-image entries.
        let mut images = BTreeMap::new();
        while self.input.remaining() > 0 {
            let tag = self.input.read_u32()?;
            let size = self.input.read_u32()? as usize;
            if (tag == 0 && size == 0) || tag == 0x8000_0000 {
                break;
            }
            let content = self.input.read_bytes(size)?;
            if tag != 1 {
                continue;
            }
            if images.len() >= 4096 {
                return Err(structure_limit("embedded images", 4096));
            }
            if content.len() < 32 {
                return Err(invalid("truncated embedded-image header"));
            }
            let key = kchar(&content[..16]);
            let data = &content[32..];
            let suffix = match data {
                [137, b'P', b'N', b'G', 13, 10, 26, 10, ..] => Some("png"),
                [0xff, 0xd8, 0xff, ..] => Some("jpg"),
                [b'B', b'M', ..] => Some("bmp"),
                [b'G', b'I', b'F', b'8', ..] => Some("gif"),
                _ => None,
            };
            let name =
                suffix.map(|suffix| format!("BinData/legacy-image-{}.{suffix}", images.len() + 1));
            if key.is_empty() || images.insert(key, name.clone()).is_some() {
                return Err(invalid("duplicate or empty embedded-image name"));
            }
            if let Some(name) = name {
                self.document.bin_streams.push(BinStream {
                    name,
                    data: data.to_vec(),
                });
            } else {
                self.omissions += 1;
            }
        }
        Ok(images)
    }
}

fn make_caption(paragraphs: Vec<Paragraph>) -> Option<Caption> {
    (!paragraphs.is_empty()).then_some(Caption {
        side: CaptionSide::Bottom,
        direction: CaptionDirection::Horizontal,
        gap: 0,
        width: None,
        last_width: 0,
        paragraphs,
    })
}

fn grid_cells(entries: &[u8], content: Vec<Vec<Paragraph>>) -> Result<Vec<Cell>> {
    // §10.6 stores rectangles. The boundaries of those rectangles determine
    // the grid and spans. Real-file tolerance for slightly shifted edges is unverified.
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for entry in entries.chunks_exact(27) {
        let (x, y, width, height) = rect(entry);
        if width == 0 || height == 0 {
            return Err(invalid("table cell has zero size"));
        }
        xs.extend([x, x + width]);
        ys.extend([y, y + height]);
    }
    for values in [&mut xs, &mut ys] {
        values.sort_unstable();
        values.dedup();
        if values.len() > MAX_GRID + 1 {
            return Err(structure_limit("grid boundaries", MAX_GRID + 1));
        }
    }
    let col_count = xs.len() - 1;
    let mut occupied = vec![false; col_count * (ys.len() - 1)];
    let mut cells = Vec::new();
    for (entry, paragraphs) in entries.chunks_exact(27).zip(content) {
        let (x, y, width, height) = rect(entry);
        let col = xs.binary_search(&x).expect("recorded boundary");
        let row = ys.binary_search(&y).expect("recorded boundary");
        let end_col = xs.binary_search(&(x + width)).expect("recorded boundary");
        let end_row = ys.binary_search(&(y + height)).expect("recorded boundary");
        for row in row..end_row {
            for col in col..end_col {
                if std::mem::replace(&mut occupied[row * col_count + col], true) {
                    return Err(invalid("overlapping table cell rectangles"));
                }
            }
        }
        cells.push(Cell {
            list_attr: 0,
            col: col as u16,
            row: row as u16,
            col_span: (end_col - col) as u16,
            row_span: (end_row - row) as u16,
            width: HwpUnit(width as i32 * 4),
            height: HwpUnit(height as i32 * 4),
            margins: [0; 4],
            border_fill: BorderFillId(0),
            header_tail: Vec::new(),
            paragraphs,
        });
    }
    Ok(cells)
}

fn rect(entry: &[u8]) -> (u32, u32, u32, u32) {
    (
        u32::from(word(entry, 4)),
        u32::from(word(entry, 6)),
        u32::from(word(entry, 8)),
        u32::from(word(entry, 10)),
    )
}

fn bind_images(
    paragraphs: &mut [Paragraph],
    images: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    for paragraph in paragraphs {
        for control in &mut paragraph.controls {
            let caption = match control {
                Control::Picture(picture) => {
                    if let BinRef::ItemRef(key) = &mut picture.bin_ref {
                        picture.bin_ref = match images.get(key) {
                            Some(Some(name)) => BinRef::ItemRef(name.clone()),
                            // Keep the picture anchor and caption. Writers report the
                            // unresolved payload as a preservation loss.
                            Some(None) => BinRef::Id(hwp_model::BinDataId(0)),
                            None => return Err(invalid("embedded-picture payload not found")),
                        };
                    }
                    &mut picture.caption
                }
                Control::Table(table) => {
                    for cell in &mut table.cells {
                        bind_images(&mut cell.paragraphs, images)?;
                    }
                    &mut table.caption
                }
                _ => continue,
            };
            if let Some(caption) = caption {
                bind_images(&mut caption.paragraphs, images)?;
            }
        }
    }
    Ok(())
}

//! Construct synthetic HWP 3.00 inputs using the Part I layouts.

pub fn word_at(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn prefix(compressed: bool) -> Vec<u8> {
    let mut bytes = hwp5::hwp3::SIGNATURE.to_vec();
    let mut info = [0; 128];
    word_at(&mut info, 6, 21_047);
    word_at(&mut info, 8, 14_882);
    for offset in [10, 12, 14, 16] {
        word_at(&mut info, offset, 1000);
    }
    info[124] = u8::from(compressed);
    bytes.extend(info);
    let mut summary = [0; 1008];
    word_at(&mut summary, 0, 0xd065);
    word_at(&mut summary, 2, 0x8b69);
    bytes.extend(summary);
    bytes
}

pub fn shapes(body: &mut Vec<u8>) {
    for _ in 0..7 {
        body.extend(0u16.to_le_bytes());
    }
    body.extend(0u16.to_le_bytes());
}

fn paragraph_header(bytes: &mut Vec<u8>, characters: u16) {
    let mut h = [0; 43];
    word_at(&mut h, 1, characters);
    word_at(&mut h, 12, 250);
    h[21..28].fill(100);
    bytes.extend(h);
    if characters > 0 {
        let mut para_shape = [0; 187];
        word_at(&mut para_shape, 6, 160);
        bytes.extend(para_shape);
    }
}

fn words(bytes: &mut Vec<u8>, values: &[u16]) {
    for &value in values {
        bytes.extend(value.to_le_bytes());
    }
}

pub fn text_list(bytes: &mut Vec<u8>, values: &[u16]) {
    paragraph_header(bytes, (values.len() + 1) as u16);
    words(bytes, values);
    words(bytes, &[13]);
    paragraph_header(bytes, 0);
}

// A valid one-pixel PNG.
pub const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 16, 177, 169, 248, 15, 0, 2,
    248, 1, 200, 244, 237, 32, 109, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

pub fn rich_document() -> Vec<u8> {
    rich_document_with_cell_text(&[u16::from(b'A')])
}

pub fn rich_document_with_cell_text(first_cell: &[u16]) -> Vec<u8> {
    let mut bytes = prefix(false);
    shapes(&mut bytes);
    paragraph_header(&mut bytes, 12);
    words(&mut bytes, &[0xd065, 0x8b69, 0x4000, 10, 0, 0, 10]);
    let mut table = [0; 84];
    word_at(&mut table, 16, 10);
    word_at(&mut table, 42, 2400);
    word_at(&mut table, 44, 1200);
    word_at(&mut table, 80, 3);
    bytes.extend(table);
    for (x, y, width) in [(0, 0, 2400), (0, 600, 1200), (1200, 600, 1200)] {
        let mut cell = [0; 27];
        word_at(&mut cell, 4, x);
        word_at(&mut cell, 6, y);
        word_at(&mut cell, 8, width);
        word_at(&mut cell, 10, 600);
        bytes.extend(cell);
    }
    if first_cell.is_empty() {
        paragraph_header(&mut bytes, 0);
    } else {
        text_list(&mut bytes, first_cell);
    }
    for ch in ['B', 'C'] {
        text_list(&mut bytes, &[ch as u16]);
    }
    paragraph_header(&mut bytes, 0); // No table caption.

    words(&mut bytes, &[11, 0, 0, 11]);
    let mut picture = [0; 348];
    word_at(&mut picture, 16, 11);
    word_at(&mut picture, 42, 600);
    word_at(&mut picture, 44, 450);
    picture[74] = 2;
    picture[83..88].copy_from_slice(b"image");
    bytes.extend(picture);
    paragraph_header(&mut bytes, 0);
    words(&mut bytes, &[13]);
    paragraph_header(&mut bytes, 0);

    bytes.extend(1u32.to_le_bytes());
    bytes.extend(((32 + PNG.len()) as u32).to_le_bytes());
    let mut names = [0; 32];
    names[..5].copy_from_slice(b"image");
    names[16..19].copy_from_slice(b"PNG");
    bytes.extend(names);
    bytes.extend(PNG);
    bytes.extend([0; 8]);
    bytes
}

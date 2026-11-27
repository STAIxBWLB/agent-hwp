use hwp_render::{
    display::PageList, fonts::FontStore, issues::RenderIssueAccumulator, wmf::draw_wmf,
};

pub fn exercise(data: &[u8]) {
    let mut store = FontStore::new_isolated();
    let mut draw = |bytes: &[u8]| {
        let mut page = PageList {
            width_pt: 100.0,
            height_pt: 100.0,
            items: Vec::new(),
        };
        let mut warnings = RenderIssueAccumulator::new();
        let _ = draw_wmf(
            bytes,
            0.0,
            0.0,
            100.0,
            100.0,
            None,
            &mut page,
            &mut store,
            &mut warnings,
        );
    };
    draw(data);

    // Embed the slice in a standard WMF DIBStretchBlt record so the DIB
    // decoder is exercised through the public drawing entry point.
    let dib_record_words = 13 + data.len().div_ceil(2) as u32;
    let total_words = 9 + 5 + dib_record_words + 3;
    let mut wmf = Vec::with_capacity(total_words as usize * 2);
    wmf.extend_from_slice(&[1, 0, 9, 0, 0, 3]);
    wmf.extend_from_slice(&total_words.to_le_bytes());
    wmf.extend_from_slice(&[0, 0]);
    wmf.extend_from_slice(&dib_record_words.to_le_bytes());
    wmf.extend_from_slice(&[0, 0]);
    // SetWindowExt, then DIBStretchBlt with SRCCOPY and a nonzero destination.
    wmf.extend_from_slice(&[5, 0, 0, 0, 12, 2, 100, 0, 100, 0]);
    wmf.extend_from_slice(&dib_record_words.to_le_bytes());
    wmf.extend_from_slice(&[65, 11]);
    wmf.extend_from_slice(&0x00cc_0020u32.to_le_bytes());
    for param in [0i16, 0, 0, 0, 100, 100, 0, 0] {
        wmf.extend_from_slice(&param.to_le_bytes());
    }
    wmf.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        wmf.push(0);
    }
    wmf.extend_from_slice(&[3, 0, 0, 0, 0, 0]);
    draw(&wmf);
}

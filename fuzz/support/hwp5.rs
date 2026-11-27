#[path = "input_file.rs"]
mod input_file;

pub fn exercise(data: &[u8]) {
    use hwp5::record::scan::{ScanMode, scan_stream};

    let _ = scan_stream(data, ScanMode::Strict);
    let _ = scan_stream(data, ScanMode::Tolerant);
    let _ = hwp5::hwp3::read_bytes(data);
    input_file::with_input(data, |path| {
        let _ = hwp5::read_document(path);
        let _ = hwp5::read_document_with_options(
            path,
            &hwp5::ReadOptions {
                password: Some("synthetic"),
            },
        );
    });
}

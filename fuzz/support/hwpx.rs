#[path = "input_file.rs"]
mod input_file;

pub fn exercise(data: &[u8]) {
    if let Ok(xml) = std::str::from_utf8(data) {
        let _ = hwpx::read::section::parse_section(xml);
        let _ = hwpx::read::header::parse_header(xml);
    }
    let _ = hwpx::read_hml(data);
    input_file::with_input(data, |path| {
        let _ = hwpx::read_document(path);
        let _ = hwpx::read_document_with_options(
            path,
            &hwpx::ReadOptions {
                password: Some("synthetic"),
            },
        );
    });
}

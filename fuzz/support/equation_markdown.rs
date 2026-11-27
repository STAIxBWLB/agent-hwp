pub fn exercise(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut fonts = hwp_render::fonts::FontStore::new_isolated();
    let document = hwp_model::Document::default();
    let _ = hwp_render::equation::typeset(&mut fonts, &document, text, 10.0);
    // No absolute path can fall under this relative root. Keep filesystem
    // image reads out of replay while still exercising embedded images.
    let roots = [std::path::PathBuf::from("fuzz-disabled-image-root")];
    let markdown = hwp_convert::from_markdown::MarkdownImportOptions {
        roots: &roots,
        ..Default::default()
    };
    let _ = hwp_convert::from_markdown::from_markdown_report(text, &markdown);
    // HtmlImportOptions keeps a crate-private field, so the struct-update form above is not
    // available for it.
    let mut html = hwp_convert::from_html::HtmlImportOptions::default();
    html.roots = &roots;
    let _ = hwp_convert::from_html::from_html_with(text, &html);
}

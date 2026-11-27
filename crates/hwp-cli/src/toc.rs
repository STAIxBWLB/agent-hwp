//! Two-pass TOC page backfill (#620): the layout pass lives here, in `hwp-cli`, because
//! `hwp-convert` (which builds the TOC) and `hwp-render` (which paginates) must not depend on
//! each other (invariant 1).
//!
//! [`backfill`] lays the document out with the same pass behind `render --layout-json`, reads
//! the 0-based page each TOC heading paragraph lands on from the segment map, converts it to the
//! displayed body page (the body restarts at page 1 after the TOC's page break), and writes the
//! numbers into the TOC. Writing digits can change the TOC's own line breaks, so it lays out
//! again until the page numbers stop changing (a fixed point), bounded by [`MAX_PASSES`].
//!
//! Each pass measures the document as the output is published: the caller's `published`
//! closure writes the IR with the very writer that later publishes it and reads it back, so the
//! layout runs on exactly what a reader of the output file (and `render --layout-json` on it)
//! sees, and the caller publishes the measured document itself, never a re-read copy. An HWP5
//! writer synthesizes line-segment caches, so the in-memory IR alone can paginate differently.

use std::path::PathBuf;

use hwp_convert::report_blocks::{TocPlan, fill_toc};
use hwp_model::Document;

/// Layout passes before the backfill gives up on reaching a fixed point.
pub const MAX_PASSES: usize = 4;

/// What [`backfill`] did, for the caller's warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocBackfill {
    /// The displayed body page of each TOC entry, in TOC order.
    pub pages: Vec<u32>,
    /// Layout passes run, the last one confirming the fixed point.
    pub passes: usize,
    /// Font requests the layout satisfied by substitution or not at all. Pagination under
    /// substituted metrics can differ from Hangul's, so the caller warns when this is non-zero.
    pub font_fallbacks: u64,
}

/// Where the layout pass takes its fonts from: directories (the CLI, like `render`) or exact
/// font files with no system fallback (hermetic MCP compose, like the isolated HWP writer).
#[derive(Debug, Clone, Copy)]
pub enum TocFonts<'a> {
    Dirs(&'a [PathBuf]),
    Isolated(&'a [PathBuf]),
}

/// How the caller names the TOC switch in messages: `--toc` for `new`, `report_blocks.toc` for
/// `compose`, so a failure tells each user which option to turn off.
#[derive(Debug, Clone, Copy)]
pub struct TocLayout<'a> {
    pub fonts: TocFonts<'a>,
    pub switch: &'a str,
}

/// The 0-based physical page each paragraph of `paragraphs` starts on (section 0), from one
/// layout pass, plus the pass's font fallback count.
fn paragraph_pages(
    doc: &Document,
    layout: &TocLayout<'_>,
    paragraphs: &[usize],
    toc_table: Option<usize>,
) -> anyhow::Result<(Vec<usize>, u64)> {
    let switch = layout.switch;
    // `doc` here is the published (written and re-read) document.
    let mut store = match layout.fonts {
        TocFonts::Dirs(dirs) => {
            let mut store = hwp_render::FontStore::new();
            for dir in dirs {
                store.load_dir(dir);
            }
            store
        }
        TocFonts::Isolated(files) => {
            let mut store = hwp_render::FontStore::new_isolated();
            for file in files {
                store.load_file(file).map_err(|e| {
                    anyhow::anyhow!(
                        "{switch}: 레이아웃용 글꼴 파일을 읽을 수 없습니다: {} ({e})\n해결: 글꼴 파일 경로를 확인하거나 {switch}를 끄고 생성하세요",
                        file.display()
                    )
                })?;
            }
            store
        }
    };
    let mut warnings = hwp_render::RenderIssueAccumulator::new();
    let (list, map) =
        hwp_render::layout::layout_document_with_segments(doc, &mut store, &mut warnings);
    if map.truncated {
        anyhow::bail!(
            "{switch}: 레이아웃 세그먼트 기록이 상한에 걸려 쪽 번호를 확정할 수 없습니다\n해결: 문서를 나누어 생성하거나 {switch}를 끄고 생성하세요"
        );
    }
    // Font resolution records substitutions on the store's own accumulator; fold them in, as
    // `render` does, or the fallback count below would always read 0.
    warnings.absorb(store.issues.finish());
    let coverage = warnings.finish().font_coverage();
    let section = doc.sections.first().ok_or_else(|| {
        anyhow::anyhow!(
            "{switch}: 게시본을 다시 읽었더니 구역이 없습니다\n해결: {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
        )
    })?;
    if let Some(index) = toc_table {
        toc_fits_one_page(doc, &list, &map, index, switch)?;
    }
    let mut pages = Vec::with_capacity(paragraphs.len());
    for &index in paragraphs {
        let para = section.paragraphs.get(index).ok_or_else(|| {
            anyhow::anyhow!(
                "{switch}: 게시본에서 목차가 가리키는 문단 {index}이 없습니다\n해결: {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
            )
        })?;
        let path = hwp_render::segment_id::SegmentPath {
            section: 0,
            indices: vec![index],
        };
        let id = hwp_render::segment_id::paragraph_id(&path, para);
        let page = map
            .rows
            .iter()
            .filter(|row| row.id == id)
            .map(|row| row.page)
            .min()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{switch}: 문단 {index}(제목 또는 본문 시작)의 쪽을 레이아웃에서 찾지 못했습니다\n해결: {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
                )
            })?;
        pages.push(page);
    }
    Ok((pages, coverage.substituted + coverage.missing))
}

/// Refuses a TOC that does not fit on its page. The TOC table is placed inline (treat as
/// character), and an inline table never splits: the renderer clips whatever runs past the page,
/// so rows would be lost silently. Every cell of the table must lie on the TOC's page inside the
/// page height.
fn toc_fits_one_page(
    doc: &Document,
    list: &hwp_render::display::DisplayList,
    map: &hwp_render::segment_map::SegmentMap,
    table_paragraph: usize,
    switch: &str,
) -> anyhow::Result<()> {
    let Some(para) = doc
        .sections
        .first()
        .and_then(|s| s.paragraphs.get(table_paragraph))
    else {
        return Ok(());
    };
    let Some((ci, hwp_model::Control::Table(table))) = para
        .controls
        .iter()
        .enumerate()
        .find(|(_, c)| matches!(c, hwp_model::Control::Table(_)))
    else {
        return Ok(());
    };
    let table_path = hwp_render::segment_id::SegmentPath {
        section: 0,
        indices: vec![table_paragraph, ci],
    };
    let table_id = hwp_render::segment_id::table_id(&table_path, table);
    let page = map
        .rows
        .iter()
        .filter(|r| r.id == table_id)
        .map(|r| r.page)
        .min();
    let rows = usize::from(table.rows);
    let clipped = table.cells.iter().enumerate().any(|(k, cell)| {
        let mut indices = table_path.indices.clone();
        indices.push(k);
        let id = hwp_render::segment_id::cell_id(
            &hwp_render::segment_id::SegmentPath {
                section: 0,
                indices,
            },
            cell,
        );
        let mut placed = map.rows.iter().filter(|r| r.id == id);
        match placed.next() {
            None => true,
            Some(row) => {
                let height = list.pages.get(row.page).map_or(0.0, |p| p.height_pt);
                Some(row.page) != page
                    || placed.next().is_some()
                    || row.bbox.is_some_and(|b| b.y1 > height)
            }
        }
    });
    if page.is_none() || clipped {
        anyhow::bail!(
            "{switch}: 목차 항목이 한 쪽을 넘습니다 (항목 {rows}개). 목차 표는 쪽을 넘겨 나뉘지 않아 넘친 항목이 잘립니다\n해결: 목차에 넣을 제목(`##`·`###`, 개요 2·개요 3)을 줄이거나 {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
        );
    }
    Ok(())
}

/// Fills `plan`'s TOC in `doc` with each heading's displayed body page. Body pages count from 1
/// on the first body page (the page after the TOC), which is where `insert_toc` restarts the
/// page number. The first body paragraph's physical page is the offset.
pub fn backfill(
    doc: &mut Document,
    plan: &TocPlan,
    layout: &TocLayout<'_>,
    published: impl Fn(&Document) -> anyhow::Result<Document>,
) -> anyhow::Result<TocBackfill> {
    let switch = layout.switch;
    let body_start = plan.body_start;
    let mut targets: Vec<usize> = plan.entries.iter().map(|e| e.paragraph).collect();
    targets.push(body_start);
    let mut previous: Option<Vec<u32>> = None;
    let mut fallbacks = 0;
    for pass in 1..=MAX_PASSES {
        let (physical, fb) = paragraph_pages(
            &published(doc)?,
            layout,
            &targets,
            Some(plan.table_paragraph),
        )?;
        fallbacks = fallbacks.max(fb);
        let (body, headings) = physical.split_last().expect("body_start is a target");
        let pages: Vec<u32> = headings
            .iter()
            .map(|&p| u32::try_from(p.saturating_sub(*body) + 1).unwrap_or(u32::MAX))
            .collect();
        if previous.as_ref() == Some(&pages) {
            return Ok(TocBackfill {
                pages,
                passes: pass,
                font_fallbacks: fallbacks,
            });
        }
        fill_toc(doc, plan, &pages).map_err(|e| {
            anyhow::anyhow!(
                "{switch}: {e}\n해결: {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
            )
        })?;
        previous = Some(pages);
    }
    anyhow::bail!(
        "{switch}: 레이아웃 {MAX_PASSES}회 안에 목차 쪽 번호가 고정되지 않았습니다\n해결: 목차 항목을 줄이거나 {switch}를 끄고 생성한 뒤 한글에서 목차를 넣으세요"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With an isolated, empty font store every glyph run is a substitution or a miss, so the
    /// layout pass must report fallbacks. Before the store's own issues were absorbed the
    /// count was always 0 and the caller's warning could never fire.
    #[test]
    fn an_empty_isolated_font_store_reports_fallbacks() {
        let doc = hwp_convert::from_markdown("## 제목\n\n본문 문단입니다.\n");
        let layout = TocLayout {
            fonts: TocFonts::Isolated(&[]),
            switch: "--toc",
        };
        let (pages, fallbacks) = paragraph_pages(&doc, &layout, &[0], None).expect("layout");
        assert_eq!(pages, [0]);
        assert!(fallbacks > 0, "fallbacks = {fallbacks}");
    }
}

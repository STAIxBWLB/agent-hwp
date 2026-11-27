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
//! Each pass measures the document as the target writer produces it: the caller's `published`
//! closure writes the IR and reads it back, so the layout runs on exactly what a reader of the
//! output file (and `render --layout-json` on it) sees. An HWP5 writer synthesizes line-segment
//! caches, so the in-memory IR alone can paginate differently from the written file.

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

/// The 0-based physical page each paragraph of `paragraphs` starts on (section 0), from one
/// layout pass, plus the pass's font fallback count.
fn paragraph_pages(
    doc: &Document,
    font_dirs: &[PathBuf],
    paragraphs: &[usize],
) -> anyhow::Result<(Vec<usize>, u64)> {
    // `doc` here is the published (written and re-read) document.
    let mut store = hwp_render::FontStore::new();
    for dir in font_dirs {
        store.load_dir(dir);
    }
    let mut warnings = hwp_render::RenderIssueAccumulator::new();
    let (_, map) =
        hwp_render::layout::layout_document_with_segments(doc, &mut store, &mut warnings);
    if map.truncated {
        anyhow::bail!(
            "--toc: 레이아웃 세그먼트 기록이 상한에 걸려 쪽 번호를 확정할 수 없습니다\n해결: 문서를 나누어 생성하거나 --toc 없이 생성하세요"
        );
    }
    let coverage = warnings.finish().font_coverage();
    let section = doc
        .sections
        .first()
        .ok_or_else(|| anyhow::anyhow!("--toc: 구역이 없습니다"))?;
    let mut pages = Vec::with_capacity(paragraphs.len());
    for &index in paragraphs {
        let para = section
            .paragraphs
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("--toc: 목차 대상 문단 {index}이 없습니다"))?;
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
                    "--toc: 제목 문단 {index}의 쪽을 레이아웃에서 찾지 못했습니다\n해결: --toc 없이 생성한 뒤 한글에서 목차를 갱신하세요"
                )
            })?;
        pages.push(page);
    }
    Ok((pages, coverage.substituted + coverage.missing))
}

/// Fills `plan`'s TOC in `doc` with each heading's displayed body page. Body pages count from 1
/// on the first body page (the page after the TOC), which is where `insert_toc` restarts the
/// page number. The first body paragraph's physical page is the offset.
pub fn backfill(
    doc: &mut Document,
    plan: &TocPlan,
    font_dirs: &[PathBuf],
    published: impl Fn(&Document) -> anyhow::Result<Document>,
) -> anyhow::Result<TocBackfill> {
    let body_start = plan.table_paragraph + 1;
    let mut targets: Vec<usize> = plan.entries.iter().map(|e| e.paragraph).collect();
    targets.push(body_start);
    let mut previous: Option<Vec<u32>> = None;
    let mut fallbacks = 0;
    for pass in 1..=MAX_PASSES {
        let (physical, fb) = paragraph_pages(&published(doc)?, font_dirs, &targets)?;
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
        fill_toc(doc, plan, &pages).map_err(|e| anyhow::anyhow!("--toc: {e}"))?;
        previous = Some(pages);
    }
    anyhow::bail!(
        "--toc: 레이아웃 {MAX_PASSES}회 안에 목차 쪽 번호가 고정되지 않았습니다\n해결: 목차 항목을 줄이거나 --toc 없이 생성한 뒤 한글에서 목차를 갱신하세요"
    )
}

//! `ahwp redact`: format-preserving PII masking with a content-free report (#617, GM-10).
//!
//! The run is one atomic transaction on the native edit pipeline:
//!
//! 1. load the input and mask it in the IR ([`hwp_convert::redact::redact_document`]);
//! 2. write a staged output through the same source-preserving writers `ahwp edit` uses
//!    (HWP5 rewrite from the source container, HWPX package surgery), then regenerate the
//!    preview text and image from the masked document so neither keeps a masked value;
//! 3. re-read the staged output, rescan its body and its `PrvText` with the same detectors and
//!    check that no masked original value occurs in either. Any remaining hit refuses the run
//!    and nothing is published.
//!
//! Privacy: stderr, the report and every error message carry counts, categories and addresses
//! only. No matched value, no prefix of one and no document text is ever printed.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use hwp_convert::redact::{Category, RedactOptions, RedactOutcome, redact_document, scan_document};
use serde_json::{Value, json};

use crate::commands::cat::load_document;
use crate::format::{FileFormat, detect};

/// Largest preview stream read back for verification.
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

/// What one run asks for.
pub struct RedactRequest<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub options: RedactOptions,
    pub report: Option<&'a Path>,
    pub dry_run: bool,
    /// Also run the semantic `--verify` re-read `ahwp edit` uses.
    pub verify: bool,
    /// Print the report to stdout on a dry run without `report` (CLI only: stdout is the
    /// protocol channel under MCP).
    pub print_dry_run_report: bool,
}

/// The `hwp-redact-report-v1` value of a run.
pub fn execute(request: &RedactRequest<'_>) -> anyhow::Result<Value> {
    let input_format = detect(request.input)?;
    let output_format = output_format_of(request.output)?;
    if input_format != output_format {
        anyhow::bail!(
            "redact는 입력과 같은 형식으로만 출력합니다 (입력 .{} → 출력 .{})",
            ext(input_format),
            ext(output_format)
        );
    }
    let original = load_document(request.input)?;
    let mut doc = original.clone();
    let outcome = redact_document(&mut doc, &request.options);

    let verification = std::cell::Cell::new(Verification::default());
    if outcome.counts().is_empty() && outcome.raw_counts.is_empty() {
        // Nothing to mask: publish nothing, and say so without failing.
        let report = report_json(request, input_format, &outcome, &verification.get());
        emit(request, &report)?;
        eprintln!("redact: 검출된 개인정보가 없어 출력을 게시하지 않습니다");
        return Ok(report);
    }

    let opts = &request.options;
    let verify_staged = |staged: &Path, _: &()| -> anyhow::Result<()> {
        let reread = load_document(staged).context("redact 출력 재읽기 실패")?;
        let body: usize = scan_document(&reread, opts).values().sum();
        let preview = read_preview(staged, input_format)?;
        let preview_hits: usize = hwp_convert::redact::scan_text(&preview, opts)
            .values()
            .sum();
        let text = format!(
            "{}\n{}",
            reread.plain_text_with(&hwp_model::TextOptions {
                include_header_footer: true,
                include_hidden: true,
            }),
            preview
        );
        let leaked = outcome.originals_present_in(&text);
        verification.set(Verification {
            body: Some(body),
            preview: Some(preview_hits),
            originals_absent: Some(leaked == 0),
        });
        if body > 0 || preview_hits > 0 || leaked > 0 {
            anyhow::bail!(
                "redact 검증 실패: 출력에 남은 검출 본문 {body}건, 미리보기 {preview_hits}건, 원래 값 {leaked}건 — 출력을 게시하지 않습니다"
            );
        }
        if request.verify {
            crate::commands::edit::verify_output(staged, Some(&doc))?;
        }
        Ok(())
    };
    let write_staged = |source: &Path, staged: &Path| -> anyhow::Result<()> {
        write_masked(source, &original, &doc, staged, input_format)
    };

    let result = match input_format {
        FileFormat::Hwp5 => crate::commands::output::write_with_private_input_snapshot(
            request.output,
            request.input,
            hwp_cli::certification::MAX_INPUT_BYTES,
            if request.dry_run {
                crate::commands::output::SnapshotOutputMode::ValidateOnly
            } else {
                crate::commands::output::SnapshotOutputMode::Publish
            },
            |snapshot, staged, _| write_staged(snapshot, staged),
            &verify_staged,
        )
        .map(|_| ()),
        FileFormat::Hwpx if request.dry_run => crate::commands::output::validate_without_publish(
            request.output,
            Some(request.input),
            |staged| write_staged(request.input, staged),
            &verify_staged,
        ),
        FileFormat::Hwpx => crate::commands::output::write_validated(
            request.output,
            Some(request.input),
            |staged| write_staged(request.input, staged),
            &verify_staged,
        ),
    };
    let report = report_json(request, input_format, &outcome, &verification.get());
    if let Err(error) = result {
        // A refused run still leaves its content-free report when one was asked for.
        if let Err(write_error) = emit(request, &report) {
            eprintln!("경고: redact 보고서를 쓰지 못했습니다: {write_error:#}");
        }
        return Err(error);
    }
    emit(request, &report)?;
    let total: usize = outcome.counts().values().sum();
    if request.dry_run {
        eprintln!("redact dry-run: {total}건 검출 (출력 파일은 게시하지 않음)");
    } else {
        eprintln!(
            "redact 완료: {total}건 마스킹 → {}",
            request.output.display()
        );
    }
    Ok(report)
}

/// CLI entry.
pub fn run(request: &RedactRequest<'_>) -> anyhow::Result<()> {
    execute(request).map(|_| ())
}

fn ext(format: FileFormat) -> &'static str {
    match format {
        FileFormat::Hwp5 => "hwp",
        FileFormat::Hwpx => "hwpx",
    }
}

fn output_format_of(output: &Path) -> anyhow::Result<FileFormat> {
    match output
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("hwp") => Ok(FileFormat::Hwp5),
        Some("hwpx") => Ok(FileFormat::Hwpx),
        _ => anyhow::bail!(
            "redact 출력은 .hwp 또는 .hwpx 여야 합니다: {}",
            output.display()
        ),
    }
}

/// The preview image of the masked document, rendered at the writer's preview size.
fn preview_image(doc: &hwp_model::Document) -> Option<Vec<u8>> {
    let font_dir =
        std::path::PathBuf::from(std::env::var("HWP_FONT_DIR").unwrap_or_else(|_| "fonts".into()));
    let options = hwp_render::RenderOptions {
        dpi: 48.0,
        font_dirs: vec![font_dir],
    };
    hwp_render::render_document_pages(doc, &options, Some(&[1]))
        .ok()
        .and_then(|out| out.pages.first().and_then(|page| page.encode_png().ok()))
}

fn write_masked(
    source: &Path,
    original: &hwp_model::Document,
    doc: &hwp_model::Document,
    staged: &Path,
    format: FileFormat,
) -> anyhow::Result<()> {
    match format {
        FileFormat::Hwp5 => {
            // The source-preserving HWP5 rewrite regenerates /PrvText (and /PrvImage when it can
            // render) whenever a section changed, which masking always does.
            let report = crate::commands::convert::write_hwp_preserving_source(
                source, original, doc, staged, true, false,
            )?;
            crate::commands::reject_preservation_loss("redact", &report.preservation)?;
            reject_stale_hwp_preview_image(source, staged)?;
        }
        FileFormat::Hwpx => {
            let parent = staged.parent().context("임시 출력 작업공간이 없습니다")?;
            let body = parent.join(".hwp-redact-body.hwpx");
            let result = (|| -> anyhow::Result<()> {
                let report =
                    crate::commands::edit::write_hwpx_surgical(source, original, doc, &body, None)?;
                crate::commands::reject_preservation_loss("redact", &report.preservation)?;
                // Package surgery raw-copies Preview/, so both previews are regenerated here.
                let image = preview_image(doc);
                let done = hwpx::patch::replace_previews(
                    &body,
                    staged,
                    &doc.plain_text(),
                    image.as_deref(),
                )?;
                if original.hwpx_preview_image.is_some() && !done.image {
                    anyhow::bail!(
                        "redact: 미리보기 이미지(Preview/PrvImage.png)를 다시 만들 수 없어 출력을 게시하지 않습니다 (HWP_FONT_DIR 확인)"
                    );
                }
                Ok(())
            })();
            let _ = std::fs::remove_file(&body);
            result?;
        }
    }
    Ok(())
}

/// An HWP5 `/PrvImage` copied unchanged from the source still shows the unmasked first page.
fn reject_stale_hwp_preview_image(source: &Path, staged: &Path) -> anyhow::Result<()> {
    let read = |path: &Path| -> anyhow::Result<Option<Vec<u8>>> {
        let mut container = hwp5::Hwp5Container::open(path)?;
        if !container
            .list_streams()
            .iter()
            .any(|stream| stream.path == "/PrvImage")
        {
            return Ok(None);
        }
        Ok(Some(container.read_stream_raw_bounded(
            "/PrvImage",
            MAX_PREVIEW_BYTES * 4,
        )?))
    };
    if let (Some(before), Some(after)) = (read(source)?, read(staged)?)
        && before == after
    {
        anyhow::bail!(
            "redact: 미리보기 이미지(/PrvImage)를 다시 만들 수 없어 출력을 게시하지 않습니다 (HWP_FONT_DIR 확인)"
        );
    }
    Ok(())
}

fn decode_utf16le(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// The preview text of a written file, decoded as the readers do.
fn read_preview(path: &Path, format: FileFormat) -> anyhow::Result<String> {
    Ok(match format {
        FileFormat::Hwp5 => {
            let mut container = hwp5::Hwp5Container::open(path)?;
            if !container
                .list_streams()
                .iter()
                .any(|stream| stream.path == "/PrvText")
            {
                return Ok(String::new());
            }
            decode_utf16le(&container.read_stream_raw_bounded("/PrvText", MAX_PREVIEW_BYTES)?)
        }
        FileFormat::Hwpx => {
            let mut package = hwpx::HwpxPackage::open(path)?;
            if !package
                .entries()?
                .iter()
                .any(|entry| entry.name == "Preview/PrvText.txt")
            {
                return Ok(String::new());
            }
            let raw = package.read_entry_bounded("Preview/PrvText.txt", MAX_PREVIEW_BYTES)?;
            match String::from_utf8(raw) {
                Ok(text) => text,
                Err(error) => decode_utf16le(error.as_bytes()),
            }
        }
    })
}

/// Verification results, filled by the staged verifier.
#[derive(Default, Clone, Copy)]
struct Verification {
    body: Option<usize>,
    preview: Option<usize>,
    originals_absent: Option<bool>,
}

fn report_json(
    request: &RedactRequest<'_>,
    format: FileFormat,
    outcome: &RedactOutcome,
    verification: &Verification,
) -> Value {
    let counts: BTreeMap<&'static str, usize> = outcome
        .counts()
        .into_iter()
        .map(|(category, n)| (category.as_str(), n))
        .collect();
    let total: usize = counts.values().sum();
    let hits: Vec<Value> = outcome
        .hits
        .iter()
        .map(|hit| {
            let mut value = json!({
                "category": hit.category.as_str(),
                "kind": hit.segment.kind.as_str(),
                "path": hit.segment.path,
                "paragraph_id": hit.segment.paragraph_id,
                "start": hit.start,
                "end": hit.end,
            });
            if let Some(index) = hit.segment.table_index {
                value["table_index"] = json!(index);
            }
            if let Some(row) = hit.segment.row {
                value["row"] = json!(row);
            }
            if let Some(col) = hit.segment.col {
                value["col"] = json!(col);
            }
            value
        })
        .collect();
    let mut notes = Vec::new();
    if outcome.has_images {
        notes.push("images_not_inspected");
    }
    notes.push("metadata_not_scrubbed");
    let detectors: Vec<&'static str> = request
        .options
        .detectors
        .categories()
        .iter()
        .map(|category: &Category| category.as_str())
        .collect();
    json!({
        "schema_version": "1.0",
        "contract": "hwp-redact-report-v1",
        "input_format": ext(format),
        "output_format": ext(format),
        "dry_run": request.dry_run,
        "detectors": detectors,
        "strict_accounts": request.options.strict_accounts,
        "counts": counts,
        "total": total,
        "hits": hits,
        "raw_record_hits": outcome.raw_counts.values().sum::<usize>(),
        "verification": {
            "body_rescan": verification.body,
            "preview_rescan": verification.preview,
            "originals_absent": verification.originals_absent,
        },
        "notes": notes,
    })
}

/// Writes `--report`, or prints the report to stdout on `--dry-run` without `--report`.
fn emit(request: &RedactRequest<'_>, report: &Value) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if let Some(path) = request.report {
        crate::commands::output::write_validated(
            path,
            None,
            |staged| {
                std::fs::write(staged, &bytes)?;
                Ok(())
            },
            |staged, _| {
                if std::fs::read(staged)? != bytes {
                    anyhow::bail!("redact 보고서 검증 중 바이트 불일치");
                }
                Ok(())
            },
        )?;
    } else if request.dry_run && request.print_dry_run_report {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

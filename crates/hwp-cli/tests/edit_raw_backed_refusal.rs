//! Native raw-list edits, enclosing record retention and atomic refusal of opaque children.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

#[path = "common/fixture_skip.rs"]
mod fixture_skip;
#[path = "common/temp_path.rs"]
mod temp_path;

fn hwp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(args: &[&OsStr]) -> Output {
    hwp().args(args).output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn workdir(name: &str) -> temp_path::TempPath {
    let dir = temp_path::TempPath::new("hwp577", name);
    let _ = std::fs::remove_dir_all(&*dir);
    std::fs::create_dir_all(&*dir).unwrap();
    dir
}

/// The committed sample as an HWP5 file. Its footnotes and endnote are raw-backed controls.
fn hwp5_with_raw_backed_notes(dir: &Path) -> PathBuf {
    let sample = repo().join("fixtures/samples/report-tables.hwpx");
    let hwp5 = dir.join("input.hwp");
    let out = run(&[
        "convert".as_ref(),
        sample.as_os_str(),
        "-o".as_ref(),
        hwp5.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    hwp5
}

fn validator() -> jsonschema::Validator {
    let schema: Value =
        serde_json::from_str(include_str!("../../../schemas/edit-report-v1.schema.json")).unwrap();
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .unwrap()
}

fn read_report(path: &Path) -> Value {
    let report: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(validator().is_valid(&report), "{report}");
    report
}

fn edit(input: &Path, output: &Path, report: &Path, args: &[&str]) -> Output {
    let mut command: Vec<&OsStr> = vec![
        "edit".as_ref(),
        input.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
    ];
    command.extend(args.iter().map(OsStr::new));
    command.extend(["--report".as_ref(), report.as_os_str()]);
    run(&command)
}

fn text(path: &Path) -> String {
    let out = run(&["cat".as_ref(), path.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The refusal a dropped edit must carry: code, the control kind and count, and a message that
/// names the cause and a next step.
fn assert_raw_backed_refusal(report: &Value, kind: &str) {
    let refusal = &report["refusal"];
    assert_eq!(refusal["code"], "raw_backed_edit", "{report}");
    let controls = refusal["values"]["controls"].as_array().unwrap();
    assert!(
        controls
            .iter()
            .any(|control| control["kind"] == kind && control["count"].as_u64() >= Some(1)),
        "expected a {kind} control in {report}"
    );
    let message = refusal["message"].as_str().unwrap();
    assert_eq!(message, hwp_cli::refusal::fixed_message("raw_backed_edit"));
    assert!(
        message.contains(".hwpx") && message.contains("한글"),
        "the message names a next step: {message}"
    );
}

fn notes(path: &Path) -> Vec<hwp_model::GenericControl> {
    hwp5::read_document(path)
        .unwrap()
        .document
        .sections
        .into_iter()
        .flat_map(|section| section.paragraphs)
        .flat_map(|paragraph| paragraph.controls)
        .filter_map(|control| match control {
            hwp_model::Control::Generic(generic)
                if matches!(&generic.ctrl_id, b"fn  " | b"en  ") =>
            {
                Some(generic)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn native_footnote_edit_preserves_untouched_notes() {
    let dir = workdir("footnote");
    let input = hwp5_with_raw_backed_notes(&dir);
    let before = notes(&input);
    for verify in [false, true] {
        let output = dir.join(format!("out-{verify}.hwp"));
        let report = dir.join(format!("report-{verify}.json"));
        let mut args = vec!["--replace", "각주 예시 1=>각주 예시 X"];
        if verify {
            args.push("--verify");
        }
        let edited = edit(&input, &output, &report, &args);
        assert!(
            edited.status.success(),
            "verify={verify}: {}",
            stderr(&edited)
        );
        assert!(text(&output).contains("각주 예시 X"));
        let after = notes(&output);
        assert_eq!(before.len(), after.len());
        let mut changed = 0;
        for (old, new) in before.iter().zip(&after) {
            if old.paragraph_lists == new.paragraph_lists {
                assert_eq!(old.raw_children, new.raw_children);
            } else {
                changed += 1;
                assert_eq!(old.ctrl_id, new.ctrl_id);
                assert_eq!(old.data, new.data);
                assert_eq!(old.raw_children[0], new.raw_children[0]);
            }
        }
        assert_eq!(changed, 1);
    }
}

#[test]
fn edit_of_note_with_an_opaque_child_refuses_atomically() {
    let dir = workdir("opaque");
    let input = hwp5_with_raw_backed_notes(&dir);
    let mut document = hwp5::read_document(&input).unwrap().document;
    let note = document
        .sections
        .iter_mut()
        .flat_map(|section| &mut section.paragraphs)
        .flat_map(|paragraph| &mut paragraph.controls)
        .find_map(|control| match control {
            hwp_model::Control::Generic(generic) if generic.ctrl_id == *b"fn  " => Some(generic),
            _ => None,
        })
        .unwrap();
    use hwp5::record::tag;
    let paragraph = note
        .raw_children
        .iter_mut()
        .find(|record| record.tag == tag::PARA_HEADER)
        .unwrap();
    paragraph.children.push(hwp_model::OpaqueRecord {
        tag: tag::CTRL_HEADER,
        data: b"zzzz".to_vec(),
        children: vec![hwp_model::OpaqueRecord {
            tag: tag::CTRL_DATA,
            data: vec![42],
            children: Vec::new(),
        }],
    });
    let source = dir.join("opaque.hwp");
    hwp5::write_document(
        &document,
        &source,
        &hwp5::WriteOptions {
            preserve_linesegs: true,
            ..Default::default()
        },
    )
    .unwrap();
    let output = dir.join("out.hwp");
    let report = dir.join("report.json");
    let result = edit(
        &source,
        &output,
        &report,
        &["--replace", "각주 예시 1=>각주 예시 X"],
    );
    assert!(!result.status.success(), "{}", stderr(&result));
    assert!(!output.exists());
    assert_raw_backed_refusal(&read_report(&report), "footnote");
}

/// The same edit is written when the output is `.hwpx`, which emits from the edited paragraphs,
/// so the refusal is specific to the HWP5 writer and the message's next step is real.
#[test]
fn the_same_footnote_edit_to_hwpx_is_written() {
    let dir = workdir("hwpx");
    let input = hwp5_with_raw_backed_notes(&dir);
    let output = dir.join("out.hwpx");
    let report = dir.join("report.json");
    let edited = edit(
        &input,
        &output,
        &report,
        &["--replace", "각주 예시 1=>각주 예시 X"],
    );
    assert!(edited.status.success(), "{}", stderr(&edited));
    assert!(text(&output).contains("각주 예시 X"));
}

/// An edit that does not touch a raw-backed paragraph is unaffected, on a document that holds
/// raw-backed controls: body text and metadata still publish and the body text lands.
#[test]
fn edits_outside_raw_backed_controls_still_publish() {
    let dir = workdir("outside");
    let input = hwp5_with_raw_backed_notes(&dir);
    let output = dir.join("out.hwp");
    let report = dir.join("report.json");
    let edited = edit(
        &input,
        &output,
        &report,
        &["--replace", "예시 문장 2입니다=>예시 문장 2X입니다"],
    );
    assert!(edited.status.success(), "{}", stderr(&edited));
    assert!(text(&output).contains("예시 문장 2X입니다"));
    let notes_intact = text(&output).contains("각주 예시 1");
    assert!(notes_intact, "the footnote text is unchanged");

    let meta_output = dir.join("meta.hwp");
    let edited = edit(
        &input,
        &meta_output,
        &dir.join("meta.json"),
        &["--set-meta", "title=probe"],
    );
    assert!(edited.status.success(), "{}", stderr(&edited));
}

fn save_hancom_fixture(output: &Path, name: &str) {
    if let Some(directory) = std::env::var_os("HWP_NATIVE_EDIT_CHECK_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(output, directory.join(name)).unwrap();
    }
}

/// These Hancom Office fixtures are local-only. The upstream filenames and
/// provenance are recorded in hwp-swift's CoreHwpTests/Fixtures README.
#[test]
fn native_text_boxes_headers_footers_and_notes_verify() {
    let dir = workdir("saved");
    for (fixture, ctrl_id, source, replacement, width) in [
        (
            "text-box",
            *b"gso ",
            "Text box fixture",
            "Text box edited",
            22368,
        ),
        (
            "header-footer",
            *b"head",
            "CoreHwp header fixture",
            "CoreHwp header edited",
            42520,
        ),
        (
            "header-footer",
            *b"foot",
            "CoreHwp footer fixture",
            "CoreHwp footer edited",
            42520,
        ),
        (
            "footnote-endnote",
            *b"fn  ",
            "CoreHwp footnote fixture",
            "CoreHwp footnote edited",
            42520,
        ),
        (
            "footnote-endnote",
            *b"en  ",
            "CoreHwp endnote fixture",
            "CoreHwp endnote edited",
            42520,
        ),
    ] {
        let input = repo().join(format!("fixtures/hwp5/624-{fixture}.hwp"));
        if fixture_skip::fixture_missing(&input) {
            continue;
        }
        let report = dir.join("report.json");
        let output = dir.join("out.hwp");
        let result = edit(
            &input,
            &output,
            &report,
            &["--replace", &format!("{source}=>{replacement}"), "--verify"],
        );
        assert!(result.status.success(), "{fixture}: {}", stderr(&result));
        let doc = hwp5::read_document(&output).unwrap().document;
        let control = doc
            .sections
            .iter()
            .flat_map(|section| &section.paragraphs)
            .flat_map(|paragraph| &paragraph.controls)
            .find_map(|control| match control {
                hwp_model::Control::Generic(generic) if generic.ctrl_id == ctrl_id => Some(generic),
                _ => None,
            })
            .unwrap();
        let paragraphs = &control.paragraph_lists[0].paragraphs;
        assert!(paragraphs.iter().any(|p| {
            let visible: String = p
                .chars
                .iter()
                .filter_map(|c| match c {
                    hwp_model::HwpChar::Text(c) => Some(*c),
                    _ => None,
                })
                .collect();
            visible.contains(replacement)
        }));
        for (index, paragraph) in paragraphs.iter().enumerate() {
            assert_eq!(
                paragraph.header.chars_flags & 0x80 != 0,
                index + 1 == paragraphs.len()
            );
            assert!(!paragraph.line_segs.is_empty());
            assert!(
                paragraph
                    .line_segs
                    .iter()
                    .all(|line| line.seg_width == width && line.flags == 0x60000),
                "{fixture} {ctrl_id:?}: {:?}",
                paragraph.line_segs
            );
        }
        let kind = std::str::from_utf8(&ctrl_id).unwrap().trim();
        save_hancom_fixture(&output, &format!("{fixture}-{kind}-edited.hwp"));
    }
}

#[test]
fn native_text_box_table_cell_verifies_and_keeps_frame() {
    use hwp_model::Control;
    use hwp5::record::{ScanMode, scan_stream, tag};

    let fixture = repo().join("fixtures/hwp5/624-text-box.hwp");
    if fixture_skip::fixture_missing(&fixture) {
        return;
    }
    let dir = workdir("box-table");
    let mut doc = hwp5::read_document(&fixture).unwrap().document;
    let anchor = "Body anchor";
    let anchor_index = doc.sections[0].paragraphs.len();
    doc.sections[0].paragraphs.push(hwp_model::Paragraph {
        chars: anchor.chars().map(hwp_model::HwpChar::Text).collect(),
        ..Default::default()
    });
    hwp_convert::add_table(&mut doc, anchor, &[vec!["Original cell".into()]]).unwrap();
    let paragraphs = &mut doc.sections[0].paragraphs;
    let table_owner = paragraphs
        .iter()
        .position(|paragraph| {
            paragraph
                .controls
                .iter()
                .any(|control| matches!(control, Control::Table(_)))
        })
        .unwrap();
    let Control::Table(table) = &mut paragraphs[table_owner].controls[0] else {
        panic!("expected an appended table");
    };
    table.cells[0].width.0 = 9000;
    let options = hwp5::WriteOptions {
        preserve_linesegs: true,
        edited: true,
        ..Default::default()
    };
    let intermediate = dir.join("body-table.hwp");
    hwp5::write_document(&doc, &intermediate, &options).unwrap();
    let mut container = hwp5::Hwp5Container::open(&intermediate).unwrap();
    let records = container.read_record_stream("/BodyText/Section0").unwrap();
    let records = scan_stream(&records, ScanMode::Strict).unwrap().roots;
    let mut table_record = hwp5::doc_info::to_opaque(
        records
            .iter()
            .filter(|record| record.tag == tag::PARA_HEADER)
            .nth(table_owner)
            .unwrap(),
    );
    // Insert the table paragraph before the saved last paragraph. In fixtures created
    // by Hancom, LIST_HEADER and its paragraphs are siblings under SHAPE_COMPONENT.
    table_record.data[3] &= 0x7f;
    let mut doc = hwp5::read_document(&intermediate).unwrap().document;
    let paragraphs = &mut doc.sections[0].paragraphs;
    paragraphs.remove(table_owner);
    paragraphs.remove(anchor_index);
    paragraphs.last_mut().unwrap().header.chars_flags |= 0x80;
    let text_box = paragraphs
        .iter_mut()
        .flat_map(|paragraph| &mut paragraph.controls)
        .find_map(|control| match control {
            Control::Generic(generic) if generic.ctrl_id == *b"gso " => Some(generic),
            _ => None,
        })
        .unwrap();
    let shape = text_box
        .raw_children
        .iter_mut()
        .find(|r| r.tag == tag::SHAPE_COMPONENT)
        .unwrap();
    let header = shape
        .children
        .iter()
        .position(|r| r.tag == tag::LIST_HEADER)
        .unwrap();
    shape.children[header].data[..4].copy_from_slice(&3_i32.to_le_bytes());
    shape.children.insert(header + 1, table_record);
    let before = text_box.raw_children.clone();
    let source = dir.join("box-table.hwp");
    hwp5::write_document(
        &doc,
        &source,
        &hwp5::WriteOptions {
            preserve_linesegs: true,
            ..Default::default()
        },
    )
    .unwrap();

    let output = dir.join("out.hwp");
    let result = edit(
        &source,
        &output,
        &dir.join("report.json"),
        &["--set-cell", "0:0:0=Updated cell", "--verify"],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let doc = hwp5::read_document(&output).unwrap().document;
    let text_box = doc.sections[0]
        .paragraphs
        .iter()
        .flat_map(|paragraph| &paragraph.controls)
        .find_map(|control| match control {
            Control::Generic(generic) if generic.ctrl_id == *b"gso " => Some(generic),
            _ => None,
        })
        .unwrap();
    let mut after = text_box.raw_children.clone();
    let mut before = before;
    for records in [&mut before, &mut after] {
        let shape = records
            .iter_mut()
            .find(|r| r.tag == tag::SHAPE_COMPONENT)
            .unwrap();
        shape.children.retain(|r| r.tag != tag::PARA_HEADER);
    }
    assert_eq!(before, after);
    save_hancom_fixture(&output, "text-box-table-cell-edited.hwp");
}

/// Runs `edit --ops` with a typed batch against `input` and returns (output, report).
fn edit_ops(input: &Path, dir: &Path, label: &str, ops: Value) -> (Output, PathBuf, PathBuf) {
    let ops_path = dir.join(format!("{label}.ops.json"));
    std::fs::write(&ops_path, serde_json::to_vec(&ops).unwrap()).unwrap();
    let output = dir.join(format!("{label}.hwp"));
    let report = dir.join(format!("{label}.report.json"));
    let edited = edit(
        input,
        &output,
        &report,
        &["--ops", ops_path.to_str().unwrap()],
    );
    (edited, output, report)
}

/// A structural op that moves every later paragraph down by one and is legal: after paragraph
/// 1, which holds plain text. (Before paragraph 0 would refuse as `section_owner_insert`, since
/// paragraph 0 holds the section definition, and never reach the check under test.)
fn insert_at_top() -> Value {
    json!({"op":"insert_blocks","address":{"at":{"section":0,"paragraph":1}},
        "position":"after",
        "blocks":[{"type":"paragraph","runs":[{"type":"text","text":"앞쪽 삽입"}]}]})
}

fn footnote_replace() -> Value {
    json!({"op":"replace","from":"각주 예시 1","to":"각주 예시 X"})
}

/// A structural op shifts the containing paragraph before the edit.
#[test]
fn a_structural_op_before_a_footnote_edit_does_not_hide_it() {
    let dir = workdir("mixed");
    let input = hwp5_with_raw_backed_notes(&dir);
    let (edited, output, _) = edit_ops(
        &input,
        &dir,
        "mixed",
        json!([insert_at_top(), footnote_replace()]),
    );
    assert!(edited.status.success(), "{}", stderr(&edited));
    assert!(text(&output).contains("각주 예시 X"));
}

/// #577 review (b): a body-only structural edit in front of paragraphs that hold footnotes must
/// publish. The sample has a footnote in paragraph 4 and an endnote in paragraph 7; inserting or
/// deleting one paragraph in front of them shifts both by one, and positional pairing compared
/// each note with the other and refused an edit that never touched a note.
#[test]
fn a_body_only_structural_edit_in_front_of_notes_publishes() {
    let dir = workdir("structural");
    let input = hwp5_with_raw_backed_notes(&dir);
    let plans = [
        ("insert", json!([insert_at_top()])),
        (
            "delete",
            json!([{"op":"delete_para","address":{"at":{"section":0,"paragraph":2}}}]),
        ),
    ];
    for (label, ops) in plans {
        let (edited, output, report) = edit_ops(&input, &dir, label, ops);
        assert!(
            edited.status.success(),
            "{label}: a body-only structural edit must publish: {}",
            stderr(&edited)
        );
        assert!(output.exists(), "{label}: published");
        let report = read_report(&report);
        assert!(
            report.get("refusal").is_none() || report["refusal"].is_null(),
            "{label}: {report}"
        );
        let body = text(&output);
        assert!(
            body.contains("각주 예시 1"),
            "{label}: the footnote text is untouched"
        );
        assert!(
            body.contains("미주 예시 1"),
            "{label}: the endnote text is untouched"
        );
    }
}

/// The committed sample with its endnote paragraph moved directly after the footnote paragraph,
/// so two adjacent paragraphs each hold a raw-backed note (paragraphs 4 and 5).
fn hwp5_with_adjacent_notes(dir: &Path) -> PathBuf {
    let base = hwp5_with_raw_backed_notes(dir);
    let json_path = dir.join("adjacent.json");
    let out = run(&[
        "convert".as_ref(),
        base.as_os_str(),
        "-o".as_ref(),
        json_path.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
    let paragraphs = doc["sections"][0]["paragraphs"].as_array_mut().unwrap();
    let endnote = paragraphs.remove(7);
    paragraphs.insert(5, endnote);
    std::fs::write(&json_path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let moved = dir.join("adjacent.hwp");
    let out = run(&[
        "new".as_ref(),
        "--from".as_ref(),
        json_path.as_os_str(),
        "-o".as_ref(),
        moved.as_os_str(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    moved
}

/// #577 review (b), the sharper case: with two notes in adjacent paragraphs, one inserted
/// paragraph in front of them moves both by one, so positional pairing compared the footnote
/// with the endnote, saw a difference, and refused an edit that never touched a note.
#[test]
fn a_structural_edit_in_front_of_adjacent_notes_is_not_refused() {
    let dir = workdir("adjacent");
    let input = hwp5_with_adjacent_notes(&dir);
    for (label, ops) in [
        ("insert", json!([insert_at_top()])),
        (
            "delete",
            json!([{"op":"delete_para","address":{"at":{"section":0,"paragraph":2}}}]),
        ),
    ] {
        let (edited, output, report) = edit_ops(&input, &dir, label, ops);
        assert!(
            edited.status.success(),
            "{label}: the notes are untouched, so this must publish: {}",
            stderr(&edited)
        );
        assert!(output.exists(), "{label}: published");
        let report = read_report(&report);
        assert!(
            report.get("refusal").is_none() || report["refusal"].is_null(),
            "{label}: {report}"
        );
    }
}

/// A move reorders paragraphs too. The edit reaches the other note after the move.
#[test]
fn a_moved_note_paragraph_does_not_hide_an_edit_to_a_note() {
    let dir = workdir("moved");
    let input = hwp5_with_adjacent_notes(&dir);
    let (edited, output, _) = edit_ops(
        &input,
        &dir,
        "moved",
        json!([
            {"op":"move_para","address":{"at":{"section":0,"paragraph":5}},
                "to":{"address":{"at":{"section":0,"paragraph":2}},"position":"before"}},
            footnote_replace()
        ]),
    );
    assert!(edited.status.success(), "{}", stderr(&edited));
    assert!(text(&output).contains("각주 예시 X"));
}

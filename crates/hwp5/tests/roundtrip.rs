//! IR 경유 hwp→hwp 왕복 테스트.
//!
//! - 모든 fixture: 의미 동등 (재파싱 텍스트/구조 일치)
//! - 단순 컨트롤만 있는 파일(hello_world): **압축 해제 스트림 바이트 동일**
//!   — prefix+tail 보존 전략의 최종 증명.

use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hwp5")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hwp5-write-tests-{}-{name}", std::process::id()))
}

#[path = "../../hwp-cli/tests/common/fixture_skip.rs"]
mod fixture_skip;

/// Fixture binaries are not in the repository (local only). `true` means skip, and the skip is
/// counted (#275).
#[track_caller]
fn skip_if_no_fixtures() -> bool {
    fixture_skip::fixture_missing(&fixture("hello_world.hwp"))
}

/// 전체 fixture: IR 경유 재저장(--preserve-layout)이 **압축 해제 스트림
/// 바이트 동일**해야 한다 — 표/그림/도형/책갈피/빈 셀 모두 포함.
/// hwp→IR→hwp가 레코드 수준 무손실임의 최종 증명 (한글 '손상' 판정 방지).
#[test]
fn 전체_fixture_바이트_동일_왕복() {
    if skip_if_no_fixtures() {
        return;
    }
    let opts = hwp5::WriteOptions {
        preserve_linesegs: true,
        ..Default::default()
    };
    for name in [
        "hello_world.hwp",
        "bookmark.hwp",
        "color_fill.hwp",
        "outline.hwp",
        "work_report.hwp",
        "annual_report.hwp",
    ] {
        let src = fixture(name);
        let doc = hwp5::read_document(&src).unwrap().document;
        let out = tmp(&format!("byteid_{name}"));
        hwp5::write_document(&doc, &out, &opts).unwrap();

        let mut orig = hwp5::Hwp5Container::open(&src).unwrap();
        let mut ours = hwp5::Hwp5Container::open(&out).unwrap();
        let mut streams = vec!["/DocInfo".to_string()];
        streams.extend(orig.body_sections());
        for stream in streams {
            let a = orig.read_record_stream(&stream).unwrap();
            let b = ours.read_record_stream(&stream).unwrap();
            assert_eq!(
                a,
                b,
                "{name} {stream}: 압축 해제 스트림 불일치 ({} vs {} 바이트)",
                a.len(),
                b.len()
            );
        }
        // FileHeader: EncryptVersion=4 + 압축 플래그 (한글 호환 필수)
        assert!(ours.file_header().is_compressed(), "{name}");
        let _ = std::fs::remove_file(&out);
    }
}

/// 전체 fixture: 의미 동등 왕복.
#[test]
fn 전체_fixture_의미_왕복() {
    if skip_if_no_fixtures() {
        return;
    }
    for name in [
        "hello_world.hwp",
        "bookmark.hwp",
        "color_fill.hwp",
        "outline.hwp",
        "work_report.hwp",
    ] {
        let doc = hwp5::read_document(&fixture(name)).unwrap().document;
        let out = tmp(&format!("rt_{name}"));
        let opts = hwp5::WriteOptions {
            preserve_linesegs: true,
            ..Default::default()
        };
        hwp5::write_document(&doc, &out, &opts).unwrap();

        let reread = hwp5::read_document(&out).unwrap_or_else(|e| panic!("{name}: {e}"));
        for w in &reread.warnings {
            assert!(!w.contains("불일치"), "{name}: {w}");
        }
        let doc2 = reread.document;
        assert_eq!(doc2.plain_text(), doc.plain_text(), "{name}: 텍스트 불일치");
        assert_eq!(
            doc2.header.char_shapes.len(),
            doc.header.char_shapes.len(),
            "{name}: 문자 모양 수"
        );
        assert_eq!(doc2.sections.len(), doc.sections.len(), "{name}");
        // 줄 배치 보존 (렌더링 충실도)
        let segs = |d: &hwp_model::Document| {
            d.sections
                .iter()
                .flat_map(|s| &s.paragraphs)
                .map(|p| p.line_segs.len())
                .sum::<usize>()
        };
        assert_eq!(segs(&doc2), segs(&doc), "{name}: lineseg 수");
        let _ = std::fs::remove_file(&out);
    }
}

#[test]
fn equation_properties_survive_hwp5_hwpx_hwp5() {
    use hwp_model::{Control, Equation, GenericControl, HwpChar};

    let expected = Equation {
        script: "LEFT [ x over y RIGHT ]".into(),
        inline: true,
        width: 2000,
        height: 3500,
        props: 1,
        base_unit: 1_800,
        text_color: 0x009c_5210,
        baseline: 68,
        version: Some("Equation Version 60".into()),
        font_name: Some("HancomEQN & Math".into()),
        ..Default::default()
    };
    let mut document = hwp_convert::from_markdown("Equation");
    let paragraph = &mut document.sections[0].paragraphs[0];
    paragraph.chars.push(HwpChar::ExtCtrl {
        code: 11,
        ctrl_id: *b"eqed",
        payload: hwp_convert::field::rev_payload(b"eqed"),
        ctrl_index: Some(paragraph.controls.len() as u32),
    });
    paragraph.controls.push(Control::Generic(GenericControl {
        ctrl_id: *b"eqed",
        data: Vec::new(),
        paragraph_lists: Vec::new(),
        extras: Vec::new(),
        raw_children: Vec::new(),
        gso_shapes: Vec::new(),
        equation: Some(Box::new(expected.clone())),
        column_def: None,
        caption: None,
        hwpx_raw_xml: None,
        container_box: None,
    }));

    let equation = |document: &hwp_model::Document| {
        document.sections[0]
            .paragraphs
            .iter()
            .flat_map(|paragraph| &paragraph.controls)
            .find_map(|control| match control {
                Control::Generic(generic) => generic.equation.clone(),
                _ => None,
            })
            .expect("the equation survived conversion")
    };
    let assert_properties = |actual: &Equation| {
        assert_eq!(actual.script, expected.script);
        assert_eq!(actual.props, expected.props);
        assert_eq!(actual.base_unit, expected.base_unit);
        assert_eq!(actual.text_color, expected.text_color);
        assert_eq!(actual.baseline, expected.baseline);
        assert_eq!(actual.version, expected.version);
        assert_eq!(actual.font_name, expected.font_name);
    };
    let native = tmp("equation_native.hwp");
    let xml = tmp("equation_bridge.hwpx");
    let returned = tmp("equation_returned.hwp");
    let options = hwp5::WriteOptions::default();
    assert!(
        hwp5::write_document_with_report(&document, &native, &options)
            .unwrap()
            .preservation
            .is_lossless()
    );
    let first = hwp5::read_document(&native).unwrap().document;
    assert_properties(&equation(&first));
    hwpx::write_document(&first, &xml).unwrap();
    let second = hwpx::read_document(&xml).unwrap().document;
    assert_properties(&equation(&second));
    assert!(
        hwp5::write_document_with_report(&second, &returned, &options)
            .unwrap()
            .preservation
            .is_lossless()
    );
    let last = hwp5::read_document(&returned).unwrap().document;
    assert_properties(&equation(&last));
    for file in [native, xml, returned] {
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn genuine_equation_controls_remain_byte_identical() {
    use hwp5::record::{RecordNode, ScanMode, scan_stream, tag};

    fn equation_records(path: &std::path::Path) -> Vec<Vec<u8>> {
        fn collect(nodes: &[RecordNode], records: &mut Vec<Vec<u8>>) {
            for node in nodes {
                if node.tag == tag::CTRL_HEADER && node.data.starts_with(b"deqe") {
                    records.push(RecordNode::serialize_forest(std::slice::from_ref(node)));
                } else {
                    collect(&node.children, records);
                }
            }
        }
        let mut container = hwp5::Hwp5Container::open(path).unwrap();
        let mut records = Vec::new();
        for section in container.body_sections() {
            let bytes = container.read_record_stream(&section).unwrap();
            let scan = scan_stream(&bytes, ScanMode::Strict).unwrap();
            collect(&scan.roots, &mut records);
        }
        records
    }

    let source = fixture("equation-ampersand-groundtruth.hwp");
    if fixture_skip::fixture_missing(&source) {
        return;
    }
    let original = equation_records(&source);
    assert_eq!(original.len(), 2);
    let document = hwp5::read_document(&source).unwrap().document;
    let output = tmp("equation_raw_identity.hwp");
    hwp5::write_document(
        &document,
        &output,
        &hwp5::WriteOptions {
            preserve_linesegs: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(equation_records(&output), original);
    std::fs::remove_file(output).unwrap();
}

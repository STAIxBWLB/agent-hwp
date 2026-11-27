//! Native chart authoring, renders, surgical preservation, and cross-format refusal.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Output};

#[path = "common/temp_path.rs"]
mod temp_path;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ahwp"))
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn entry(path: &Path, name: &str) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn compose_render_and_edit_native_hwpx_charts() {
    let dir = temp_path::TempPath::new("ahwp-native-charts", "compose");
    std::fs::create_dir_all(&*dir).unwrap();
    let json = dir.join("charts.json");
    let input = dir.join("charts.hwpx");
    let mut blocks = vec![serde_json::json!({
        "type":"paragraph", "runs":[{"type":"text", "text":"Before charts"}]
    })];
    for kind in ["column", "line", "pie", "scatter"] {
        let categories: &[&str] = if kind == "scatter" {
            &[]
        } else {
            &["One", "Two", "Three"]
        };
        blocks.push(serde_json::json!({
            "type":"chart", "width_mm":100, "height_mm":48,
            "data": {"kind":kind, "title":kind, "categories":categories,
                "series":[{"name":"Series & data", "values":[1,4,2.5], "color":"#173F73"}]}
        }));
    }
    std::fs::write(
        &json,
        serde_json::json!({
            "version":"2.0", "document":{"version":"1.0","sections":[{"blocks":blocks}]}
        })
        .to_string(),
    )
    .unwrap();
    success(
        cli()
            .arg("compose")
            .arg(&json)
            .arg("-o")
            .arg(&input)
            .output()
            .unwrap(),
    );
    success(cli().arg("validate").arg(&input).output().unwrap());
    let manifest = String::from_utf8(entry(&input, "Contents/content.hpf")).unwrap();
    let section = String::from_utf8(entry(&input, "Contents/section0.xml")).unwrap();
    for (n, tag) in [
        (1, "barChart"),
        (2, "lineChart"),
        (3, "pieChart"),
        (4, "scatterChart"),
    ] {
        assert!(manifest.contains(&format!(r#"id="chart{n}" href="Chart/chart{n}.xml""#)));
        assert!(section.contains(&format!(r#"chartIDRef="Chart/chart{n}.xml""#)));
        let xml = String::from_utf8(entry(&input, &format!("Chart/chart{n}.xml"))).unwrap();
        assert!(xml.contains(&format!("<c:{tag}>")));
        assert!(xml.contains("Series &amp; data"));
    }
    let reread = hwpx::read_document(&input).unwrap().document;
    let charts: Vec<_> = reread.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .filter_map(|control| match control {
            hwp_model::Control::Generic(generic) => generic.chart.as_deref()?.data.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(charts.len(), 4);
    for chart in &charts {
        assert_eq!(chart.series[0].color.as_deref(), Some("#173F73"));
    }
    assert_eq!(charts[1].series[0].values, [1.0, 4.0, 2.5]);

    for extension in ["svg", "pdf", "png"] {
        let rendered = dir.join(format!("chart.{extension}"));
        let report = dir.join(format!("render-{extension}.json"));
        success(
            cli()
                .arg("render")
                .arg(&input)
                .args(["--pages", "1", "--report"])
                .arg(&report)
                .arg("-o")
                .arg(&rendered)
                .output()
                .unwrap(),
        );
        let bytes = std::fs::read(rendered).unwrap();
        match extension {
            "svg" => assert!(String::from_utf8_lossy(&bytes).contains("#173f73")),
            "pdf" => assert!(bytes.starts_with(b"%PDF-")),
            _ => assert!(bytes.starts_with(&[137, 80, 78, 71])),
        }
        assert!(
            !std::fs::read_to_string(report)
                .unwrap()
                .contains("unsupported_control_omitted")
        );
    }

    let edited = dir.join("edited.hwpx");
    success(
        cli()
            .arg("edit")
            .arg(&input)
            .args(["--replace", "Before charts=>After charts", "--verify", "-o"])
            .arg(&edited)
            .output()
            .unwrap(),
    );
    let wrapper = |xml: &str| -> Vec<String> {
        xml.split("<hp:chart ")
            .skip(1)
            .map(|tail| tail.split("</hp:chart>").next().unwrap().to_owned())
            .collect()
    };
    let rewritten = dir.join("rewritten.hwpx");
    hwpx::write_document(&reread, &rewritten).unwrap();
    let rewritten_manifest = String::from_utf8(entry(&rewritten, "Contents/content.hpf")).unwrap();
    let chart_items = |xml: &str| -> Vec<String> {
        xml.split("<opf:item ")
            .filter(|item| item.starts_with("id=\"chart"))
            .map(|item| item.split("/>").next().unwrap().to_owned())
            .collect()
    };
    assert_eq!(chart_items(&manifest), chart_items(&rewritten_manifest));
    let rewritten_section = String::from_utf8(entry(&rewritten, "Contents/section0.xml")).unwrap();
    assert_eq!(wrapper(&rewritten_section), wrapper(&section));
    let edited_section = String::from_utf8(entry(&edited, "Contents/section0.xml")).unwrap();
    assert_ne!(section, edited_section);
    assert_eq!(wrapper(&edited_section), wrapper(&section));
    for n in 1..=4 {
        let part = format!("Chart/chart{n}.xml");
        assert_eq!(entry(&input, &part), entry(&edited, &part));
        assert_eq!(entry(&input, &part), entry(&rewritten, &part));
    }

    let native = dir.join("refused.hwp");
    let loss = dir.join("loss.json");
    let result = cli()
        .arg("convert")
        .arg(&input)
        .args(["--strict", "--loss-report"])
        .arg(&loss)
        .arg("-o")
        .arg(&native)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!native.exists());
    assert!(
        std::fs::read_to_string(loss)
            .unwrap()
            .contains("chart_control_unrepresentable")
    );
}

fn repackage(input: &Path, output: &Path, replacements: &[(&str, &[u8])]) {
    use std::io::Write as _;
    let mut source = zip::ZipArchive::new(std::fs::File::open(input).unwrap()).unwrap();
    let mut out = zip::ZipWriter::new(std::fs::File::create(output).unwrap());
    for i in 0..source.len() {
        let file = source.by_index(i).unwrap();
        if !replacements
            .iter()
            .any(|(name, _)| *name == file.name().unwrap())
        {
            out.raw_copy_file(file).unwrap();
        }
    }
    for (name, bytes) in replacements {
        out.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        out.write_all(bytes).unwrap();
    }
    out.finish().unwrap();
}

#[test]
fn foreign_chart_in_cell_and_href_alias_survive_both_rewrite_paths() {
    use hwp_model::Control;

    let dir = temp_path::TempPath::new("ahwp-native-charts", "foreign-cell");
    std::fs::create_dir_all(&*dir).unwrap();
    let spec = dir.join("spec.json");
    let authored = dir.join("authored.hwpx");
    let chart = serde_json::json!({
        "type":"chart", "width_mm":60, "height_mm":45,
        "data":{"kind":"column", "categories":["Q1", "Q2"], "series":[{"name":"S","values":[1,2]}]}
    });
    std::fs::write(
        &spec,
        serde_json::json!({
            "version":"2.0", "document":{"version":"1.0","sections":[{"blocks":[
                {"type":"paragraph","runs":[{"type":"text","text":"Before charts"}]},
                {"type":"table","width_mm":90,"columns":[{"width_mm":90}],
                 "rows":[{"cells":[{"blocks":[chart.clone()]}]}]},
                chart
            ]}]}
        })
        .to_string(),
    )
    .unwrap();
    success(
        cli()
            .arg("compose")
            .arg(&spec)
            .arg("-o")
            .arg(&authored)
            .output()
            .unwrap(),
    );

    let part_name = "Chart/chart1.xml";
    let rel_name = "Chart/_rels/chart1.xml.rels";
    let part = String::from_utf8(entry(&authored, part_name))
        .unwrap()
        .replace(
            "<c:numLit>",
            "<c:numRef><c:f>Sheet1!$B$1:$B$2</c:f><c:numCache>",
        )
        .replace("</c:numLit>", "</c:numCache></c:numRef>")
        .replace(
            "</c:chartSpace>",
            "<c:extLst><c:ext uri=\"foreign\"/></c:extLst></c:chartSpace>",
        );
    let rels =
        br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;
    let ole_name = "BinData/ole1.ole";
    let ole = b"opaque synthetic fallback";
    let manifest = String::from_utf8(entry(&authored, "Contents/content.hpf")).unwrap();
    let (before_chart, chart_item) = manifest.split_once(r#"<opf:item id="chart1""#).unwrap();
    let (_, after_chart) = chart_item.split_once("/>").unwrap();
    let manifest = format!(
        r#"{before_chart}<opf:item id="ole1" href="{ole_name}" media-type="application/ole" isEmbeded="0"/>{after_chart}"#
    )
    .replace(
        r#"id="chart2" href="Chart/chart2.xml""#,
        r#"id="chart2" href="Chart/chart1.xml""#,
    );
    let section = String::from_utf8(entry(&authored, "Contents/section0.xml"))
        .unwrap()
        .replace(r#"chartIDRef="Chart/chart2.xml""#, r#"chartIDRef="chart2""#)
        .replace(r#"chartIDRef="chart1""#, r#"chartIDRef="Chart/chart1.xml""#);
    let chart_start = section.find("<hp:chart ").unwrap();
    let chart_end = section[chart_start..].find("</hp:chart>").unwrap() + chart_start;
    let switch = format!(
        concat!(
            r#"<hp:switch><hp:case hp:required-namespace="http://www.hancom.co.kr/hwpml/2016/ooxmlchart">"#,
            r#"{}<hp:outMargin/></hp:chart></hp:case>"#,
            "\n  ",
            r#"<hp:default><hp:ole binaryItemIDRef="ole1" /></hp:default></hp:switch>"#,
        ),
        &section[chart_start..chart_end],
    );
    let section = format!(
        "{}{switch}{}",
        &section[..chart_start],
        &section[chart_end + "</hp:chart>".len()..],
    );
    let foreign = dir.join("foreign.hwpx");
    repackage(
        &authored,
        &foreign,
        &[
            (part_name, part.as_bytes()),
            (rel_name, rels),
            (ole_name, ole),
            ("Contents/content.hpf", manifest.as_bytes()),
            ("Contents/section0.xml", section.as_bytes()),
        ],
    );
    let read = hwpx::read_document(&foreign).unwrap().document;
    fn chart_data(paragraphs: &[hwp_model::Paragraph]) -> &std::sync::Arc<hwp_model::ChartData> {
        paragraphs
            .iter()
            .flat_map(|p| &p.controls)
            .find_map(|ctrl| match ctrl {
                Control::Generic(g) => g.chart.as_deref()?.data.as_ref(),
                _ => None,
            })
            .unwrap()
    }
    let body = &read.sections[0].paragraphs;
    let table = body
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|ctrl| match ctrl {
            Control::Table(table) => Some(table),
            _ => None,
        })
        .unwrap();
    let cell_chart = chart_data(&table.cells[0].paragraphs);
    assert_eq!(cell_chart.series[0].values, [1.0, 2.0]);
    assert!(std::sync::Arc::ptr_eq(cell_chart, chart_data(body)));
    let structure = hwpx::read_structure(&foreign).unwrap().document;
    let structure_body = &structure.sections[0].paragraphs;
    let structure_cell = structure_body
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|ctrl| match ctrl {
            Control::Table(table) => Some(&table.cells[0].paragraphs),
            _ => None,
        })
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        chart_data(structure_body),
        chart_data(structure_cell)
    ));
    let svg = dir.join("foreign.svg");
    success(
        cli()
            .arg("render")
            .arg(&foreign)
            .args(["--pages", "1", "-o"])
            .arg(&svg)
            .output()
            .unwrap(),
    );
    assert!(
        std::fs::read_to_string(&svg)
            .unwrap()
            .matches("#2871b6")
            .count()
            >= 4
    );

    let complete = dir.join("full.hwpx");
    hwpx::write_document(&read, &complete).unwrap();
    let edited = dir.join("edited.hwpx");
    success(
        cli()
            .arg("edit")
            .arg(&foreign)
            .args(["--replace", "Before charts=>After charts", "--verify", "-o"])
            .arg(&edited)
            .output()
            .unwrap(),
    );
    for output in [&complete, &edited] {
        assert_eq!(entry(output, part_name), part.as_bytes());
        assert_eq!(entry(output, rel_name), rels);
        let output_section = String::from_utf8(entry(output, "Contents/section0.xml")).unwrap();
        assert_eq!(entry(output, ole_name), ole);
        assert!(output_section.contains(&switch));
        assert!(output_section.contains(r#"chartIDRef="chart2""#));
    }

    let unmanifested = dir.join("unmanifested.hwpx");
    let (before_alias, alias_item) = manifest.split_once(r#"<opf:item id="chart2""#).unwrap();
    let (_, after_alias) = alias_item.split_once("/>").unwrap();
    let manifest_without_charts = format!("{before_alias}{after_alias}");
    let section_path_only =
        section.replace(r#"chartIDRef="chart2""#, r#"chartIDRef="Chart/chart1.xml""#);
    repackage(
        &foreign,
        &unmanifested,
        &[
            ("Contents/content.hpf", manifest_without_charts.as_bytes()),
            ("Contents/section0.xml", section_path_only.as_bytes()),
        ],
    );
    for read in [
        hwpx::read_document(&unmanifested),
        hwpx::read_structure(&unmanifested),
    ] {
        let doc = read.unwrap().document;
        assert_eq!(
            chart_data(&doc.sections[0].paragraphs).series[0].values,
            [1.0, 2.0]
        );
    }

    let broken = dir.join("malformed.hwpx");
    repackage(&foreign, &broken, &[(part_name, b"<c:chartSpace><broken")]);
    let broken_doc = hwpx::read_document(&broken).unwrap().document;
    let opaque = broken_doc.sections[0]
        .paragraphs
        .iter()
        .flat_map(|p| &p.controls)
        .find_map(|ctrl| match ctrl {
            Control::Generic(g) => g.chart.as_deref(),
            _ => None,
        })
        .unwrap();
    assert!(opaque.data.is_none());
    let rewritten = dir.join("opaque.hwpx");
    hwpx::write_document(&broken_doc, &rewritten).unwrap();
    assert_eq!(entry(&rewritten, part_name), b"<c:chartSpace><broken");
    let report = dir.join("broken-render.json");
    success(
        cli()
            .arg("render")
            .arg(&broken)
            .arg("--report")
            .arg(&report)
            .arg("-o")
            .arg(dir.join("broken.svg"))
            .output()
            .unwrap(),
    );
    assert!(
        std::fs::read_to_string(report)
            .unwrap()
            .contains("unsupported_control_omitted")
    );
}

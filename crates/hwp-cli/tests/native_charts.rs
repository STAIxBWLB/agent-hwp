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
    for kind in ["column", "line", "pie"] {
        blocks.push(serde_json::json!({
            "type":"chart", "width_mm":100, "height_mm":48,
            "data": {"kind":kind, "title":kind, "categories":["One", "Two", "Three"],
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
    for (n, tag) in [(1, "barChart"), (2, "lineChart"), (3, "pieChart")] {
        assert!(manifest.contains(&format!(r#"id="chart{n}" href="Chart/chart{n}.xml""#)));
        assert!(section.contains(&format!(r#"chartIDRef="chart{n}""#)));
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
    assert_eq!(charts.len(), 3);
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
    let rewritten_section = String::from_utf8(entry(&rewritten, "Contents/section0.xml")).unwrap();
    assert_eq!(wrapper(&rewritten_section), wrapper(&section));
    let edited_section = String::from_utf8(entry(&edited, "Contents/section0.xml")).unwrap();
    assert_ne!(section, edited_section);
    assert_eq!(wrapper(&edited_section), wrapper(&section));
    for n in 1..=3 {
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

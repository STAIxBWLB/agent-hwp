//! DrawingML chart parts (ECMA-376 Part 1, §21.2).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hwp_model::{ChartData, ChartKind, ChartLegend, ChartSeries};
use quick_xml::{Reader, events::Event};

pub(crate) const MIME: &str = "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
pub(crate) const MAX_PART: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct Element {
    name: String,
    attrs: BTreeMap<String, String>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn get(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.name == name)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    fn text_runs(&self, out: &mut String) {
        if self.name == "t" {
            out.push_str(&self.text);
        }
        for child in &self.children {
            child.text_runs(out);
        }
    }
}

fn tree(xml: &[u8]) -> Option<Element> {
    if xml.len() > MAX_PART {
        return None;
    }
    let mut reader = Reader::from_reader(xml);
    let mut stack = vec![Element::default()];
    loop {
        match reader.read_event().ok()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let (tag, empty) = match event {
                    Event::Start(tag) => (tag, false),
                    Event::Empty(tag) => (tag, true),
                    _ => unreachable!(),
                };
                if stack.len() >= 48 {
                    return None;
                }
                let name = tag.local_name();
                let node = Element {
                    name: name.as_ref().to_owned(),
                    attrs: tag
                        .attributes()
                        .map(|entry| {
                            let entry = entry.ok()?;
                            Some((
                                entry.key.local_name().as_ref().to_owned(),
                                quick_xml::escape::unescape(&entry.value).ok()?.into_owned(),
                            ))
                        })
                        .collect::<Option<_>>()?,
                    ..Element::default()
                };
                if empty {
                    stack.last_mut()?.children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                let child = stack.pop()?;
                stack.last_mut()?.children.push(child);
            }
            Event::Text(text) => stack.last_mut()?.text.push_str(text.as_ref()),
            Event::CData(text) => stack.last_mut()?.text.push_str(text.as_ref()),
            Event::GeneralRef(reference) => {
                stack
                    .last_mut()?
                    .text
                    .push(crate::read::xml::resolve_entity(&reference)?);
            }
            Event::DocType(_) => return None,
            Event::Eof => {
                return (stack.len() == 1 && stack[0].children.len() == 1)
                    .then(|| stack[0].children.pop())
                    .flatten();
            }
            _ => (),
        }
    }
}

fn label(parent: &Element, key: &str) -> String {
    let mut out = String::new();
    if let Some(node) = parent.get(key) {
        node.text_runs(&mut out);
    }
    out
}

fn cache(node: &Element) -> Option<&Element> {
    for key in ["strLit", "numLit", "strCache", "numCache"] {
        if let Some(data) = node.get(key) {
            return Some(data);
        }
    }
    let reference = node.get("strRef").or_else(|| node.get("numRef"))?;
    reference
        .get("strCache")
        .or_else(|| reference.get("numCache"))
}

fn points(node: &Element) -> Option<Vec<String>> {
    let mut values = BTreeMap::new();
    for pt in &cache(node)?.children {
        if pt.name == "pt" {
            let i = pt.value("idx")?.parse::<usize>().ok()?;
            if i >= ChartData::MAX_POINTS || values.insert(i, pt.get("v")?.text.clone()).is_some() {
                return None;
            }
        }
    }
    if !values.keys().copied().eq(0..values.len()) {
        return None;
    }
    Some(values.into_values().collect())
}

fn numbers(node: &Element) -> Option<Vec<f64>> {
    points(node)?
        .iter()
        .map(|value| value.trim().parse().ok())
        .collect()
}

pub(crate) fn parse(xml: &[u8]) -> Option<ChartData> {
    let root = tree(xml)?;
    if root.name != "chartSpace" {
        return None;
    }
    let chart = root.get("chart")?;
    let plot = chart.get("plotArea")?;
    let mut plots = plot
        .children
        .iter()
        .filter(|node| node.name.ends_with("Chart"));
    let series_root = plots.next()?;
    if plots.next().is_some() {
        return None;
    }
    let kind = match series_root.name.as_str() {
        "barChart" => match series_root.get("barDir")?.value("val")? {
            "bar" => ChartKind::Bar,
            "col" => ChartKind::Column,
            _ => return None,
        },
        "lineChart" => ChartKind::Line,
        "pieChart" => ChartKind::Pie,
        "doughnutChart" => ChartKind::Doughnut,
        "areaChart" => ChartKind::Area,
        "scatterChart" => ChartKind::Scatter,
        _ => return None,
    };
    let mut categories = Vec::new();
    let mut series = Vec::new();
    for node in series_root
        .children
        .iter()
        .filter(|node| node.name == "ser")
    {
        if series.len() == ChartData::MAX_SERIES {
            return None;
        }
        let (values, x_values) = if kind == ChartKind::Scatter {
            (numbers(node.get("yVal")?)?, numbers(node.get("xVal")?)?)
        } else {
            if let Some(cat) = node.get("cat") {
                let labels = points(cat)?;
                if !categories.is_empty() && categories != labels {
                    return None;
                }
                categories = labels;
            }
            (numbers(node.get("val")?)?, Vec::new())
        };
        let name = node
            .get("tx")
            .and_then(|tx| {
                tx.get("v")
                    .map(|v| v.text.clone())
                    .or_else(|| points(tx)?.into_iter().next())
            })
            .unwrap_or_default();
        let color = node.get("spPr").and_then(|style| {
            let fill = style
                .get("solidFill")
                .or_else(|| style.get("ln")?.get("solidFill"))?;
            Some(format!("#{}", fill.get("srgbClr")?.value("val")?))
        });
        series.push(ChartSeries {
            name,
            values,
            x_values,
            color,
        });
    }
    let legend = match chart
        .get("legend")
        .and_then(|legend| legend.get("legendPos"))
        .and_then(|position| position.value("val"))
    {
        Some("t") => ChartLegend::Top,
        Some("l") => ChartLegend::Left,
        Some("r" | "tr") => ChartLegend::Right,
        Some(_) => ChartLegend::Bottom,
        None if chart.get("legend").is_some() => ChartLegend::Right,
        None => ChartLegend::None,
    };
    let mut axes = plot
        .children
        .iter()
        .filter(|child| matches!(child.name.as_str(), "catAx" | "valAx"));
    let result = ChartData {
        kind,
        title: label(chart, "title"),
        categories,
        series,
        legend,
        x_label: axes
            .next()
            .map_or_else(String::new, |axis| label(axis, "title")),
        y_label: axes
            .next()
            .map_or_else(String::new, |axis| label(axis, "title")),
    };
    result.validate().ok().map(|()| result)
}

fn title(out: &mut String, text: &str) {
    if !text.is_empty() {
        let text = crate::write::templates::esc(text);
        let _ = write!(
            out,
            "<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>"
        );
    }
}

fn write_points(out: &mut String, tag: &str, numeric: bool, values: &[String]) {
    let lit = if numeric { "numLit" } else { "strLit" };
    let _ = write!(out, "<c:{tag}><c:{lit}>");
    if numeric {
        out.push_str("<c:formatCode>General</c:formatCode>");
    }
    let _ = write!(out, "<c:ptCount val=\"{}\"/>", values.len());
    for (i, value) in values.iter().enumerate() {
        let _ = write!(
            out,
            "<c:pt idx=\"{i}\"><c:v>{}</c:v></c:pt>",
            crate::write::templates::esc(value)
        );
    }
    let _ = write!(out, "</c:{lit}></c:{tag}>");
}

fn axis(out: &mut String, tag: &str, id: u32, cross: u32, pos: &str, text: &str) {
    let _ = write!(
        out,
        "<c:{tag}><c:axId val=\"{id}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{pos}\"/>"
    );
    if pos == "l" {
        out.push_str("<c:majorGridlines/>");
    }
    title(out, text);
    let _ = write!(
        out,
        "<c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"{cross}\"/><c:crosses val=\"autoZero\"/>"
    );
    if tag == "catAx" {
        out.push_str("<c:auto val=\"1\"/><c:lblAlgn val=\"ctr\"/><c:lblOffset val=\"100\"/>");
    } else {
        out.push_str("<c:crossBetween val=\"between\"/>");
    }
    let _ = write!(out, "</c:{tag}>");
}

pub(crate) fn write(data: &ChartData) -> Vec<u8> {
    use ChartKind::*;
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart>"#,
    );
    title(&mut out, &data.title);
    out.push_str("<c:plotArea><c:layout/>");
    let (tag, prelude) = match data.kind {
        Column => (
            "barChart",
            r#"<c:barDir val="col"/><c:grouping val="clustered"/>"#,
        ),
        Bar => (
            "barChart",
            r#"<c:barDir val="bar"/><c:grouping val="clustered"/>"#,
        ),
        Line => ("lineChart", r#"<c:grouping val="standard"/>"#),
        Area => ("areaChart", r#"<c:grouping val="standard"/>"#),
        Pie => ("pieChart", r#"<c:varyColors val="1"/>"#),
        Doughnut => ("doughnutChart", r#"<c:varyColors val="1"/>"#),
        Scatter => ("scatterChart", r#"<c:scatterStyle val="marker"/>"#),
    };
    let _ = write!(out, "<c:{tag}>{prelude}");
    for (index, series) in data.series.iter().enumerate() {
        let name = crate::write::templates::esc(&series.name);
        let _ = write!(
            out,
            "<c:ser><c:idx val=\"{index}\"/><c:order val=\"{index}\"/><c:tx><c:v>{name}</c:v></c:tx>"
        );
        if let Some(color) = series
            .color
            .as_deref()
            .and_then(|color| color.strip_prefix('#'))
        {
            if data.kind == Line {
                let _ = write!(
                    out,
                    "<c:spPr><a:ln><a:solidFill><a:srgbClr val=\"{color}\"/></a:solidFill></a:ln></c:spPr>"
                );
            } else {
                let _ = write!(
                    out,
                    "<c:spPr><a:solidFill><a:srgbClr val=\"{color}\"/></a:solidFill></c:spPr>"
                );
            }
        }
        if matches!(data.kind, Pie | Doughnut) {
            // Match the preview's slice colors instead of applying one fill to all wedges.
            for i in 0..series.values.len() {
                let c = data.display_color(i);
                let _ = write!(
                    out,
                    r#"<c:dPt><c:idx val="{i}"/><c:spPr><a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill></c:spPr></c:dPt>"#,
                    c & 255,
                    (c >> 8) & 255,
                    (c >> 16) & 255
                );
            }
        }
        let values = series
            .values
            .iter()
            .map(|value| format!("{value:e}"))
            .collect::<Vec<_>>();
        if data.kind == Scatter {
            let xs = if series.x_values.is_empty() {
                (1..=series.values.len())
                    .map(|value| format!("{:e}", value as f64))
                    .collect()
            } else {
                series
                    .x_values
                    .iter()
                    .map(|value| format!("{value:e}"))
                    .collect::<Vec<_>>()
            };
            write_points(&mut out, "xVal", true, &xs);
            write_points(&mut out, "yVal", true, &values);
        } else {
            if !data.categories.is_empty() {
                write_points(&mut out, "cat", false, &data.categories);
            }
            write_points(&mut out, "val", true, &values);
        }
        out.push_str("</c:ser>");
    }
    if !matches!(data.kind, Pie | Doughnut) {
        out.push_str(r#"<c:axId val="1"/><c:axId val="2"/>"#);
    } else if data.kind == Doughnut {
        out.push_str(r#"<c:holeSize val="55"/>"#);
    }
    let _ = write!(out, "</c:{tag}>");
    if !matches!(data.kind, Pie | Doughnut) {
        let (x, y) = if data.kind == Bar {
            ("l", "b")
        } else {
            ("b", "l")
        };
        let x_tag = if data.kind == Scatter {
            "valAx"
        } else {
            "catAx"
        };
        axis(&mut out, x_tag, 1, 2, x, &data.x_label);
        axis(&mut out, "valAx", 2, 1, y, &data.y_label);
    }
    out.push_str("</c:plotArea>");
    let pos = match data.legend {
        ChartLegend::Top => "t",
        ChartLegend::Bottom => "b",
        ChartLegend::Left => "l",
        ChartLegend::Right => "r",
        ChartLegend::None => "",
    };
    if !pos.is_empty() {
        let _ = write!(
            out,
            "<c:legend><c:legendPos val=\"{pos}\"/><c:overlay val=\"0\"/></c:legend>"
        );
    }
    out.push_str(
        "<c:plotVisOnly val=\"1\"/><c:dispBlanksAs val=\"gap\"/></c:chart></c:chartSpace>",
    );
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_caches_and_excess_depth_are_unrepresentable() {
        let series = r#"<c:ser><c:val><c:numLit><c:pt idx="0"><c:v>1</c:v></c:pt></c:numLit></c:val></c:ser>"#;
        let make = |serial: &str| {
            format!(
                r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:lineChart>{serial}</c:lineChart></c:plotArea></c:chart></c:chartSpace>"#
            )
        };
        assert!(parse(make(series).as_bytes()).is_some());
        assert!(parse(make(&series.repeat(33)).as_bytes()).is_none());
        assert!(parse(make(&series.replace("idx=\"0\"", "idx=\"1\"")).as_bytes()).is_none());
        assert!(
            parse(
                make(&series.replace(
                    "</c:numLit>",
                    r#"<c:pt idx="0"><c:v>2</c:v></c:pt></c:numLit>"#
                ))
                .as_bytes()
            )
            .is_none()
        );
        assert!(parse(make(&series.replace(">1<", ">NaN<")).as_bytes()).is_none());
        let extension = |depth: usize| {
            make(series).replace(
                "</c:chartSpace>",
                &format!(
                    "<c:extLst>{}{}</c:extLst></c:chartSpace>",
                    "<e>".repeat(depth),
                    "</e>".repeat(depth)
                ),
            )
        };
        assert!(parse(extension(2).as_bytes()).is_some());
        assert!(parse(extension(48).as_bytes()).is_none());
    }

    #[test]
    fn all_supported_parts_reparse() {
        for kind in [
            ChartKind::Bar,
            ChartKind::Column,
            ChartKind::Line,
            ChartKind::Pie,
            ChartKind::Doughnut,
            ChartKind::Area,
            ChartKind::Scatter,
        ] {
            let scatter = kind == ChartKind::Scatter;
            let data = ChartData {
                kind,
                title: "A & B".into(),
                categories: if scatter {
                    Vec::new()
                } else {
                    vec!["One".into(), "Two".into()]
                },
                series: vec![ChartSeries {
                    name: "Series".into(),
                    values: vec![2.0, 3.5],
                    x_values: if scatter { vec![-1.0, 2.0] } else { Vec::new() },
                    color: Some("#123456".into()),
                }],
                legend: ChartLegend::Right,
                x_label: String::new(),
                y_label: String::new(),
            };
            assert_eq!(parse(&write(&data)), Some(data));
        }
    }
}

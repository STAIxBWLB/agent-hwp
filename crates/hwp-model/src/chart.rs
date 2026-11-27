//! Bounded semantic view of an HWPX DrawingML chart.

use serde::{Deserialize, Serialize};

use crate::GsoPlacement;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chart {
    pub placement: GsoPlacement,
    /// Original manifest reference. Raw package bytes and wrapper are retained separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Absent when an original chart has an unsupported or unreadable chart part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<std::sync::Arc<ChartData>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Column,
    Bar,
    Line,
    Pie,
    Doughnut,
    Area,
    Scatter,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartLegend {
    None,
    Top,
    #[default]
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub x_values: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartData {
    pub kind: ChartKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub categories: Vec<String>,
    pub series: Vec<ChartSeries>,
    #[serde(default)]
    pub legend: ChartLegend,
    #[serde(default)]
    pub x_label: String,
    #[serde(default)]
    pub y_label: String,
}

impl ChartData {
    pub const MAX_SERIES: usize = 32;
    pub const MAX_POINTS: usize = 512;
    pub const MAX_LABEL_CHARS: usize = 256;

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.series.is_empty() || self.series.len() > Self::MAX_SERIES {
            return Err("chart requires 1 to 32 series");
        }
        if self.kind == ChartKind::Scatter && !self.categories.is_empty() {
            return Err("scatter charts use numeric x_values instead of category labels");
        }
        let circular = matches!(self.kind, ChartKind::Pie | ChartKind::Doughnut);
        if circular && (!self.x_label.is_empty() || !self.y_label.is_empty()) {
            return Err("pie and doughnut charts have no axes");
        }
        if circular && self.series.len() != 1 {
            return Err("pie and doughnut charts require exactly one series");
        }
        if self.categories.len() > Self::MAX_POINTS
            || [&self.title, &self.x_label, &self.y_label]
                .into_iter()
                .chain(&self.categories)
                .any(|label| label.chars().count() > Self::MAX_LABEL_CHARS)
        {
            return Err("chart category or label limit exceeded");
        }
        for series in &self.series {
            if series.name.chars().count() > Self::MAX_LABEL_CHARS
                || series.values.is_empty()
                || series.values.len() > Self::MAX_POINTS
                || (!self.categories.is_empty() && series.values.len() != self.categories.len())
            {
                return Err("chart points must match the categories and respect the size limits");
            }
            if !series.x_values.is_empty()
                && (self.kind != ChartKind::Scatter || series.x_values.len() != series.values.len())
            {
                return Err("x_values must match the values of a scatter series");
            }
            if series
                .values
                .iter()
                .chain(&series.x_values)
                .any(|v| !v.is_finite() || v.abs() > 1e100)
            {
                return Err("chart values must be finite and within ±1e100");
            }
            if circular
                && (series.values.iter().any(|v| *v < 0.0)
                    || !series.values.iter().any(|v| *v > 0.0))
            {
                return Err(
                    "pie and doughnut charts require nonnegative values and a positive total",
                );
            }
            if series.color.as_ref().is_some_and(|color| {
                !color.strip_prefix('#').is_some_and(|digits| {
                    digits.len() == 6 && digits.bytes().all(|ch| ch.is_ascii_hexdigit())
                })
            }) {
                return Err("chart colors use #RRGGBB");
            }
        }
        Ok(())
    }
}

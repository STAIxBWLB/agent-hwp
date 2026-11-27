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

    /// Resolve an authored series or slice color to HWP COLORREF.
    pub fn display_color(&self, index: usize) -> u32 {
        const PALETTE: [u32; 8] = [
            0xb67128, 0x3984db, 0x759047, 0xa45c87, 0x7a8e16, 0xa66e45, 0x5a7cc5, 0x98744d,
        ];
        self.series
            .get(index)
            .and_then(|series| series.color.as_deref())
            .and_then(|value| u32::from_str_radix(value.strip_prefix('#')?, 16).ok())
            .map_or(PALETTE[index % PALETTE.len()], |rgb| {
                ((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16)
            })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.series.is_empty() || self.series.len() > Self::MAX_SERIES {
            return Err("chart requires 1 to 32 series".into());
        }
        if self.kind == ChartKind::Scatter && !self.categories.is_empty() {
            return Err("scatter charts use numeric x_values instead of category labels".into());
        }
        let circular = matches!(self.kind, ChartKind::Pie | ChartKind::Doughnut);
        if circular && (!self.x_label.is_empty() || !self.y_label.is_empty()) {
            return Err("pie and doughnut charts have no axes".into());
        }
        if circular && self.series.len() != 1 {
            return Err("pie and doughnut charts require exactly one series".into());
        }
        if self.categories.len() > Self::MAX_POINTS {
            return Err(format!(
                "categories exceeds the {}-label limit",
                Self::MAX_POINTS
            ));
        }
        for (field, value) in [
            ("title", &self.title),
            ("x_label", &self.x_label),
            ("y_label", &self.y_label),
        ] {
            if value.chars().count() > Self::MAX_LABEL_CHARS {
                return Err(format!(
                    "{field} exceeds the {}-character limit",
                    Self::MAX_LABEL_CHARS
                ));
            }
        }
        for (index, label) in self.categories.iter().enumerate() {
            if label.chars().count() > Self::MAX_LABEL_CHARS {
                return Err(format!(
                    "categories[{index}] exceeds the {}-character limit",
                    Self::MAX_LABEL_CHARS
                ));
            }
        }
        for (index, series) in self.series.iter().enumerate() {
            if series.name.chars().count() > Self::MAX_LABEL_CHARS {
                return Err(format!(
                    "series[{index}].name exceeds the {}-character limit",
                    Self::MAX_LABEL_CHARS
                ));
            }
            if series.values.is_empty() || series.values.len() > Self::MAX_POINTS {
                return Err(format!(
                    "series[{index}].values requires 1 to {} points",
                    Self::MAX_POINTS
                ));
            }
            if !self.categories.is_empty() && series.values.len() != self.categories.len() {
                return Err(format!(
                    "series[{index}].values has {} points but categories has {} labels",
                    series.values.len(),
                    self.categories.len()
                ));
            }
            if !series.x_values.is_empty()
                && (self.kind != ChartKind::Scatter || series.x_values.len() != series.values.len())
            {
                return Err(format!(
                    "series[{index}].x_values must match the values of a scatter series"
                ));
            }
            if series
                .values
                .iter()
                .chain(&series.x_values)
                .any(|v| !v.is_finite() || v.abs() > 1e100)
            {
                return Err(format!(
                    "series[{index}] values must be finite and within ±1e100"
                ));
            }
            if circular
                && (series.values.iter().any(|v| *v < 0.0)
                    || !series.values.iter().any(|v| *v > 0.0))
            {
                return Err(format!(
                    "series[{index}] requires nonnegative values and a positive total"
                ));
            }
            if series.color.as_ref().is_some_and(|color| {
                !color.strip_prefix('#').is_some_and(|digits| {
                    digits.len() == 6 && digits.bytes().all(|ch| ch.is_ascii_hexdigit())
                })
            }) {
                return Err(format!("series[{index}].color must use #RRGGBB"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_values_identify_the_series_and_constraint() {
        let mut data = ChartData {
            kind: ChartKind::Column,
            title: String::new(),
            categories: vec!["A".into(), "B".into()],
            series: vec![ChartSeries {
                name: "Series".into(),
                values: vec![1.0, 2.0],
                x_values: Vec::new(),
                color: None,
            }],
            legend: ChartLegend::None,
            x_label: String::new(),
            y_label: String::new(),
        };
        for (size, expected) in [
            (513, "series[0].values requires 1 to 512 points"),
            (
                1,
                "series[0].values has 1 points but categories has 2 labels",
            ),
        ] {
            data.series[0].values = vec![1.0; size];
            assert_eq!(data.validate().unwrap_err(), expected);
        }
    }
}

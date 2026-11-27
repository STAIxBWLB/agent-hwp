//! Native 2D chart emission for the shared display list.

use hwp_model::{ChartData, ChartKind, ChartLegend, Document};

use crate::{
    display::{Fill, FillRule, Item, PageList, PathCmd, Stroke},
    fonts::FontStore,
    issues::RenderIssueAccumulator,
    shape::shape_plain,
};

const PALETTE: [u32; 8] = [
    0xb67128, 0x3984db, 0x759047, 0xa45c87, 0x7a8e16, 0xa66e45, 0x5a7cc5, 0x98744d,
];
const INK: u32 = 0x302b28;
const GRID: u32 = 0xdfd9d4;

#[derive(Clone, Copy)]
struct Box2 {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Canvas<'a> {
    page: &'a mut PageList,
    fonts: &'a mut FontStore,
    doc: &'a Document,
    issues: &'a mut RenderIssueAccumulator,
}

impl Canvas<'_> {
    fn rect(&mut self, rect: Box2, fill: u32) {
        if rect.w > 0.0 && rect.h > 0.0 && self.issues.charge_display_items(1) {
            self.page.items.push(Item::Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
                fill,
            });
        }
    }

    fn path(&mut self, points: &[(f32, f32)], color: u32, fill: bool) {
        if points.is_empty() || !self.issues.charge_display_items(1) {
            return;
        }
        let mut commands: Vec<_> = points
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| {
                if i == 0 {
                    PathCmd::MoveTo(x, y)
                } else {
                    PathCmd::LineTo(x, y)
                }
            })
            .collect();
        if fill {
            commands.push(PathCmd::Close);
        }
        self.page.items.push(Item::Path {
            commands,
            fill: fill.then_some(Fill::Solid(color)),
            stroke: (!fill).then_some(Stroke::solid(color, 1.1)),
            rule: FillRule::NonZero,
        });
    }

    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, centered: bool, limit: f32) {
        if text.is_empty() || limit <= 0.0 {
            return;
        }
        let max_chars = ((limit / size) * 1.55).floor().max(1.0) as usize;
        let text: String = text.chars().take(max_chars).collect();
        if let Some(run) = shape_plain(self.fonts, self.doc, &text, size, INK, false) {
            let x = if centered { x - run.width_pt / 2.0 } else { x };
            crate::layout::push_run(self.page, x, y, run, &[], self.issues);
        }
    }
}

fn color(data: &ChartData, index: usize) -> u32 {
    data.series
        .get(index)
        .and_then(|series| series.color.as_deref())
        .and_then(|value| u32::from_str_radix(value.strip_prefix('#')?, 16).ok())
        .map_or(PALETTE[index % PALETTE.len()], |rgb| {
            ((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16)
        })
}

pub(crate) fn draw(
    doc: &Document,
    fonts: &mut FontStore,
    page: &mut PageList,
    issues: &mut RenderIssueAccumulator,
    data: &ChartData,
    [x, y, w, h]: [f32; 4],
) {
    let mut canvas = Canvas {
        page,
        fonts,
        doc,
        issues,
    };
    let frame = Box2 { x, y, w, h };
    canvas.rect(frame, 0x00ffffff);
    let circular = matches!(data.kind, ChartKind::Pie | ChartKind::Doughnut);
    let mut plot = Box2 {
        x: x + 37.0,
        y: y + 12.0,
        w: (w - 49.0).max(1.0),
        h: (h - 39.0).max(1.0),
    };
    if !data.title.is_empty() {
        canvas.text(&data.title, x + w / 2.0, y + 15.0, 11.0, true, w - 12.0);
        plot.y += 14.0;
        plot.h -= 14.0;
    }
    if data.legend != ChartLegend::None {
        legend(&mut canvas, data, frame, &mut plot, circular);
    }
    if plot.w < 8.0 || plot.h < 8.0 {
        return;
    }
    if circular {
        slices(&mut canvas, data, plot);
    } else {
        axes(&mut canvas, data, plot);
    }
}

fn legend(canvas: &mut Canvas<'_>, data: &ChartData, frame: Box2, plot: &mut Box2, circular: bool) {
    let horizontal = matches!(data.legend, ChartLegend::Top | ChartLegend::Bottom);
    let side = (frame.w * 0.22).min(85.0);
    let (x, y) = match data.legend {
        ChartLegend::Top => {
            let y = plot.y + 3.0;
            plot.y += 14.0;
            plot.h -= 14.0;
            (plot.x, y)
        }
        ChartLegend::Bottom => {
            plot.h -= 14.0;
            (plot.x, frame.y + frame.h - 6.0)
        }
        ChartLegend::Left => {
            plot.x += side;
            plot.w -= side;
            (frame.x + 4.0, plot.y + 8.0)
        }
        ChartLegend::Right => {
            plot.w -= side;
            (plot.x + plot.w + 7.0, plot.y + 8.0)
        }
        ChartLegend::None => return,
    };
    let limit = if horizontal {
        (plot.w / 55.0).floor()
    } else {
        (plot.h / 12.0).floor()
    }
    .max(1.0) as usize;
    let labels = if circular {
        data.categories
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    } else {
        data.series
            .iter()
            .map(|series| series.name.as_str())
            .collect()
    };
    let spacing = if horizontal {
        plot.w / labels.len().min(limit).max(1) as f32
    } else {
        0.0
    };
    for (i, label) in labels.into_iter().take(limit).enumerate() {
        let xx = x + spacing * i as f32;
        let yy = y + if horizontal { 0.0 } else { 12.0 * i as f32 };
        canvas.rect(
            Box2 {
                x: xx,
                y: yy - 6.0,
                w: 6.0,
                h: 6.0,
            },
            color(data, i),
        );
        canvas.text(
            label,
            xx + 9.0,
            yy,
            8.0,
            false,
            if horizontal {
                spacing - 10.0
            } else {
                side - 12.0
            },
        );
    }
}

fn slices(canvas: &mut Canvas<'_>, data: &ChartData, plot: Box2) {
    let values = &data.series[0].values;
    let total: f64 = values.iter().sum();
    let center = (plot.x + plot.w / 2.0, plot.y + plot.h / 2.0);
    let radius = plot.w.min(plot.h) * 0.46;
    let mut angle = -std::f32::consts::FRAC_PI_2;
    let hole = if data.kind == ChartKind::Doughnut {
        0.55
    } else {
        0.0
    };
    let point = |a: f32, r| {
        (
            center.0 + a.cos() * radius * r,
            center.1 + a.sin() * radius * r,
        )
    };
    for (i, &value) in values.iter().enumerate() {
        if value == 0.0 {
            continue;
        }
        let sweep = std::f32::consts::TAU * (value / total) as f32;
        let segments = (sweep / 0.06).ceil().max(1.0) as usize;
        let angles: Vec<_> = (0..=segments)
            .map(|step| angle + sweep * step as f32 / segments as f32)
            .collect();
        let mut points: Vec<_> = angles.iter().map(|&a| point(a, 1.0)).collect();
        if hole == 0.0 {
            points.push(center);
        } else {
            points.extend(angles.iter().rev().map(|&a| point(a, hole)));
        }
        canvas.path(&points, color(data, i), true);
        angle += sweep;
    }
}

fn axes(canvas: &mut Canvas<'_>, data: &ChartData, plot: Box2) {
    let horizontal = data.kind == ChartKind::Bar;
    let scatter = data.kind == ChartKind::Scatter;
    let (lo, hi) = data
        .series
        .iter()
        .flat_map(|series| &series.values)
        .fold((0.0_f64, 0.0_f64), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let (x_lo, x_hi) = data
        .series
        .iter()
        .flat_map(|series| &series.x_values)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let fraction = |v, min: f64, max: f64| {
        if max > min {
            ((v - min) / (max - min)) as f32
        } else {
            0.5
        }
    };
    let value_fraction = |v| fraction(v, lo, hi);
    for step in 0..=4 {
        let relative = step as f32 / 4.0;
        let value = lo + (hi - lo) * f64::from(relative);
        let label = if value.abs() > 99999.0 {
            format!("{value:.1e}")
        } else {
            format!("{value:.3}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        };
        if horizontal {
            let x = plot.x + relative * plot.w;
            canvas.path(&[(x, plot.y), (x, plot.y + plot.h)], GRID, false);
            canvas.text(&label, x, plot.y + plot.h + 11.0, 7.0, true, plot.w / 5.0);
        } else {
            let y = plot.y + (1.0 - relative) * plot.h;
            canvas.path(&[(plot.x, y), (plot.x + plot.w, y)], GRID, false);
            canvas.text(&label, plot.x - 18.0, y + 2.0, 7.0, true, 31.0);
        }
    }
    let points = data
        .series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let zero = value_fraction(0.0);
    let category_space = if horizontal { plot.h } else { plot.w } / points as f32;
    if !scatter {
        let step = ((if horizontal { 12.0 } else { 28.0 } / category_space).ceil() as usize).max(1);
        for (i, label) in data.categories.iter().enumerate().step_by(step) {
            let middle = (i as f32 + 0.5) * category_space;
            if horizontal {
                canvas.text(label, plot.x - 18.0, plot.y + middle + 2.0, 7.0, true, 31.0);
            } else {
                canvas.text(
                    label,
                    plot.x + middle,
                    plot.y + plot.h + 11.0,
                    7.0,
                    true,
                    category_space * step as f32,
                );
            }
        }
    }
    canvas.text(
        &data.x_label,
        plot.x + plot.w / 2.0,
        plot.y + plot.h + 23.0,
        8.0,
        true,
        plot.w,
    );
    canvas.text(
        &data.y_label,
        plot.x + 4.0,
        plot.y - 2.0,
        8.0,
        false,
        plot.w,
    );
    for (sidx, series) in data.series.iter().enumerate() {
        let tint = color(data, sidx);
        let mut stroke = Vec::new();
        for (i, &v) in series.values.iter().enumerate() {
            let val = value_fraction(v);
            let position = (i as f32 + 0.5) * category_space;
            let y = plot.y + (1.0 - val) * plot.h;
            match data.kind {
                ChartKind::Column | ChartKind::Bar => {
                    let bar = 0.76 * category_space / data.series.len() as f32;
                    let start = category_space * (i as f32 + 0.12) + bar * sidx as f32;
                    let length = (val - zero).abs();
                    if horizontal {
                        canvas.rect(
                            Box2 {
                                x: plot.x + val.min(zero) * plot.w,
                                y: plot.y + start,
                                w: length * plot.w,
                                h: bar,
                            },
                            tint,
                        );
                    } else {
                        canvas.rect(
                            Box2 {
                                x: plot.x + start,
                                y: plot.y + (1.0 - val.max(zero)) * plot.h,
                                w: bar,
                                h: length * plot.h,
                            },
                            tint,
                        );
                    }
                }
                ChartKind::Scatter | ChartKind::Line => {
                    let x = plot.x
                        + if scatter {
                            series
                                .x_values
                                .get(i)
                                .map_or(position, |v| fraction(*v, x_lo, x_hi) * plot.w)
                        } else {
                            position
                        };
                    canvas.rect(
                        Box2 {
                            x: x - 1.8,
                            y: y - 1.8,
                            w: 3.6,
                            h: 3.6,
                        },
                        tint,
                    );
                    stroke.push((x, y));
                }
                ChartKind::Area => stroke.push((plot.x + position, y)),
                _ => (),
            }
        }
        if data.kind == ChartKind::Line {
            canvas.path(&stroke, tint, false);
        } else if data.kind == ChartKind::Area
            && let (Some(&first), Some(&last)) = (stroke.first(), stroke.last())
        {
            let baseline = plot.y + (1.0 - zero) * plot.h;
            stroke.push((last.0, baseline));
            stroke.insert(0, (first.0, baseline));
            canvas.path(&stroke, tint, true);
        }
    }
}

//! Signed differences between displayed averages, with no differences across gaps.
use std::collections::VecDeque;
use std::time::Instant;

use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};

const UP: Color32 = Color32::from_rgb(101, 224, 161);
const DOWN: Color32 = Color32::from_rgb(255, 113, 134);
const QUIET: Color32 = Color32::from_rgb(165, 157, 183);
const WINDOW_SECONDS: f32 = 60.0;

type Reading = (Instant, Option<f64>);

fn differences(history: &VecDeque<Reading>) -> Vec<Reading> {
    let mut previous = None;
    history
        .iter()
        .map(|&(time, value)| {
            let delta = value
                .zip(previous)
                .map(|(current, previous)| current - previous);
            previous = value;
            (time, delta)
        })
        .collect()
}

fn tint(value: f64) -> Color32 {
    if value > 0.0 {
        UP
    } else if value < 0.0 {
        DOWN
    } else {
        QUIET
    }
}

fn number(value: f64) -> String {
    // Avoid a negative zero when a sub-resolution change rounds to zero.
    if value.abs() < 0.005 {
        "0.00".into()
    } else {
        format!("{value:+.2}")
    }
}

/// Clip a convex fill polygon before colouring it. This keeps the gradient
/// linear in screen height even when a reading is far outside the visible range.
fn clip_fill(mut polygon: Vec<Pos2>, plot: Rect) -> Vec<Pos2> {
    for (axis, edge, minimum) in [
        (0, plot.left(), true),
        (0, plot.right(), false),
        (1, plot.top(), true),
        (1, plot.bottom(), false),
    ] {
        if polygon.is_empty() {
            break;
        }
        let coordinate = |point: Pos2| if axis == 0 { point.x } else { point.y };
        let inside = |point: Pos2| {
            if minimum {
                coordinate(point) >= edge
            } else {
                coordinate(point) <= edge
            }
        };
        let mut clipped = Vec::new();
        let mut previous = *polygon.last().unwrap();
        for &point in &polygon {
            if inside(previous) != inside(point) {
                let fraction =
                    (edge - coordinate(previous)) / (coordinate(point) - coordinate(previous));
                let mut intersection = previous + (point - previous) * fraction;
                if axis == 0 {
                    intersection.x = edge;
                } else {
                    intersection.y = edge;
                }
                clipped.push(intersection);
            }
            if inside(point) {
                clipped.push(point);
            }
            previous = point;
        }
        polygon = clipped;
    }
    polygon
}

fn fill_colour(y: f32, zero: f32, plot: Rect, positive: bool) -> Color32 {
    let colour = if positive { UP } else { DOWN };
    let extent = if positive {
        zero - plot.top()
    } else {
        plot.bottom() - zero
    };
    let fraction = ((y - zero).abs() / extent.max(1.0)).clamp(0.0, 1.0);
    let alpha = (8.0 + 77.0 * fraction).round() as u8;
    Color32::from_rgba_unmultiplied(colour.r(), colour.g(), colour.b(), alpha)
}

fn area_mesh(a: Pos2, b: Pos2, zero: f32, plot: Rect) -> egui::Mesh {
    let polygon = clip_fill(
        vec![egui::pos2(a.x, zero), a, b, egui::pos2(b.x, zero)],
        plot,
    );
    let positive = (a.y + b.y) * 0.5 <= zero;
    let mut mesh = egui::Mesh::default();
    for point in polygon {
        mesh.colored_vertex(point, fill_colour(point.y, zero, plot, positive));
    }
    for index in 1..mesh.vertices.len().saturating_sub(1) {
        mesh.add_triangle(0, index as u32, index as u32 + 1);
    }
    mesh
}

/// Split at zero to preserve sign colours, then clip the fill to the viewport.
/// Every segment uses the same height-based gradient, independent of its slope.
fn area(painter: &egui::Painter, a: Pos2, b: Pos2, zero: f32, plot: Rect) {
    if (a.y - zero) * (b.y - zero) < 0.0 {
        let fraction = (zero - a.y) / (b.y - a.y);
        let crossing = egui::pos2(a.x + fraction * (b.x - a.x), zero);
        area(painter, a, crossing, zero, plot);
        area(painter, crossing, b, zero, plot);
        return;
    }
    let colour = if a.y == zero && b.y == zero {
        QUIET
    } else if (a.y + b.y) * 0.5 <= zero {
        UP
    } else {
        DOWN
    };
    let mesh = area_mesh(a, b, zero, plot);
    if !mesh.indices.is_empty() {
        painter.add(egui::Shape::mesh(mesh));
    }
    painter.line_segment([a, b], Stroke::new(1.8, colour));
}

/// Keep one predecessor solely for rendering the segment crossing the left edge.
/// It must not participate in bounds or hover values, and a missing reading stays
/// missing so a genuine sensor gap cannot be filled in accidentally.
fn visible_start(samples: &[Reading], now: Instant, window: f32) -> usize {
    samples.partition_point(|(time, _)| now.saturating_duration_since(*time).as_secs_f32() > window)
}

/// Middle 60% of finite values, expanded to contain every sample from the last
/// ten seconds. Percentiles use linear interpolation between sorted observations.
fn robust_bounds(samples: &[Reading], now: Instant, signed: bool) -> (f64, f64) {
    let mut values: Vec<_> = samples
        .iter()
        .filter_map(|(_, value)| *value)
        .filter(|v| v.is_finite())
        .collect();
    if values.is_empty() {
        return if signed { (-0.1, 0.1) } else { (0.0, 1.0) };
    }
    values.sort_by(f64::total_cmp);
    let percentile = |fraction: f64| {
        let index = (values.len() - 1) as f64 * fraction;
        let low = index.floor() as usize;
        let high = index.ceil() as usize;
        values[low] + (values[high] - values[low]) * index.fract()
    };
    let mut low = percentile(0.2);
    let mut high = percentile(0.8);
    for &(time, value) in samples {
        if now.saturating_duration_since(time).as_secs_f64() <= 10.0
            && let Some(value) = value.filter(|v| v.is_finite())
        {
            low = low.min(value);
            high = high.max(value);
        }
    }
    if high - low < 0.1 {
        let midpoint = (low + high) * 0.5;
        (midpoint - 0.05, midpoint + 0.05)
    } else {
        (low, high)
    }
}

pub(super) fn readout(
    ui: &mut egui::Ui,
    value: Option<f64>,
    signed: bool,
    font_size: f32,
    average_seconds: f64,
) {
    let colour = if signed {
        value.map_or(QUIET, tint)
    } else {
        super::VIOLET
    };
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(if signed {
                "CHANGE IN RESISTANCE"
            } else {
                "SKIN RESISTANCE"
            })
            .size(12.0)
            .color(if signed { QUIET } else { super::VIOLET }),
        );
        ui.add_space(14.0);
        let text = value.map_or_else(
            || "—".into(),
            |value| {
                if signed {
                    number(value)
                } else {
                    format!("{value:.1}")
                }
            },
        );
        ui.add(
            egui::Label::new(
                RichText::new(text)
                    .monospace()
                    .size(font_size)
                    .color(colour),
            )
            .wrap_mode(egui::TextWrapMode::Extend),
        );
        ui.label(RichText::new("kΩ").size(23.0).color(colour));
        ui.add_space(12.0);
        ui.label(
            RichText::new(if signed {
                "current − previous reading".into()
            } else {
                format!("{average_seconds:.2} s average")
            })
            .size(11.0)
            .color(QUIET),
        );
    });
}

pub(super) fn show(
    ui: &mut egui::Ui,
    history: &VecDeque<Reading>,
    signed: bool,
    current: Option<f64>,
    now: Instant,
) {
    let window = if signed { WINDOW_SECONDS } else { 180.0 };
    let history: Vec<_> = if signed {
        differences(history)
    } else {
        history.iter().copied().collect()
    };
    let start = visible_start(&history, now, window);
    let samples = &history[start..];
    let render_samples = &history[start.saturating_sub(1)..];
    let (low, high) = robust_bounds(samples, now, signed);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 190.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let plot = Rect::from_min_max(
        rect.min + Vec2::new(3.0, 12.0),
        rect.max - Vec2::new(61.0, 12.0),
    );
    let trace = painter.with_clip_rect(plot);
    let position = |time: Instant, value: f64| {
        egui::pos2(
            plot.right()
                - now.saturating_duration_since(time).as_secs_f32() / window * plot.width(),
            plot.bottom() - ((value - low) / (high - low)) as f32 * plot.height(),
        )
    };
    let zero = position(now, 0.0).y;
    if signed {
        let split = zero.clamp(plot.top(), plot.bottom());
        trace.rect_filled(
            Rect::from_min_max(plot.min, egui::pos2(plot.right(), split)),
            0,
            UP.gamma_multiply(0.035),
        );
        trace.rect_filled(
            Rect::from_min_max(egui::pos2(plot.left(), split), plot.max),
            0,
            DOWN.gamma_multiply(0.035),
        );
    }
    let middle = if signed && low < 0.0 && high > 0.0 {
        0.0
    } else {
        (low + high) * 0.5
    };
    for value in [high, middle, low] {
        let y = position(now, value).y;
        painter.hline(
            plot.x_range(),
            y,
            Stroke::new(
                1.0,
                if signed && value == 0.0 {
                    Color32::from_gray(106)
                } else {
                    Color32::from_gray(49)
                },
            ),
        );
        let text = if signed {
            number(value)
        } else {
            format!("{value:.2}")
        };
        painter.text(
            egui::pos2(plot.right() + 7.0, y),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(10.0),
            QUIET,
        );
    }
    let in_bounds = |value: f64| value.is_finite() && value >= low && value <= high;
    let mut previous = None;
    let mut latest = None;
    for &(time, value) in render_samples {
        if let Some(value) = value.filter(|v| v.is_finite() && (signed || in_bounds(*v))) {
            let point = position(time, value);
            if let Some(previous) = previous {
                if signed {
                    area(&trace, previous, point, zero, plot);
                } else {
                    trace.line_segment([previous, point], Stroke::new(2.0, super::VIOLET));
                }
            }
            previous = Some(point);
            if time == samples.last().map_or(time, |(time, _)| *time) && in_bounds(value) {
                latest = Some((point, value));
            }
        } else {
            // Genuine gaps break the fill; delta outliers are geometrically clipped.
            // Absolute resistance retains its existing outlier-omission behaviour.
            previous = None;
        }
    }
    if current.is_some()
        && let Some((point, value)) = latest
    {
        let colour = if signed { tint(value) } else { super::VIOLET };
        trace.circle_filled(point, 5.5, colour.gamma_multiply(0.15));
        trace.circle_filled(point, 2.7, colour);
    }
    if samples.iter().all(|(_, value)| value.is_none()) {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "Waiting for readings",
            FontId::proportional(12.0),
            QUIET,
        );
    }
    if let Some(pointer) = response.hover_pos()
        && let Some(&(time, Some(value))) = samples
            .iter()
            .filter(|(_, value)| value.is_some_and(in_bounds))
            .min_by(|(a, _), (b, _)| {
                (position(*a, low).x - pointer.x)
                    .abs()
                    .total_cmp(&(position(*b, low).x - pointer.x).abs())
            })
    {
        let point = position(time, value);
        let colour = if signed { tint(value) } else { super::VIOLET };
        trace.vline(
            point.x,
            plot.y_range(),
            Stroke::new(1.0, QUIET.gamma_multiply(0.45)),
        );
        trace.circle_filled(point, 3.5, colour);
        response.on_hover_ui(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} kΩ",
                    if signed {
                        number(value)
                    } else {
                        format!("{value:.2}")
                    }
                ))
                .monospace()
                .color(colour),
            );
            ui.label(format!(
                "{:.1} seconds ago",
                now.saturating_duration_since(time).as_secs_f64()
            ));
        });
    }
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(if signed { "60 s ago" } else { "3 min ago" })
                .size(10.0)
                .color(QUIET),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("now").size(10.0).color(QUIET));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mesh_area(mesh: &egui::Mesh) -> f32 {
        mesh.indices
            .chunks_exact(3)
            .map(|indices| {
                let a = mesh.vertices[indices[0] as usize].pos;
                let b = mesh.vertices[indices[1] as usize].pos;
                let c = mesh.vertices[indices[2] as usize].pos;
                ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() * 0.5
            })
            .sum()
    }
    #[test]
    fn offscale_segments_fill_to_the_boundary_on_both_sides_of_zero() {
        let plot = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        for y in [-100.0, 200.0] {
            let mesh = area_mesh(egui::pos2(0.0, y), egui::pos2(100.0, y), 50.0, plot);
            assert!((mesh_area(&mesh) - 5000.0).abs() < 0.01);
            assert!(mesh.vertices.iter().all(|vertex| plot.contains(vertex.pos)));
        }
    }
    #[test]
    fn fill_reaches_left_edge_using_the_preceding_sample() {
        let now = Instant::now();
        let readings = [
            (now - std::time::Duration::from_secs(61), Some(2.0)),
            (now - std::time::Duration::from_secs(59), Some(3.0)),
        ];
        let start = visible_start(&readings, now, 60.0);
        assert_eq!(start, 1);
        assert_eq!(readings[start.saturating_sub(1)..].len(), 2);
        let plot = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let mesh = area_mesh(egui::pos2(-10.0, 20.0), egui::pos2(10.0, 30.0), 50.0, plot);
        assert!(
            mesh.vertices
                .iter()
                .any(|vertex| vertex.pos == egui::pos2(0.0, 25.0))
        );
        assert!((mesh_area(&mesh) - 225.0).abs() < 0.01);
    }
    #[test]
    fn gradient_uses_plot_height_not_segment_height() {
        let plot = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let steep = area_mesh(egui::pos2(0.0, 10.0), egui::pos2(50.0, 40.0), 50.0, plot);
        let shallow = area_mesh(egui::pos2(50.0, 40.0), egui::pos2(100.0, 30.0), 50.0, plot);
        for mesh in [&steep, &shallow] {
            for vertex in &mesh.vertices {
                assert_eq!(vertex.color, fill_colour(vertex.pos.y, 50.0, plot, true));
            }
        }
        assert!(fill_colour(10.0, 50.0, plot, true).a() > fill_colour(40.0, 50.0, plot, true).a());
        let shared = |mesh: &egui::Mesh| {
            mesh.vertices
                .iter()
                .find(|v| v.pos == egui::pos2(50.0, 40.0))
                .unwrap()
                .color
        };
        assert_eq!(shared(&steep), shared(&shallow));
    }
    #[test]
    fn predecessor_does_not_erase_a_real_gap() {
        let now = Instant::now();
        let history = VecDeque::from([
            (now - std::time::Duration::from_secs(61), Some(100.0)),
            (now - std::time::Duration::from_secs(60), None),
            (now - std::time::Duration::from_secs(59), Some(102.0)),
            (now, Some(103.0)),
        ]);
        let deltas = differences(&history);
        let start = visible_start(&deltas, now, 60.0);
        assert_eq!(start, 1);
        assert_eq!(deltas[start].1, None);
        assert_eq!(deltas[start + 1].1, None);
        assert_eq!(deltas[start + 2].1, Some(1.0));
    }
    fn old_ramp(now: Instant) -> Vec<Reading> {
        (0..100)
            .map(|value| {
                (
                    now - std::time::Duration::from_secs(20),
                    Some(f64::from(value)),
                )
            })
            .collect()
    }
    #[test]
    fn percentile_bounds_reject_old_extreme_values() {
        let now = Instant::now();
        let mut values = old_ramp(now);
        values[0].1 = Some(-1_000_000.0);
        values[99].1 = Some(1_000_000.0);
        let (low, high) = robust_bounds(&values, now, false);
        assert!((low - 19.8).abs() < 1e-9);
        assert!((high - 79.2).abs() < 1e-9);
    }
    #[test]
    fn every_recent_extreme_is_kept_including_exactly_ten_seconds() {
        let now = Instant::now();
        let mut values = old_ramp(now);
        values.push((now - std::time::Duration::from_secs(10), Some(-1000.0)));
        values.push((now, Some(500.0)));
        assert_eq!(robust_bounds(&values, now, false), (-1000.0, 500.0));
        assert_eq!(robust_bounds(&values, now, true), (-1000.0, 500.0));
    }
    #[test]
    fn an_outlier_stops_controlling_scale_after_ten_seconds() {
        let now = Instant::now();
        let mut values = old_ramp(now);
        values.push((now - std::time::Duration::from_millis(10_001), Some(1000.0)));
        assert!(robust_bounds(&values, now, false).1 < 100.0);
        values.last_mut().unwrap().0 = now - std::time::Duration::from_secs(10);
        assert_eq!(robust_bounds(&values, now, false).1, 1000.0);
    }
    #[test]
    fn bounds_are_finite_for_empty_flat_and_invalid_histories() {
        let now = Instant::now();
        for signed in [false, true] {
            for values in [
                vec![],
                vec![(now, None), (now, Some(f64::NAN))],
                vec![(now, Some(300.0))],
            ] {
                let (low, high) = robust_bounds(&values, now, signed);
                assert!(low.is_finite() && high.is_finite() && low < high);
                if values
                    .last()
                    .is_some_and(|(_, value)| *value == Some(300.0))
                {
                    assert!(low <= 300.0 && high >= 300.0);
                }
            }
        }
    }
    #[test]
    fn differences_use_consecutive_averages_and_break_at_gaps() {
        let now = Instant::now();
        let history = [
            Some(100.0),
            Some(102.5),
            Some(101.0),
            None,
            Some(800.0),
            Some(800.0),
            Some(801.25),
        ]
        .into_iter()
        .map(|value| (now, value))
        .collect();
        assert_eq!(
            differences(&history)
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>(),
            vec![
                None,
                Some(2.5),
                Some(-1.5),
                None,
                None,
                Some(0.0),
                Some(1.25)
            ]
        );
    }
    #[test]
    fn signed_readout_does_not_show_negative_zero() {
        assert_eq!(number(-0.001), "0.00");
        assert_eq!(number(2.345), "+2.35");
        assert_eq!(number(-1.25), "-1.25");
    }
}

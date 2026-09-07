//! The Inspector's histogram (`docs/DESIGN.md` §12.1).
//!
//! One canvas, no interaction: bars for the bins, a baseline, and the span
//! written under the ends. The distribution is computed by the store in a
//! single pass over the column (`sp_store::stats`), so the widget draws a
//! handful of counts however long the signal is.

use iced::widget::canvas::{self, Cache, Frame, Geometry, Path, Text};
use iced::{mouse, Color, Point, Rectangle, Renderer, Size, Theme};
use sp_core::stats::Histogram;

/// Height reserved under the bars for the span labels.
const AXIS_H: f32 = 16.0;

/// A drawn histogram. The cache belongs to the screen so a redraw for an
/// unrelated reason — a theme change, a resize — does not re-tessellate the
/// bars.
#[derive(Debug)]
pub struct HistogramView<'a> {
    pub histogram: &'a Histogram,
    pub cache: &'a Cache,
    /// How the ends of the span are written.
    pub format: fn(f64) -> String,
}

impl<Message> canvas::Program<Message> for HistogramView<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let size = bounds.size();
        let palette = theme.extended_palette();
        let tokens = crate::theme::tokens(theme);
        let bars = self.cache.draw(renderer, size, |frame| {
            draw(
                frame,
                self.histogram,
                size,
                palette.primary.base.color,
                // The axis is a rule and the numbers under it are captions, so
                // both come from the same tokens the rest of the application
                // draws its rules and its quiet text with.
                tokens.rule,
                tokens.text_dim,
                self.format,
            );
        });
        vec![bars]
    }
}

fn draw(
    frame: &mut Frame,
    histogram: &Histogram,
    size: Size,
    bar: Color,
    axis: Color,
    label: Color,
    format: fn(f64) -> String,
) {
    let plot_h = (size.height - AXIS_H).max(1.0);
    frame.stroke(
        &Path::line(Point::new(0.0, plot_h), Point::new(size.width, plot_h)),
        canvas::Stroke::default().with_color(axis).with_width(1.0),
    );

    let (low, high) = histogram.span();
    let peak = histogram.peak();
    if peak == 0 {
        frame.fill_text(Text {
            content: "no values in range".to_owned(),
            position: Point::new(6.0, plot_h / 2.0 - 6.0),
            color: label,
            size: crate::typography::LABEL_SIZE,
            font: crate::typography::READOUT,
            ..Text::default()
        });
        return;
    }

    let bins = histogram.bins().max(1);
    let width = size.width / bins as f32;
    for (index, count) in histogram.counts().iter().enumerate() {
        if *count == 0 {
            continue;
        }
        // A bin with anything in it is at least a pixel tall, so a rare value
        // is visible rather than rounded away.
        let height = ((*count as f32 / peak as f32) * plot_h).max(1.0);
        frame.fill_rectangle(
            Point::new(index as f32 * width, plot_h - height),
            Size::new((width - 1.0).max(1.0), height),
            bar,
        );
    }

    for (content, position) in [
        (format(low), Point::new(2.0, plot_h + 2.0)),
        (
            format(high),
            Point::new((size.width - 60.0).max(0.0), plot_h + 2.0),
        ),
        (
            format!("peak {peak}"),
            Point::new(size.width / 2.0 - 24.0, plot_h + 2.0),
        ),
    ] {
        frame.fill_text(Text {
            content,
            position,
            color: label,
            size: crate::typography::LABEL_SIZE,
            font: crate::typography::READOUT,
            ..Text::default()
        });
    }
}

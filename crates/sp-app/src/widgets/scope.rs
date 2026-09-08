//! The scope canvas (`docs/DESIGN.md` §11.4).
//!
//! Rendering is layered so a moving playhead never re-tessellates the traces:
//!
//! | Layer | Contents | Invalidated when |
//! |-------|----------|------------------|
//! | Grid | Axes, gridlines, tick labels | Viewport changes |
//! | Traces | Signal geometry from the pyramid | Viewport or signal set changes |
//! | Overlay | Playhead, cursor, loop band, selection | Every frame (cheap) |
//!
//! | Artifacts | Overlay artifacts on the shared time axis | Viewport or artifact set changes |
//!
//! The grid, trace and artifact layers are [`Cache`]s the screen clears when
//! their inputs change; the overlay is drawn fresh each frame, which is what
//! keeps a playhead tick inside its 0.5 ms budget (§13).
//!
//! The canvas draws from [`TraceSnapshot`]s the engine already reduced — it
//! never touches a sample.

use iced::advanced::text::Shaping;
use iced::mouse;
use iced::widget::canvas::{self, Cache, Frame, Geometry, Path, Stroke, Text};
use iced::{keyboard, Color, Point, Rectangle, Renderer, Size, Theme, Vector};
use sp_core::stats::MinMax;
use sp_core::{OverlayForm, TimeRange};
use sp_engine::reduce::{LogicLevel, TraceForm, TraceGeometry, TraceSnapshot};
use sp_engine::Viewport;

/// How traces share the canvas (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Every trace in one set of axes.
    #[default]
    Overlaid,
    /// One lane per trace, stacked top to bottom.
    Stacked,
}

impl Layout {
    pub const ALL: [Self; 2] = [Self::Overlaid, Self::Stacked];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overlaid => "Overlaid",
            Self::Stacked => "Stacked",
        }
    }
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The eight trace colours, in the order traces take them.
///
/// They live in [`crate::theme`] with the rest of the application's colour;
/// this name stays because every caller of it is here or on a screen that
/// draws a scope.
pub use crate::theme::TRACES as PALETTE;

/// One trace as the canvas sees it.
#[derive(Debug, Clone, Copy)]
pub struct TraceView<'a> {
    pub name: &'a str,
    pub colour: Color,
    pub snapshot: Option<&'a TraceSnapshot>,
    /// Where a logic trace's high band starts, in engineering units.
    pub logic_threshold: f64,
}

/// What the canvas asks the screen to do. The canvas owns no state that
/// outlives a frame: gestures become messages and the screen decides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Scroll: zoom time about the pointer. `factor` above one zooms out.
    ZoomTime { at_s: f64, factor: f64 },
    /// Shift+scroll: pan by pixels.
    Pan(f32),
    /// Ctrl+scroll: zoom the amplitude window.
    ZoomAmplitude(f64),
    /// Drag: fit the dragged span.
    BoxZoom(TimeRange),
    /// Double-click: fit everything.
    Fit,
    /// Click: move the playhead.
    Seek(f64),
    /// Pointer position for the readout, or `None` when it leaves.
    Hover(Option<f64>),
    /// The canvas is a different width than the snapshots were reduced at.
    Resized(f32),
}

/// One row of an overlay artifact, placed on the shared time axis (§10.1).
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayItem {
    pub span: TimeRange,
    /// The row's magnitude, for the forms that have a height.
    pub value: Option<f64>,
    pub label: String,
}

/// One overlay artifact as the canvas sees it.
#[derive(Debug, Clone)]
pub struct OverlayView {
    pub form: OverlayForm,
    pub colour: Color,
    /// Owned: a pane's rows are kilobytes, and the canvas is rebuilt per
    /// frame anyway.
    pub items: Vec<OverlayItem>,
    /// The row under the playhead, drawn emphasised so the table and the
    /// scope agree about which detection is current (§10.3).
    pub current: Option<usize>,
}

/// The canvas layers that survive between frames.
#[derive(Debug, Default)]
pub struct Caches {
    pub grid: Cache,
    pub traces: Cache,
    pub artifacts: Cache,
}

impl Caches {
    /// Invalidates the geometry that depends on the viewport — which is every
    /// cached layer, since the traces are reduced against it.
    pub fn clear(&self) {
        self.grid.clear();
        self.traces.clear();
        self.artifacts.clear();
    }
}

/// Gesture state the canvas keeps between events.
#[derive(Debug, Default)]
pub struct Interaction {
    modifiers: keyboard::Modifiers,
    /// Where a drag started, in canvas pixels.
    drag_from: Option<f32>,
    /// Current drag position, for the rubber band.
    drag_to: Option<f32>,
    /// The last left-button press, for double-click detection.
    last_click: Option<(std::time::Instant, f32)>,
}

impl Interaction {
    /// The band being dragged, in canvas pixels.
    fn band(&self) -> Option<(f32, f32)> {
        let (from, to) = (self.drag_from?, self.drag_to?);
        (from - to)
            .abs()
            .ge(&2.0)
            .then_some((from.min(to), from.max(to)))
    }
}

/// Pixels a drag must cover before it counts as a box zoom rather than a click.
const DRAG_THRESHOLD_PX: f32 = 3.0;
/// How close two clicks must be to count as a double-click.
const DOUBLE_CLICK_MS: u128 = 400;
/// One scroll notch.
const ZOOM_STEP: f64 = 1.25;

/// The scope canvas.
#[derive(Debug)]
pub struct Scope<'a> {
    pub traces: Vec<TraceView<'a>>,
    /// Artifacts drawn on the signals' own time axis (§10.1).
    pub overlays: Vec<OverlayView>,
    pub viewport: &'a Viewport,
    pub playhead_s: f64,
    pub loop_range: TimeRange,
    pub layout: Layout,
    pub caches: &'a Caches,
    /// Whether the playhead is worth drawing — there is nothing to play
    /// before a trace is loaded.
    pub show_playhead: bool,
}

impl Scope<'_> {
    /// Horizontal scale between the width the snapshots were reduced at and
    /// the width being drawn into. It is 1.0 except in the moment between a
    /// window resize and the reduction that follows it.
    fn x_scale(&self, bounds: Rectangle) -> f32 {
        if self.viewport.width_px() <= 0.0 {
            1.0
        } else {
            bounds.width / self.viewport.width_px()
        }
    }

    /// The vertical band a trace draws into.
    fn lane(&self, index: usize, height: f32) -> (f32, f32) {
        match self.layout {
            Layout::Overlaid => (0.0, height),
            Layout::Stacked => {
                let lanes = self.traces.len().max(1) as f32;
                let lane_height = height / lanes;
                let top = index as f32 * lane_height;
                (top, lane_height)
            }
        }
    }
}

impl canvas::Program<Action> for Scope<'_> {
    type State = Interaction;

    fn update(
        &self,
        state: &mut Self::State,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Action>) {
        use canvas::event::Status;

        // A resize is noticed on the first event over the canvas after it: the
        // draw path meanwhile scales the geometry it has, so the interim is
        // stretched rather than wrong.
        if (bounds.width - self.viewport.width_px()).abs() > 0.5 {
            return (Status::Ignored, Some(Action::Resized(bounds.width)));
        }

        let position = cursor.position_in(bounds);
        match event {
            canvas::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = modifiers;
                (Status::Ignored, None)
            }
            canvas::Event::Mouse(mouse::Event::CursorLeft) => {
                state.drag_from = None;
                state.drag_to = None;
                (Status::Ignored, Some(Action::Hover(None)))
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let Some(point) = position else {
                    return (Status::Ignored, Some(Action::Hover(None)));
                };
                if state.drag_from.is_some() {
                    state.drag_to = Some(point.x);
                }
                (
                    Status::Captured,
                    Some(Action::Hover(Some(self.viewport.time_at(point.x)))),
                )
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(point) = position else {
                    return (Status::Ignored, None);
                };
                let notches = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => f64::from(y),
                    mouse::ScrollDelta::Pixels { y, .. } => f64::from(y) / 40.0,
                };
                if notches == 0.0 {
                    return (Status::Ignored, None);
                }
                let action = if state.modifiers.shift() {
                    Action::Pan(-(notches as f32) * bounds.width * 0.1)
                } else if state.modifiers.control() || state.modifiers.command() {
                    Action::ZoomAmplitude(ZOOM_STEP.powf(-notches))
                } else {
                    Action::ZoomTime {
                        at_s: self.viewport.time_at(point.x),
                        factor: ZOOM_STEP.powf(-notches),
                    }
                };
                (Status::Captured, Some(action))
            }
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = position else {
                    return (Status::Ignored, None);
                };
                let now = std::time::Instant::now();
                let double = state.last_click.is_some_and(|(at, x)| {
                    now.duration_since(at).as_millis() < DOUBLE_CLICK_MS
                        && (x - point.x).abs() < 6.0
                });
                state.last_click = Some((now, point.x));
                if double {
                    state.drag_from = None;
                    state.drag_to = None;
                    return (Status::Captured, Some(Action::Fit));
                }
                state.drag_from = Some(point.x);
                state.drag_to = Some(point.x);
                (Status::Captured, None)
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let from = state.drag_from.take();
                let to = state.drag_to.take().or(position.map(|p| p.x));
                let (Some(from), Some(to)) = (from, to) else {
                    return (Status::Ignored, None);
                };
                if (from - to).abs() < DRAG_THRESHOLD_PX {
                    // A click, not a drag: put the playhead where it landed.
                    (
                        Status::Captured,
                        Some(Action::Seek(self.viewport.time_at(to))),
                    )
                } else {
                    let span = TimeRange::new(
                        self.viewport.time_at(from.min(to)),
                        self.viewport.time_at(from.max(to)),
                    );
                    (Status::Captured, Some(Action::BoxZoom(span)))
                }
            }
            _ => (Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let tokens = crate::theme::tokens(theme);
        let size = bounds.size();
        let scale = self.x_scale(bounds);

        let grid = self.caches.grid.draw(renderer, size, |frame| {
            draw_grid(frame, self.viewport, tokens, size);
        });

        let traces = self.caches.traces.draw(renderer, size, |frame| {
            for (index, trace) in self.traces.iter().enumerate() {
                let Some(snapshot) = trace.snapshot else {
                    continue;
                };
                let (top, height) = self.lane(index, size.height);
                draw_trace(frame, self.viewport, trace, snapshot, top, height, scale);
                if self.layout == Layout::Stacked {
                    // Stacked lanes are only readable if they say whose they
                    // are; overlaid traces are named by the trace panel.
                    frame.fill_text(Text {
                        content: trace.name.to_owned(),
                        position: Point::new(6.0, top + 4.0),
                        color: trace.colour,
                        size: crate::typography::LABEL_SIZE,
                        font: crate::typography::READOUT,
                        shaping: Shaping::Basic,
                        ..Text::default()
                    });
                }
            }
        });

        let artifacts = self.caches.artifacts.draw(renderer, size, |frame| {
            for overlay in &self.overlays {
                draw_overlay(frame, self.viewport, overlay, size);
            }
        });

        let mut overlay = Frame::new(renderer, size);
        draw_loop_band(&mut overlay, self.viewport, self.loop_range, palette, size);
        if let Some((from, to)) = state.band() {
            overlay.fill_rectangle(
                Point::new(from, 0.0),
                Size::new(to - from, size.height),
                Color {
                    a: 0.18,
                    ..palette.primary.base.color
                },
            );
        }
        if self.show_playhead && self.viewport.contains(self.playhead_s) {
            let x = self.viewport.x_of(self.playhead_s);
            stroke_line(
                &mut overlay,
                Point::new(x, 0.0),
                Point::new(x, size.height),
                tokens.playhead,
                1.5,
            );
        }
        if let Some(point) = cursor.position_in(bounds) {
            draw_cursor(&mut overlay, self.viewport, point, palette, tokens, size);
        }

        vec![grid, traces, artifacts, overlay.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Crosshair
        } else {
            mouse::Interaction::default()
        }
    }
}

type Palette<'a> = &'a iced::theme::palette::Extended;

fn stroke_line(frame: &mut Frame, from: Point, to: Point, colour: Color, width: f32) {
    let path = Path::new(|builder| {
        builder.move_to(from);
        builder.line_to(to);
    });
    frame.stroke(
        &path,
        Stroke::default().with_color(colour).with_width(width),
    );
}

fn draw_grid(frame: &mut Frame, viewport: &Viewport, tokens: &crate::theme::Tokens, size: Size) {
    // The graticule is the quietest thing on the face by design: it is there
    // to be measured against, not to be looked at. Its loudness is fixed by
    // the theme and held inside bounds the theme's own tests enforce, rather
    // than being an alpha guessed at here.
    let line = tokens.grid;
    let label = tokens.text_dim;

    let time = viewport.time();
    let step = nice_step(time.duration_s(), 10.0);
    if step > 0.0 {
        let mut t = (time.start_s / step).ceil() * step;
        while t < time.end_s {
            let x = viewport.x_of(t);
            stroke_line(
                frame,
                Point::new(x, 0.0),
                Point::new(x, size.height),
                line,
                1.0,
            );
            frame.fill_text(Text {
                content: format_time(t),
                position: Point::new(x + 3.0, size.height - 14.0),
                color: label,
                size: crate::typography::LABEL_SIZE,
                font: crate::typography::READOUT,
                shaping: Shaping::Basic,
                ..Text::default()
            });
            t += step;
        }
    }

    let amplitude = viewport.amplitude;
    let vstep = nice_step(amplitude.span(), 6.0);
    if vstep > 0.0 {
        let mut v = (amplitude.min / vstep).ceil() * vstep;
        while v < amplitude.max {
            let y = viewport.y_of(v, size.height);
            let zero = v.abs() < vstep / 1e6;
            stroke_line(
                frame,
                Point::new(0.0, y),
                Point::new(size.width, y),
                if zero { tokens.rule } else { line },
                if zero { 1.5 } else { 1.0 },
            );
            frame.fill_text(Text {
                content: format_value(v),
                position: Point::new(3.0, y - 12.0),
                color: label,
                size: crate::typography::LABEL_SIZE,
                font: crate::typography::READOUT,
                shaping: Shaping::Basic,
                ..Text::default()
            });
            v += vstep;
        }
    }
}

/// Draws one overlay artifact in the form its schema declared (§10.1).
///
/// Overlays share the traces' time axis but not their amplitude window: a
/// detection score is not volts, so spans, markers and bands are placed by
/// time and by fraction of the canvas rather than by value. Stems are the
/// exception — a symbol decision *is* an amplitude — and use the viewport.
fn draw_overlay(frame: &mut Frame, viewport: &Viewport, overlay: &OverlayView, size: Size) {
    let visible = viewport.time();
    for (index, item) in overlay.items.iter().enumerate() {
        if item.span.end_s < visible.start_s || item.span.start_s > visible.end_s {
            continue;
        }
        let current = overlay.current == Some(index);
        let alpha = if current { 0.55 } else { 0.28 };
        let x0 = viewport.x_of(item.span.start_s);
        let x1 = viewport.x_of(item.span.end_s);

        match overlay.form {
            OverlayForm::Spans => {
                // A zero-width span would vanish; give it a pixel so a
                // detection on one sample is still visible.
                let width = (x1 - x0).max(1.0);
                frame.fill_rectangle(
                    Point::new(x0, 0.0),
                    Size::new(width, size.height),
                    Color {
                        a: alpha,
                        ..overlay.colour
                    },
                );
                stroke_line(
                    frame,
                    Point::new(x0, 0.0),
                    Point::new(x0, size.height),
                    overlay.colour,
                    if current { 2.0 } else { 1.0 },
                );
            }
            OverlayForm::Markers => {
                stroke_line(
                    frame,
                    Point::new(x0, 0.0),
                    Point::new(x0, size.height),
                    Color {
                        a: if current { 1.0 } else { 0.7 },
                        ..overlay.colour
                    },
                    if current { 2.0 } else { 1.0 },
                );
            }
            OverlayForm::Stems => {
                let base = viewport.y_of(0.0, size.height);
                let y = match item.value {
                    Some(value) => viewport.y_of(value, size.height),
                    None => 0.0,
                };
                stroke_line(
                    frame,
                    Point::new(x0, base),
                    Point::new(x0, y),
                    overlay.colour,
                    if current { 2.0 } else { 1.0 },
                );
                frame.fill_rectangle(
                    Point::new(x0 - 2.0, y - 2.0),
                    Size::new(4.0, 4.0),
                    overlay.colour,
                );
            }
            OverlayForm::Bands => {
                // A band is an amplitude threshold: a horizontal rule across
                // the span it applies to.
                let y = match item.value {
                    Some(value) => viewport.y_of(value, size.height),
                    None => size.height / 2.0,
                };
                stroke_line(
                    frame,
                    Point::new(x0, y),
                    Point::new(x1.max(x0 + 1.0), y),
                    overlay.colour,
                    if current { 2.0 } else { 1.0 },
                );
            }
        }

        if current && !item.label.is_empty() {
            frame.fill_text(Text {
                content: item.label.clone(),
                position: Point::new(x0 + 4.0, 4.0),
                color: overlay.colour,
                size: crate::typography::LABEL_SIZE,
                font: crate::typography::READOUT,
                shaping: Shaping::Basic,
                ..Text::default()
            });
        }
    }
}

fn draw_loop_band(
    frame: &mut Frame,
    viewport: &Viewport,
    loop_range: TimeRange,
    palette: Palette<'_>,
    size: Size,
) {
    let visible = loop_range.intersect(viewport.time());
    if visible.is_empty() || visible == viewport.time() {
        return;
    }
    // Shade what playback will *not* reach, so the loop reads as a restriction.
    let left = viewport.x_of(visible.start_s);
    let right = viewport.x_of(visible.end_s);
    let shade = Color {
        a: 0.25,
        ..palette.background.weak.color
    };
    if left > 0.0 {
        frame.fill_rectangle(Point::ORIGIN, Size::new(left, size.height), shade);
    }
    if right < size.width {
        frame.fill_rectangle(
            Point::new(right, 0.0),
            Size::new(size.width - right, size.height),
            shade,
        );
    }
}

fn draw_cursor(
    frame: &mut Frame,
    viewport: &Viewport,
    point: Point,
    palette: Palette<'_>,
    tokens: &crate::theme::Tokens,
    size: Size,
) {
    // The crosshair is brighter than the graticule and quieter than the
    // playhead: it follows the pointer, so it has to be findable without
    // becoming the thing the eye settles on.
    let hair = Color {
        a: 0.75,
        ..tokens.rule
    };
    stroke_line(
        frame,
        Point::new(point.x, 0.0),
        Point::new(point.x, size.height),
        hair,
        1.0,
    );
    stroke_line(
        frame,
        Point::new(0.0, point.y),
        Point::new(size.width, point.y),
        hair,
        1.0,
    );

    let readout = format!(
        "{}  {}",
        format_time(viewport.time_at(point.x)),
        format_value(viewport.value_at(point.y, size.height)),
    );
    let width = 9.0 + readout.len() as f32 * 6.0;
    let anchor = Point::new(
        (point.x + 8.0).min(size.width - width).max(0.0),
        (point.y + 8.0).min(size.height - 20.0).max(0.0),
    );
    frame.fill_rectangle(
        anchor,
        Size::new(width, 16.0),
        Color {
            a: 0.85,
            ..palette.background.base.color
        },
    );
    frame.fill_text(Text {
        content: readout,
        position: anchor + Vector::new(4.0, 2.0),
        color: palette.background.base.text,
        size: crate::typography::LABEL_SIZE,
        font: crate::typography::READOUT,
        shaping: Shaping::Basic,
        ..Text::default()
    });
}

/// Draws one trace with the renderer its domain calls for (§11.4).
fn draw_trace(
    frame: &mut Frame,
    viewport: &Viewport,
    trace: &TraceView<'_>,
    snapshot: &TraceSnapshot,
    top: f32,
    height: f32,
    scale: f32,
) {
    // Lanes keep a little air so stacked traces do not touch.
    let pad = (height * 0.04).min(8.0);
    let band = (height - 2.0 * pad).max(1.0);
    let y_of = |value: f64| top + pad + viewport.y_of(value, band);

    match (&snapshot.geometry, snapshot.form) {
        (TraceGeometry::Bars { first_px, cells }, TraceForm::Logic) => {
            draw_logic(frame, trace, first_px, cells, top, height, scale);
        }
        (TraceGeometry::Bars { first_px, cells }, TraceForm::Stems) => {
            let base = y_of(0.0);
            for (index, cell) in cells.iter().enumerate() {
                if cell.is_empty() {
                    continue;
                }
                let x = (*first_px as f32 + index as f32 + 0.5) * scale;
                let peak = if cell.max.abs() >= cell.min.abs() {
                    cell.max
                } else {
                    cell.min
                };
                stroke_line(
                    frame,
                    Point::new(x, base),
                    Point::new(x, y_of(f64::from(peak))),
                    trace.colour,
                    1.0,
                );
                frame.fill_rectangle(
                    Point::new(x - 1.5, y_of(f64::from(peak)) - 1.5),
                    Size::new(3.0, 3.0),
                    trace.colour,
                );
            }
        }
        (TraceGeometry::Bars { first_px, cells }, form) => {
            let bars = Path::new(|builder| {
                for (index, cell) in cells.iter().enumerate() {
                    if cell.is_empty() {
                        continue;
                    }
                    let x = (*first_px as f32 + index as f32 + 0.5) * scale;
                    builder.move_to(Point::new(x, y_of(f64::from(cell.max))));
                    builder.line_to(Point::new(x, y_of(f64::from(cell.min))));
                }
            });
            frame.stroke(
                &bars,
                Stroke::default()
                    .with_color(trace.colour)
                    .with_width(scale.clamp(1.0, 2.0)),
            );
            if form == TraceForm::Iq {
                // The pyramid reduces a complex column to magnitude, so the
                // envelope is drawn mirrored: the shape an I/Q capture has.
                let mirrored = Path::new(|builder| {
                    for (index, cell) in cells.iter().enumerate() {
                        if cell.is_empty() {
                            continue;
                        }
                        let x = (*first_px as f32 + index as f32 + 0.5) * scale;
                        builder.move_to(Point::new(x, y_of(-f64::from(cell.max))));
                        builder.line_to(Point::new(x, y_of(-f64::from(cell.min))));
                    }
                });
                frame.stroke(
                    &mirrored,
                    Stroke::default()
                        .with_color(Color {
                            a: 0.45,
                            ..trace.colour
                        })
                        .with_width(scale.clamp(1.0, 2.0)),
                );
            }
        }
        (TraceGeometry::Points(points), form) => {
            if points.is_empty() {
                return;
            }
            let base = y_of(0.0);
            if form == TraceForm::Stems {
                for (t, value) in points {
                    let x = viewport.x_of(*t);
                    stroke_line(
                        frame,
                        Point::new(x, base),
                        Point::new(x, y_of(*value)),
                        trace.colour,
                        1.0,
                    );
                }
            } else {
                let line = Path::new(|builder| {
                    for (index, (t, value)) in points.iter().enumerate() {
                        let point = Point::new(viewport.x_of(*t), y_of(*value));
                        if index == 0 {
                            builder.move_to(point);
                        } else {
                            builder.line_to(point);
                        }
                    }
                });
                frame.stroke(
                    &line,
                    Stroke::default().with_color(trace.colour).with_width(1.5),
                );
            }
            // Markers, so single samples are visible as samples.
            if points.len() < 400 {
                for (t, value) in points {
                    frame.fill_rectangle(
                        Point::new(viewport.x_of(*t) - 1.5, y_of(*value) - 1.5),
                        Size::new(3.0, 3.0),
                        trace.colour,
                    );
                }
            }
        }
    }
}

/// Logic lanes: a high rail, a low rail and a vertical edge wherever a pixel
/// holds both (§11.4).
fn draw_logic(
    frame: &mut Frame,
    trace: &TraceView<'_>,
    first_px: &u32,
    cells: &[MinMax],
    top: f32,
    height: f32,
    scale: f32,
) {
    let pad = (height * 0.2).min(16.0);
    let high = top + pad;
    let low = top + height - pad;
    let path = Path::new(|builder| {
        let mut previous: Option<(f32, f32)> = None;
        for (index, cell) in cells.iter().enumerate() {
            let x = (*first_px as f32 + index as f32) * scale;
            let level = LogicLevel::of(*cell, trace.logic_threshold);
            let y = match level {
                LogicLevel::High => Some(high),
                LogicLevel::Low => Some(low),
                LogicLevel::Transition => {
                    builder.move_to(Point::new(x, high));
                    builder.line_to(Point::new(x, low));
                    previous = None;
                    continue;
                }
                LogicLevel::Unknown => {
                    previous = None;
                    continue;
                }
            };
            let Some(y) = y else { continue };
            match previous {
                Some((_prev_x, prev_y)) if (prev_y - y).abs() < f32::EPSILON => {
                    builder.line_to(Point::new(x, y));
                    previous = Some((x, y));
                }
                Some((_, prev_y)) => {
                    builder.line_to(Point::new(x, prev_y));
                    builder.line_to(Point::new(x, y));
                    previous = Some((x, y));
                }
                None => {
                    builder.move_to(Point::new(x, y));
                    previous = Some((x, y));
                }
            }
        }
    });
    frame.stroke(
        &path,
        Stroke::default().with_color(trace.colour).with_width(1.5),
    );
}

/// A 1, 2 or 5 x 10^n step that puts roughly `target` gridlines across `span`.
#[must_use]
pub fn nice_step(span: f64, target: f64) -> f64 {
    if !(span.is_finite() && span > 0.0) || target <= 0.0 {
        return 0.0;
    }
    let rough = span / target;
    let magnitude = 10f64.powf(rough.log10().floor());
    let normalised = rough / magnitude;
    let step = if normalised <= 1.0 {
        1.0
    } else if normalised <= 2.0 {
        2.0
    } else if normalised <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * magnitude
}

/// A time in the largest unit that keeps it readable.
#[must_use]
pub fn format_time(t_s: f64) -> String {
    if !t_s.is_finite() {
        return "—".to_owned();
    }
    let magnitude = t_s.abs();
    if magnitude >= 1.0 || magnitude == 0.0 {
        format!("{t_s:.4} s")
    } else if magnitude >= 1e-3 {
        format!("{:.3} ms", t_s * 1e3)
    } else if magnitude >= 1e-6 {
        format!("{:.3} µs", t_s * 1e6)
    } else {
        format!("{:.1} ns", t_s * 1e9)
    }
}

/// An amplitude, with a short exponent when it needs one.
#[must_use]
pub fn format_value(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_owned();
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-3..1e6).contains(&magnitude) {
        format!("{value:.3e}")
    } else {
        format!("{value:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_one_two_or_five() {
        for span in [1e-9, 0.003, 1.0, 7.0, 250.0, 1e7] {
            let step = nice_step(span, 10.0);
            let normalised = step / 10f64.powf(step.log10().floor());
            assert!(
                [1.0, 2.0, 5.0]
                    .iter()
                    .any(|n| (n - normalised).abs() < 1e-9),
                "span {span} gave step {step}"
            );
            // Roughly the number of lines asked for, never an unusable count.
            let lines = span / step;
            assert!(
                (2.0..=20.0).contains(&lines),
                "span {span} gave {lines} lines"
            );
        }
        assert_eq!(nice_step(0.0, 10.0), 0.0);
        assert_eq!(nice_step(f64::NAN, 10.0), 0.0);
    }

    #[test]
    fn times_pick_a_readable_unit() {
        assert_eq!(format_time(1.5), "1.5000 s");
        assert_eq!(format_time(0.0015), "1.500 ms");
        assert_eq!(format_time(1.5e-6), "1.500 µs");
        assert_eq!(format_time(1.5e-9), "1.5 ns");
        assert_eq!(format_time(0.0), "0.0000 s");
        assert_eq!(format_time(f64::NAN), "—");
    }

    #[test]
    fn values_fall_back_to_an_exponent() {
        assert_eq!(format_value(1.25), "1.2500");
        assert_eq!(format_value(0.0), "0.0000");
        assert_eq!(format_value(2.5e9), "2.500e9");
        assert_eq!(format_value(1e-9), "1.000e-9");
    }

    #[test]
    fn a_drag_needs_to_cover_ground_to_be_a_band() {
        let mut interaction = Interaction {
            drag_from: Some(10.0),
            drag_to: Some(11.0),
            ..Interaction::default()
        };
        assert_eq!(interaction.band(), None);
        interaction.drag_to = Some(40.0);
        assert_eq!(interaction.band(), Some((10.0, 40.0)));
        // Dragging right to left reads the same way.
        interaction.drag_from = Some(40.0);
        interaction.drag_to = Some(10.0);
        assert_eq!(interaction.band(), Some((10.0, 40.0)));
    }
}

//! Artifact viewers (`docs/DESIGN.md` §10.1, §10.3).
//!
//! One function draws any artifact: the payload arrives decoded into an
//! [`ArtifactData`], and its schema's [`ViewHint`] picks the renderer. Nothing
//! here knows what a detection or a spectrum is, which is what makes "a new
//! artifact type costs one `impl`" true of the viewer as well as of storage.
//!
//! Every time-aware view takes the row under the global playhead and
//! emphasises it, so scrubbing the scope moves the highlight in the table at
//! the same instant (§10.3).

use crate::typography;
use crate::ui;
use iced::advanced::text::Shaping;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Alignment, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};
use sp_core::artifact::{ArtifactData, Column, FieldDiff, FieldKind, FieldRef, ViewHint};

/// How a table is ordered, by field name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub field: String,
    pub ascending: bool,
}

impl Sort {
    #[must_use]
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            ascending: true,
        }
    }

    /// Clicking the sorted column again reverses it; clicking another sorts
    /// by that one ascending.
    #[must_use]
    pub fn toggled(&self, field: &str) -> Self {
        if self.field == field {
            Self {
                field: self.field.clone(),
                ascending: !self.ascending,
            }
        } else {
            Self::new(field)
        }
    }
}

/// One artifact pane's inputs.
#[derive(Debug, Clone)]
pub struct Pane<'a> {
    pub title: &'a str,
    pub summary: Option<&'a str>,
    pub data: &'a ArtifactData,
    /// The row the playhead is inside, when the artifact carries time.
    pub current_row: Option<usize>,
    pub sort: Option<&'a Sort>,
    /// Field-level differences against the pinned stage's artifact (§10.4).
    pub diff: Option<&'a [FieldDiff]>,
    pub colour: Color,
}

/// Draws one artifact in whatever form its schema declared.
pub fn view<'a, M: Clone + 'a>(
    pane: &Pane<'a>,
    on_sort: impl Fn(String) -> M + 'a,
) -> Element<'a, M> {
    let body: Element<'a, M> = match &pane.data.schema().view {
        ViewHint::Table { columns } => table(pane, columns, on_sort),
        ViewHint::Series { x, y, x_log, y_log } => chart(
            pane,
            ChartKind::Series {
                x: x.as_str().to_owned(),
                y: y.iter().map(|f| f.as_str().to_owned()).collect(),
                x_log: *x_log,
                y_log: *y_log,
            },
        ),
        ViewHint::Scatter { x, y, colour } => chart(
            pane,
            ChartKind::Scatter {
                x: x.as_str().to_owned(),
                y: y.as_str().to_owned(),
                colour: colour.as_ref().map(|f| f.as_str().to_owned()),
            },
        ),
        ViewHint::Heatmap { values, .. } => chart(
            pane,
            ChartKind::Heatmap {
                values: values.as_str().to_owned(),
            },
        ),
        ViewHint::Scalars => scalars(pane),
        // An overlay's home is the scope; the pane lists it so the rows are
        // readable as numbers too.
        ViewHint::Overlay { .. } => table_of_every_field(pane, on_sort),
        ViewHint::Tree => tree(pane),
    };

    // The port is what the stage called this artifact, so it is the pane's
    // name and it is set as a caption over the pane rather than as a line of
    // body text competing with the rows under it.
    let mut heading = row![ui::caption(pane.title)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(summary) = pane.summary {
        heading = heading.push(text(summary).size(typography::LABEL_SIZE).style(ui::dim));
    }
    heading = heading.push(Space::with_width(Length::Fill));
    heading = heading.push(
        text(format!("{} rows", pane.data.rows()))
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(ui::dim),
    );

    let mut content = column![heading].spacing(6);
    if let Some(diff) = pane.diff {
        content = content.push(diff_summary(diff));
    }
    content = content.push(body);

    // A pane already sits inside a rail on its own surface, so painting it
    // as a second surface would be a card inside a card. A hairline above it
    // is enough to say where one artifact ends and the next begins.
    column![
        ui::rule(),
        container(content).padding([8, 2]).width(Length::Fill)
    ]
    .spacing(6)
    .into()
}

/// The rows in display order.
fn order(data: &ArtifactData, sort: Option<&Sort>) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..data.rows()).collect();
    let Some(sort) = sort else {
        return rows;
    };
    let Some(column) = data.column(&sort.field) else {
        return rows;
    };
    rows.sort_by(|a, b| {
        let ordering = match (column.number_at(*a), column.number_at(*b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => column
                .display_at(*a, None)
                .cmp(&column.display_at(*b, None)),
        };
        if sort.ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
    rows
}

fn table<'a, M: Clone + 'a>(
    pane: &Pane<'a>,
    columns: &[sp_core::ColumnSpec],
    on_sort: impl Fn(String) -> M + 'a,
) -> Element<'a, M> {
    let headers: Vec<(String, String, Option<u8>)> = columns
        .iter()
        .filter(|spec| pane.data.column(spec.field.as_str()).is_some())
        .map(|spec| {
            (
                spec.field.as_str().to_owned(),
                spec.header.clone(),
                spec.precision,
            )
        })
        .collect();
    draw_table(pane, headers, on_sort)
}

/// The table an overlay artifact gets: every decoded field, in schema order.
fn table_of_every_field<'a, M: Clone + 'a>(
    pane: &Pane<'a>,
    on_sort: impl Fn(String) -> M + 'a,
) -> Element<'a, M> {
    let headers: Vec<(String, String, Option<u8>)> = pane
        .data
        .fields()
        .map(|(spec, _)| {
            let header = match &spec.unit {
                Some(unit) => format!("{} ({unit})", spec.name),
                None => spec.name.clone(),
            };
            (spec.name.clone(), header, Some(4))
        })
        .collect();
    draw_table(pane, headers, on_sort)
}

fn draw_table<'a, M: Clone + 'a>(
    pane: &Pane<'a>,
    headers: Vec<(String, String, Option<u8>)>,
    on_sort: impl Fn(String) -> M + 'a,
) -> Element<'a, M> {
    if headers.is_empty() || pane.data.is_empty() {
        return empty("This artifact carries no rows.");
    }

    let mut header_row = row![].spacing(4);
    for (field, header, _) in &headers {
        let marker = match pane.sort {
            Some(sort) if sort.field == *field => {
                if sort.ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            }
            _ => "",
        };
        // The column the table is sorted on is the one piece of state the
        // header carries, so it is the one heading in the full text colour.
        let sorted = matches!(pane.sort, Some(sort) if sort.field == *field);
        header_row = header_row.push(
            button(ui::column_label(format!("{header}{marker}"), sorted))
                .padding([2.0, 4.0])
                .style(button::text)
                .width(Length::Fill)
                .on_press(on_sort(field.clone())),
        );
    }

    let mut body = column![].spacing(1);
    for index in order(pane.data, pane.sort) {
        let current = pane.current_row == Some(index);
        let mut cells = row![].spacing(4);
        for (field, _, precision) in &headers {
            let value = pane
                .data
                .column(field)
                .map_or_else(String::new, |column| column.display_at(index, *precision));
            // Every cell of an artifact table is machine output, so the whole
            // grid is monospaced and a column can be scanned rather than read.
            cells = cells.push(
                text(value)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .width(Length::Fill),
            );
        }
        // The row under the playhead was painted in the accent at full
        // strength, which is louder than the trace the playhead is on. It
        // takes the same band a selection takes everywhere else.
        body = body.push(container(cells).padding([2, 4]).width(Length::Fill).style(
            move |theme: &Theme| container::Style {
                background: current.then(|| crate::theme::tokens(theme).selection.into()),
                border: iced::border::rounded(2),
                ..container::Style::default()
            },
        ));
    }

    column![header_row, scrollable(body).height(Length::Fill)]
        .spacing(4)
        .height(Length::Fill)
        .into()
}

fn scalars<'a, M: 'a>(pane: &Pane<'a>) -> Element<'a, M> {
    let mut list = column![].spacing(3);
    for (spec, column) in pane.data.fields() {
        let label = match &spec.unit {
            Some(unit) => format!("{} ({unit})", spec.name),
            None => spec.name.clone(),
        };
        list = list.push(
            row![
                container(ui::caption(label)).width(Length::Fixed(160.0)),
                text(column.display_at(0, Some(6)))
                    .size(typography::BODY_SIZE)
                    .font(typography::READOUT),
            ]
            .spacing(8),
        );
    }
    if pane.data.is_empty() {
        return empty("This artifact carries no values.");
    }
    scrollable(list).height(Length::Fill).into()
}

fn tree<'a, M: 'a>(pane: &Pane<'a>) -> Element<'a, M> {
    if pane.data.is_empty() {
        return empty(
            "No schema is registered for this artifact kind, so its payload is shown as stored.",
        );
    }
    let mut list = column![].spacing(2);
    for (spec, column) in pane.data.fields() {
        list = list.push(
            text(format!(
                "{}: {} × {}",
                spec.name,
                column.len(),
                describe_kind(spec.kind)
            ))
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT),
        );
    }
    scrollable(list).height(Length::Fill).into()
}

fn diff_summary<'a, M: 'a>(diff: &[FieldDiff]) -> Element<'a, M> {
    let changed: Vec<&FieldDiff> = diff.iter().filter(|d| !d.is_equal()).collect();
    if changed.is_empty() {
        return text("Identical to the pinned stage.")
            .size(typography::LABEL_SIZE)
            .style(text::success)
            .into();
    }
    let mut list = column![].spacing(1);
    for field in changed {
        let first = field
            .first_divergence
            .map_or_else(String::new, |row| format!(", first at row {row}"));
        list = list.push(
            text(format!(
                "{}: {} row{} differ, max |Δ| {:.6}{first}",
                field.field,
                field.mismatches,
                if field.mismatches == 1 { "" } else { "s" },
                field.max_abs_error
            ))
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(text::danger),
        );
    }
    list.into()
}

fn empty<'a, M: 'a>(message: &'a str) -> Element<'a, M> {
    container(text(message).size(typography::LABEL_SIZE).style(ui::dim))
        .padding(6)
        .width(Length::Fill)
        .into()
}

fn describe_kind(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Float | FieldKind::FloatArray => "number",
        FieldKind::Int => "integer",
        FieldKind::Bool => "boolean",
        FieldKind::Text => "text",
        FieldKind::TimeS => "time",
        FieldKind::SpanS => "span",
    }
}

// ---------------------------------------------------------------------------
// Charts
// ---------------------------------------------------------------------------

/// Which chart a view hint asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartKind {
    Series {
        x: String,
        y: Vec<String>,
        x_log: bool,
        y_log: bool,
    },
    Scatter {
        x: String,
        y: String,
        colour: Option<String>,
    },
    Heatmap {
        values: String,
    },
}

fn chart<'a, M: 'a>(pane: &Pane<'a>, kind: ChartKind) -> Element<'a, M> {
    if pane.data.is_empty() {
        return empty("This artifact carries no rows.");
    }
    let program = Chart {
        data: pane.data.clone(),
        kind,
        colour: pane.colour,
    };
    container(
        canvas::Canvas::new(program)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .height(Length::Fixed(180.0))
    .width(Length::Fill)
    .into()
}

/// A chart over one artifact's columns. It owns its data: a pane is rebuilt
/// per frame, and a decoded artifact is kilobytes rather than samples.
#[derive(Debug)]
struct Chart {
    data: ArtifactData,
    kind: ChartKind,
    colour: Color,
}

impl<M> canvas::Program<M> for Chart {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        let size = bounds.size();
        frame.fill_rectangle(Point::ORIGIN, size, palette.background.base.color);

        match &self.kind {
            ChartKind::Series { x, y, x_log, y_log } => {
                self.draw_series(&mut frame, size, x, y, *x_log, *y_log, palette);
            }
            ChartKind::Scatter { x, y, .. } => self.draw_scatter(&mut frame, size, x, y, palette),
            ChartKind::Heatmap { values } => self.draw_heatmap(&mut frame, size, values),
        }

        vec![frame.into_geometry()]
    }
}

type Palette<'a> = &'a iced::theme::palette::Extended;

impl Chart {
    #[allow(clippy::too_many_arguments)]
    fn draw_series(
        &self,
        frame: &mut Frame,
        size: Size,
        x_field: &str,
        y_fields: &[String],
        x_log: bool,
        y_log: bool,
        palette: Palette<'_>,
    ) {
        let Some(x) = self.data.column(x_field) else {
            return;
        };
        let series: Vec<&Column> = y_fields
            .iter()
            .filter_map(|name| self.data.column(name))
            .collect();
        if series.is_empty() {
            return;
        }

        let x_values: Vec<Option<f64>> = (0..x.len())
            .map(|row| scale(x.number_at(row), x_log))
            .collect();
        let y_extent = series
            .iter()
            .flat_map(|column| {
                (0..column.len()).filter_map(|row| scale(column.number_at(row), y_log))
            })
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), value| {
                (lo.min(value), hi.max(value))
            });
        let x_extent = x_values
            .iter()
            .flatten()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), value| {
                (lo.min(*value), hi.max(*value))
            });
        let (Some(to_x), Some(to_y)) =
            (mapper(x_extent, size.width), mapper(y_extent, size.height))
        else {
            return;
        };

        axes(frame, size, palette);
        for (index, column) in series.iter().enumerate() {
            let colour = shade(self.colour, index);
            let path = Path::new(|builder| {
                let mut started = false;
                for (row, x) in x_values.iter().enumerate().take(column.len()) {
                    let (Some(x), Some(y)) = (*x, scale(column.number_at(row), y_log)) else {
                        started = false;
                        continue;
                    };
                    let point = Point::new(to_x(x), size.height - to_y(y));
                    if started {
                        builder.line_to(point);
                    } else {
                        builder.move_to(point);
                        started = true;
                    }
                }
            });
            frame.stroke(&path, Stroke::default().with_color(colour).with_width(1.5));
        }
        label(
            frame,
            size,
            palette,
            &format!(
                "{x_field}{}  ·  {}",
                if x_log { " (log)" } else { "" },
                y_fields.join(", ")
            ),
        );
    }

    fn draw_scatter(
        &self,
        frame: &mut Frame,
        size: Size,
        x_field: &str,
        y_field: &str,
        palette: Palette<'_>,
    ) {
        let (Some(x), Some(y)) = (self.data.column(x_field), self.data.column(y_field)) else {
            return;
        };
        let extent = |column: &Column| {
            (0..column.len())
                .filter_map(|row| column.number_at(row))
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), value| {
                    (lo.min(value), hi.max(value))
                })
        };
        let (Some(to_x), Some(to_y)) = (
            mapper(extent(x), size.width),
            mapper(extent(y), size.height),
        ) else {
            return;
        };

        axes(frame, size, palette);
        for row in 0..x.len().min(y.len()) {
            let (Some(x), Some(y)) = (x.number_at(row), y.number_at(row)) else {
                continue;
            };
            frame.fill_rectangle(
                Point::new(to_x(x) - 1.5, size.height - to_y(y) - 1.5),
                Size::new(3.0, 3.0),
                self.colour,
            );
        }
        label(frame, size, palette, &format!("{x_field} × {y_field}"));
    }

    fn draw_heatmap(&self, frame: &mut Frame, size: Size, values: &str) {
        let Some(Column::Matrix(rows)) = self.data.column(values) else {
            return;
        };
        let (lo, hi) = rows
            .iter()
            .flatten()
            .filter(|value| value.is_finite())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), value| {
                (lo.min(*value), hi.max(*value))
            });
        let span = (hi - lo).max(f64::MIN_POSITIVE);
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 || rows.is_empty() {
            return;
        }
        let cell = Size::new(size.width / columns as f32, size.height / rows.len() as f32);
        for (r, values) in rows.iter().enumerate() {
            for (c, value) in values.iter().enumerate() {
                if !value.is_finite() {
                    continue;
                }
                let intensity = ((value - lo) / span) as f32;
                frame.fill_rectangle(
                    Point::new(c as f32 * cell.width, r as f32 * cell.height),
                    cell,
                    Color {
                        a: intensity.clamp(0.05, 1.0),
                        ..self.colour
                    },
                );
            }
        }
    }
}

/// Maps a data extent onto a pixel span, or `None` when there is nothing to
/// map — a single point, or no finite values at all.
fn mapper((lo, hi): (f64, f64), pixels: f32) -> Option<impl Fn(f64) -> f32> {
    if !lo.is_finite() || !hi.is_finite() {
        return None;
    }
    let pad = 6.0;
    let span = if (hi - lo).abs() < f64::EPSILON {
        1.0
    } else {
        hi - lo
    };
    let usable = (pixels - 2.0 * pad).max(1.0);
    Some(move |value: f64| pad + (((value - lo) / span) as f32) * usable)
}

/// A log axis drops non-positive values rather than clamping them, which would
/// invent a floor the data does not have.
fn scale(value: Option<f64>, log: bool) -> Option<f64> {
    let value = value?;
    if log {
        (value > 0.0).then(|| value.log10())
    } else {
        Some(value)
    }
}

fn axes(frame: &mut Frame, size: Size, palette: Palette<'_>) {
    let colour = Color {
        a: 0.4,
        ..palette.background.strong.color
    };
    let path = Path::new(|builder| {
        builder.move_to(Point::new(0.0, size.height - 1.0));
        builder.line_to(Point::new(size.width, size.height - 1.0));
        builder.move_to(Point::new(1.0, 0.0));
        builder.line_to(Point::new(1.0, size.height));
    });
    frame.stroke(&path, Stroke::default().with_color(colour).with_width(1.0));
}

fn label(frame: &mut Frame, size: Size, palette: Palette<'_>, content: &str) {
    frame.fill_text(Text {
        content: content.to_owned(),
        position: Point::new(6.0, size.height - 14.0),
        color: palette.background.base.text,
        size: 10.0.into(),
        shaping: Shaping::Basic,
        ..Text::default()
    });
}

/// Successive series of one chart, distinguished by alpha rather than by hue:
/// the pane's colour identifies the artifact, and its series are its own.
fn shade(colour: Color, index: usize) -> Color {
    Color {
        a: 1.0 - (index as f32 * 0.22).min(0.6),
        ..colour
    }
}

/// The column a pane opens ordered by: the one its view reads first, or —
/// for an overlay, whose pane lists every field — the first declared field.
#[must_use]
pub fn default_sort(schema: &sp_core::ArtifactSchema) -> Option<Sort> {
    let first = |field: &FieldRef| Some(Sort::new(field.as_str()));
    match &schema.view {
        ViewHint::Table { columns } => columns.first().map(|c| Sort::new(c.field.as_str())),
        ViewHint::Series { x, .. } | ViewHint::Scatter { x, .. } => first(x),
        ViewHint::Overlay { .. } => schema.fields.first().map(|spec| Sort::new(&spec.name)),
        ViewHint::Heatmap { .. } | ViewHint::Scalars | ViewHint::Tree => None,
    }
}

#[cfg(test)]
mod tests {
    use sp_core::artifact::{ArtifactSchema, ColumnSpec, FieldSpec};

    use super::*;

    fn data() -> ArtifactData {
        let schema = ArtifactSchema::new(
            vec![
                FieldSpec::new("spans", FieldKind::SpanS),
                FieldSpec::new("scores", FieldKind::Float),
            ],
            ViewHint::Table {
                columns: vec![
                    ColumnSpec::new("spans", "Span"),
                    ColumnSpec::new("scores", "Score").with_precision(2),
                ],
            },
        );
        ArtifactData::decode(
            schema,
            r#"{"spans":[[0.0,1.0],[2.0,3.0],[4.0,5.0]],"scores":[0.7,0.1,0.4]}"#,
        )
        .unwrap()
    }

    #[test]
    fn rows_sort_by_the_chosen_column_and_reverse_on_a_second_click() {
        let data = data();
        let ascending = Sort::new("scores");
        assert_eq!(order(&data, Some(&ascending)), vec![1, 2, 0]);
        let descending = ascending.toggled("scores");
        assert!(!descending.ascending);
        assert_eq!(order(&data, Some(&descending)), vec![0, 2, 1]);
        // A different column starts ascending again.
        let other = descending.toggled("spans");
        assert_eq!(other, Sort::new("spans"));
        assert_eq!(order(&data, Some(&other)), vec![0, 1, 2]);
    }

    #[test]
    fn unsorted_rows_stay_in_payload_order() {
        assert_eq!(order(&data(), None), vec![0, 1, 2]);
        // Sorting by a field the payload omits changes nothing.
        assert_eq!(order(&data(), Some(&Sort::new("absent"))), vec![0, 1, 2]);
    }

    #[test]
    fn a_table_view_sorts_by_its_first_column_until_the_user_says_otherwise() {
        let sort = default_sort(data().schema()).unwrap();
        assert_eq!(sort.field, "spans");
        assert!(sort.ascending);
        assert!(default_sort(&ArtifactSchema::opaque()).is_none());
    }

    #[test]
    fn a_log_axis_drops_values_it_cannot_place() {
        assert_eq!(scale(Some(100.0), true), Some(2.0));
        assert_eq!(scale(Some(0.0), true), None);
        assert_eq!(scale(Some(-1.0), true), None);
        assert_eq!(scale(Some(-1.0), false), Some(-1.0));
        assert_eq!(scale(None, false), None);
    }

    #[test]
    fn a_flat_extent_still_maps_rather_than_dividing_by_zero() {
        let map = mapper((3.0, 3.0), 100.0).expect("a flat extent maps");
        assert!(map(3.0).is_finite());
        assert!(mapper((f64::INFINITY, f64::NEG_INFINITY), 100.0).is_none());
    }
}

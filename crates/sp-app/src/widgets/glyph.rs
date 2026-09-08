//! The shapes the type does not carry (`.impeccable.md`).
//!
//! Archivo and Martian Mono are text faces. Neither has a cmap entry for a
//! geometric triangle, a filled square or a pause bar, and a character a font
//! cannot draw renders as a hollow box — the application's own disclosure
//! arrows, sort markers and transport controls were all boxes on a machine
//! with no fallback face holding those blocks.
//!
//! Rather than embed a symbol font for eleven characters, the eleven shapes
//! are drawn. Each is a canvas one line tall, filled in a colour taken from
//! the theme, so a mark is the same weight and the same grey as the text it
//! sits beside and follows the theme when it changes. Drawn marks also come
//! out crisper than a glyph at 8px, and are not at the mercy of whatever a
//! given font decided a "black right-pointing triangle" should look like.
//!
//! Sizes are the caller's: [`SMALL`] beside a caption, [`MEDIUM`] beside body
//! text or inside a button.

use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::widget::Canvas;
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

/// A mark beside a caption: a disclosure arrow, a sort direction.
pub const SMALL: f32 = 8.0;

/// A mark beside body text, or carrying a button of its own.
pub const MEDIUM: f32 = 11.0;

/// The marks the application draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Points at something not yet open, or at the next page.
    Right,
    /// The previous page.
    Left,
    /// Ascending: smallest first, so the arrow points at the top of the list.
    Up,
    /// Descending, and an open disclosure.
    Down,
    /// Transport: play.
    Play,
    /// Transport: pause. Two bars, the one control that is not a triangle.
    Pause,
    /// Transport: stop.
    Stop,
    /// A colour swatch: a trace's colour, a series key.
    Swatch,
}

/// Where a mark takes its colour from. Every option but [`Ink::Fixed`] is a
/// theme token, so a mark is never a colour the rest of the screen is not.
#[derive(Clone, Copy, Debug)]
pub enum Ink {
    /// The colour of the text around it.
    Text,
    /// Quiet: a disclosure arrow, a marker on a column heading.
    Dim,
    /// The text colour of a filled primary button, for a mark that sits on
    /// the accent rather than on the page.
    OnPrimary,
    /// The text colour of a filled secondary button.
    OnSecondary,
    /// A colour the mark is *about* — a trace's own colour.
    Fixed(Color),
}

impl Ink {
    fn colour(self, theme: &Theme) -> Color {
        match self {
            Self::Text => theme.extended_palette().background.base.text,
            Self::Dim => crate::theme::tokens(theme).text_dim,
            Self::OnPrimary => theme.extended_palette().primary.base.text,
            Self::OnSecondary => theme.extended_palette().secondary.base.text,
            Self::Fixed(colour) => colour,
        }
    }
}

/// A drawn mark, sized by the canvas it is handed.
#[derive(Debug)]
struct Drawn {
    mark: Mark,
    ink: Ink,
}

impl<Message> canvas::Program<Message> for Drawn {
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
        let mut frame = Frame::new(renderer, size);
        let colour = self.ink.colour(theme);
        for path in paths(self.mark, size) {
            frame.fill(&path, colour);
        }
        vec![frame.into_geometry()]
    }
}

/// The geometry of a mark inside a square of `size`.
///
/// Triangles are drawn on a slightly narrow base — 0.82 of the height — which
/// is what a text face does with the same shape: a pointer that is as wide as
/// it is tall reads as a lozenge rather than as a direction.
fn paths(mark: Mark, size: Size) -> Vec<Path> {
    let side = size.width.min(size.height);
    let (cx, cy) = (size.width / 2.0, size.height / 2.0);
    let half = side / 2.0;
    let reach = half * 0.82;

    let triangle = |dx: f32, dy: f32| {
        // `dx`/`dy` is the unit direction the point faces; the base is the
        // opposite edge.
        Path::new(|builder| {
            let (bx, by) = (-dy, dx);
            builder.move_to(Point::new(cx + dx * half, cy + dy * half));
            builder.line_to(Point::new(
                cx - dx * reach + bx * reach,
                cy - dy * reach + by * reach,
            ));
            builder.line_to(Point::new(
                cx - dx * reach - bx * reach,
                cy - dy * reach - by * reach,
            ));
            builder.close();
        })
    };

    match mark {
        Mark::Right | Mark::Play => vec![triangle(1.0, 0.0)],
        Mark::Left => vec![triangle(-1.0, 0.0)],
        Mark::Up => vec![triangle(0.0, -1.0)],
        Mark::Down => vec![triangle(0.0, 1.0)],
        Mark::Pause => {
            // Two bars over the same width a triangle would have covered, so
            // play and pause do not shift the label beside them as they swap.
            let bar = side * 0.28;
            let gap = side * 0.2;
            vec![
                Path::rectangle(
                    Point::new(cx - gap / 2.0 - bar, cy - half),
                    Size::new(bar, side),
                ),
                Path::rectangle(Point::new(cx + gap / 2.0, cy - half), Size::new(bar, side)),
            ]
        }
        Mark::Stop => {
            let stop = side * 0.78;
            vec![Path::rectangle(
                Point::new(cx - stop / 2.0, cy - stop / 2.0),
                Size::new(stop, stop),
            )]
        }
        Mark::Swatch => {
            // A key, not a control: a bar rather than a block, so it reads as
            // a sample of the line it stands for.
            let height = side * 0.42;
            vec![Path::rectangle(
                Point::new(cx - half, cy - height / 2.0),
                Size::new(side, height),
            )]
        }
    }
}

/// A mark, drawn in a square of `size` and taking no space beyond it.
pub fn mark<'a, Message: 'a>(mark: Mark, size: f32, ink: Ink) -> Element<'a, Message> {
    Canvas::new(Drawn { mark, ink })
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .into()
}

/// The disclosure arrow on a branch that can be opened and shut.
pub fn disclosure<'a, Message: 'a>(collapsed: bool) -> Element<'a, Message> {
    mark(
        if collapsed { Mark::Right } else { Mark::Down },
        SMALL,
        Ink::Dim,
    )
}

/// The marker on the column a table is sorted by.
pub fn sort_marker<'a, Message: 'a>(ascending: bool) -> Element<'a, Message> {
    mark(
        if ascending { Mark::Up } else { Mark::Down },
        SMALL,
        Ink::Dim,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark draws something. A mark that produces no geometry is the
    /// same blank space the missing glyph left behind.
    #[test]
    fn every_mark_has_geometry() {
        for mark in [
            Mark::Right,
            Mark::Left,
            Mark::Up,
            Mark::Down,
            Mark::Play,
            Mark::Pause,
            Mark::Stop,
            Mark::Swatch,
        ] {
            assert!(
                !paths(mark, Size::new(MEDIUM, MEDIUM)).is_empty(),
                "{mark:?} draws nothing"
            );
        }
    }
}

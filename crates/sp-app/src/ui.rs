//! The pieces every screen is built from.
//!
//! [`crate::theme`] holds the colours and [`crate::typography`] holds the
//! faces; this module is the layer above both, where a colour and a face and a
//! measure are combined into the handful of shapes this application actually
//! draws: a caption, a reading, a spec, a hairline, a row that can be picked.
//!
//! It exists so that "dim" means one thing. Before it, a dozen screens each
//! reached for `text::secondary` and for `horizontal_rule(1)` and each got a
//! slightly different grey out of Iced's own generator. A single definition of
//! each shape is what makes the application read as one instrument rather than
//! as eleven panels built by eleven people.
//!
//! Everything here is generic over the message type, because a caption on the
//! Runs screen is the same caption as one on Generate and neither should have
//! to be written twice.

use iced::widget::{button, column, container, horizontal_rule, rule, text};
use iced::{Alignment, Element, Length, Theme};

use crate::typography;

// ---------------------------------------------------------------- text styles

/// Text that is present but not the subject.
///
/// Most of what is on screen is metadata *about* the thing being looked at,
/// and if all of it is set at full strength none of it is. The dim comes from
/// the open theme, so it is the same dim on every screen.
pub fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(crate::theme::tokens(theme).text_dim),
    }
}

/// A reading that disagrees with what it should be, or a state the operator
/// should notice before acting.
///
/// Not an error — the work was still done and the value is still shown — so it
/// takes the warning colour rather than the danger one. Danger is reserved for
/// something that failed.
pub fn warned(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(crate::theme::tokens(theme).warning),
    }
}

// ------------------------------------------------------------- type registers

/// A caption: small capitals in the condensed face, dim.
///
/// This is how a panel labels the thing under it rather than competing with
/// it. Iced exposes no letter-spacing, so the separation from body text is
/// carried by width, case and colour together — any one of the three alone
/// would not be enough.
pub fn caption<'a, Message: 'a>(content: impl ToString) -> Element<'a, Message> {
    text(content.to_string().to_uppercase())
        .size(typography::LABEL_SIZE)
        .font(typography::LABEL)
        .style(dim)
        .into()
}

/// A reading that is context rather than subject: dim, still monospaced.
pub fn dim_reading<'a, Message: 'a>(content: impl ToString) -> Element<'a, Message> {
    text(content.to_string())
        .size(typography::BODY_SIZE)
        .font(typography::READOUT)
        .style(dim)
        .into()
}

// -------------------------------------------------------------------- a spec

/// One fact: what it is, then what it says.
///
/// The caption is small capitals in the condensed face and the reading is
/// monospaced, which is how an instrument panel is made — the caption is
/// etched into the metal and the reading is the part that changes. A row of
/// these replaces the interpunct-joined sentence that would otherwise carry
/// the same facts as prose the reader has to parse.
pub fn spec<'a, Message: 'a>(
    label: impl ToString,
    value: impl ToString,
    style: fn(&Theme) -> text::Style,
) -> Element<'a, Message> {
    column![
        caption::<Message>(label),
        text(value.to_string())
            .size(typography::BODY_SIZE)
            .font(typography::READOUT)
            .style(style),
    ]
    .spacing(1)
    .into()
}

/// A spec whose reading needs no colour of its own.
pub fn fact<'a, Message: 'a>(label: impl ToString, value: impl ToString) -> Element<'a, Message> {
    spec(label, value, text::base)
}

// -------------------------------------------------------------------- tables

/// A cell holding words: a name, a domain, a type.
pub fn cell<'a, Message: 'a>(content: impl ToString, width: f32) -> Element<'a, Message> {
    container(text(content.to_string()).size(typography::BODY_SIZE))
        .width(Length::Fixed(width))
        .padding([3, 6])
        .into()
}

/// A cell holding a number. Monospaced and flush right, which is the pair of
/// choices that makes a column of statistics readable: the digits line up on
/// the units place, so two values of different magnitude can be told apart by
/// their shape before either is read.
pub fn value<'a, Message: 'a>(content: impl ToString, width: f32) -> Element<'a, Message> {
    container(
        text(content.to_string())
            .size(typography::BODY_SIZE)
            .font(typography::READOUT),
    )
    .width(Length::Fixed(width))
    .align_x(Alignment::End)
    .padding([3, 6])
    .into()
}

/// The text of a column heading. Where a table can be sorted, the column it is
/// sorted on is the one piece of state the header carries, so it is the one
/// heading set in the medium weight and the full text colour; the rest stay
/// dim.
pub fn column_label<'a>(content: impl ToString, sorted: bool) -> iced::widget::Text<'a, Theme> {
    let label = text(content.to_string().to_uppercase())
        .size(typography::LABEL_SIZE)
        .font(if sorted {
            typography::BODY_STRONG
        } else {
            typography::LABEL
        });
    if sorted {
        label
    } else {
        label.style(dim)
    }
}

/// A column heading over words: flush left, where the eye starts.
pub fn heading<'a, Message: 'a>(content: impl ToString, width: f32) -> Element<'a, Message> {
    heading_aligned(content, width, Alignment::Start)
}

/// A column heading over numbers: flush right, with them. A caption that does
/// not sit over its own column is not a caption.
pub fn heading_aligned<'a, Message: 'a>(
    content: impl ToString,
    width: f32,
    align: Alignment,
) -> Element<'a, Message> {
    container(column_label(content, false))
        .width(Length::Fixed(width))
        .align_x(align)
        .padding([3, 6])
        .into()
}

// ------------------------------------------------------------------- surfaces

/// A hairline: between the head of a table and its rows, or between two
/// halves of a pane.
///
/// One weight, one colour, from the theme. Iced's own rule is a mid grey that
/// lands at a different loudness in each theme; this one is held inside the
/// bounds the theme's tests enforce.
pub fn rule<'a, Message: 'a>() -> Element<'a, Message> {
    horizontal_rule(1)
        .style(|theme: &Theme| rule::Style {
            color: crate::theme::tokens(theme).rule,
            width: 1,
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
        })
        .into()
}

/// The style of a row, chip or tab that can be picked.
///
/// A selected thing is a band of the accent at low alpha, not a solid fill of
/// it. The accent is the loudest colour the application has and it is spent on
/// the playhead and on this; a whole row painted in it at full strength would
/// drown the name it is meant to be pointing at. Hover is a step of the
/// neutral ladder instead, so hovering never reads as selecting.
pub fn selectable(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let background = match (active, status) {
            (true, _) => Some(crate::theme::tokens(theme).selection.into()),
            (false, button::Status::Hovered | button::Status::Pressed) => {
                Some(palette.background.strong.color.scale_alpha(0.55).into())
            }
            (false, _) => None,
        };
        button::Style {
            background,
            text_color: palette.background.base.text,
            border: iced::border::rounded(2),
            shadow: iced::Shadow::default(),
        }
    }
}

/// A surface that holds something apart from the pane it sits in: a rail, a
/// sidebar, a footer.
///
/// One step of the neutral ladder plus a hairline border — beside the content,
/// not floating over it. No shadow and no radius to speak of: a shadow implies
/// a depth this interface does not have.
pub fn panel(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.weak.color.into()),
        border: iced::Border {
            color: crate::theme::tokens(theme).rule,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// An empty state: what is not here, then the way in.
///
/// The second line is the point. "Nothing here" tells the operator something
/// they can already see; naming the way in teaches the interface.
pub fn empty<'a, Message: 'a>(
    headline: impl ToString,
    guidance: impl ToString,
) -> Element<'a, Message> {
    column![
        text(headline.to_string()).size(typography::BODY_SIZE),
        text(guidance.to_string())
            .size(typography::BODY_SIZE)
            .style(dim),
    ]
    .spacing(4)
    .max_width(460)
    .into()
}

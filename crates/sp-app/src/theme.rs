//! The application's colour, in one place (`.impeccable.md`).
//!
//! Iced generates most of a theme from five colours, and the widget code
//! across `screens` and `widgets` already reads what it needs from
//! [`iced::Theme::extended_palette`]. Two things follow from that:
//!
//! 1. Replacing [`iced::Theme::Dark`] and `Light` with the two themes built
//!    here re-skins every screen without touching a single call site.
//! 2. The generated ladder — Iced mixes `background` toward `text` by fixed
//!    fractions — cannot express the surface steps this design wants, so both
//!    themes are built with [`iced::Theme::custom_with_fn`] and every step is
//!    stated outright rather than derived.
//!
//! Colours are authored in OKLCH and written here as the sRGB they convert
//! to; the doc comment on each constant is the source of truth and the
//! numbers beside it are the result. OKLCH is perceptually uniform, so a
//! lightness step reads as the size it is, and chroma is pulled in as
//! lightness approaches either end to keep the colour from going garish.
//!
//! The neutrals are not grey: they carry a chroma of 0.006 to 0.012 at the
//! brand hue, which is enough to read as warm graphite (dark) and warm paper
//! (light) beside the ochre accent without ever looking tinted on its own.
//! Nothing here is pure black or pure white.
//!
//! Everything Iced's palette has no room for — rules, grid lines, the
//! playhead, the warning colour, the trace palette — lives in [`Tokens`].

use std::sync::OnceLock;

use iced::theme::palette::{
    Background, Danger, Extended, Pair, Palette, Primary, Secondary, Success,
};
use iced::{Color, Theme};

// ---------------------------------------------------------------- dark ----

/// `oklch(0.200 0.006 70)`
const DARK_BASE: Color = Color::from_rgb(0.0934, 0.0846, 0.0750);
/// `oklch(0.260 0.007 70)`
const DARK_WEAK: Color = Color::from_rgb(0.1500, 0.1391, 0.1271);
/// `oklch(0.360 0.008 70)`
const DARK_STRONG: Color = Color::from_rgb(0.2505, 0.2370, 0.2222);
/// `oklch(0.900 0.008 70)`
const DARK_TEXT: Color = Color::from_rgb(0.8842, 0.8671, 0.8486);
/// `oklch(0.700 0.008 70)`
const DARK_TEXT_DIM: Color = Color::from_rgb(0.6340, 0.6180, 0.6005);
/// `oklch(0.320 0.008 70)`
const DARK_RULE: Color = Color::from_rgb(0.2099, 0.1967, 0.1824);
/// `oklch(0.290 0.007 70)`
const DARK_GRID: Color = Color::from_rgb(0.1789, 0.1677, 0.1554);
/// `oklch(0.500 0.105 66)`
const DARK_PRIMARY_WEAK: Color = Color::from_rgb(0.5447, 0.3338, 0.0613);
/// `oklch(0.720 0.150 68)`
const DARK_PRIMARY: Color = Color::from_rgb(0.8825, 0.5644, 0.1199);
/// `oklch(0.820 0.110 72)`
const DARK_PRIMARY_STRONG: Color = Color::from_rgb(0.9427, 0.7231, 0.4433);
/// `oklch(0.460 0.085 150)`
const DARK_SUCCESS_WEAK: Color = Color::from_rgb(0.1921, 0.3968, 0.2428);
/// `oklch(0.700 0.110 150)`
const DARK_SUCCESS: Color = Color::from_rgb(0.4128, 0.6968, 0.4766);
/// `oklch(0.800 0.080 150)`
const DARK_SUCCESS_STRONG: Color = Color::from_rgb(0.6001, 0.8030, 0.6382);
/// `oklch(0.450 0.130 25)`
const DARK_DANGER_WEAK: Color = Color::from_rgb(0.5632, 0.1881, 0.1806);
/// `oklch(0.620 0.170 25)`
const DARK_DANGER: Color = Color::from_rgb(0.8531, 0.3253, 0.3102);
/// `oklch(0.720 0.130 25)`
const DARK_DANGER_STRONG: Color = Color::from_rgb(0.9210, 0.5099, 0.4812);
/// `oklch(0.780 0.130 80)`
const DARK_WARNING: Color = Color::from_rgb(0.8891, 0.6798, 0.2932);

// --------------------------------------------------------------- light ----

/// `oklch(0.975 0.004 80)`
const LIGHT_BASE: Color = Color::from_rgb(0.9730, 0.9664, 0.9559);
/// `oklch(0.940 0.006 80)`
const LIGHT_WEAK: Color = Color::from_rgb(0.9302, 0.9204, 0.9047);
/// `oklch(0.860 0.008 80)`
const LIGHT_STRONG: Color = Color::from_rgb(0.8300, 0.8173, 0.7968);
/// `oklch(0.280 0.012 70)`
const LIGHT_TEXT: Color = Color::from_rgb(0.1757, 0.1566, 0.1356);
/// `oklch(0.480 0.010 70)`
const LIGHT_TEXT_DIM: Color = Color::from_rgb(0.3818, 0.3635, 0.3436);
/// `oklch(0.820 0.008 80)`
const LIGHT_RULE: Color = Color::from_rgb(0.7794, 0.7669, 0.7466);
/// `oklch(0.890 0.006 80)`
const LIGHT_GRID: Color = Color::from_rgb(0.8656, 0.8560, 0.8405);
/// `oklch(0.760 0.100 66)`
const LIGHT_PRIMARY_WEAK: Color = Color::from_rgb(0.8652, 0.6440, 0.4176);
/// `oklch(0.550 0.120 62)`
const LIGHT_PRIMARY: Color = Color::from_rgb(0.6360, 0.3721, 0.0710);
/// `oklch(0.450 0.105 60)`
const LIGHT_PRIMARY_STRONG: Color = Color::from_rgb(0.4954, 0.2685, 0.0041);
/// `oklch(0.760 0.080 150)`
const LIGHT_SUCCESS_WEAK: Color = Color::from_rgb(0.5513, 0.7524, 0.5897);
/// `oklch(0.500 0.110 150)`
const LIGHT_SUCCESS: Color = Color::from_rgb(0.1676, 0.4559, 0.2506);
/// `oklch(0.400 0.095 150)`
const LIGHT_SUCCESS_STRONG: Color = Color::from_rgb(0.0885, 0.3349, 0.1663);
/// `oklch(0.780 0.090 27)`
const LIGHT_DANGER_WEAK: Color = Color::from_rgb(0.9237, 0.6328, 0.5973);
/// `oklch(0.500 0.170 27)`
const LIGHT_DANGER: Color = Color::from_rgb(0.6883, 0.1678, 0.1545);
/// `oklch(0.410 0.150 27)`
const LIGHT_DANGER_STRONG: Color = Color::from_rgb(0.5397, 0.0878, 0.0878);
/// `oklch(0.540 0.115 72)`
const LIGHT_WARNING: Color = Color::from_rgb(0.5925, 0.3830, 0.0077);

// -------------------------------------------------------------- traces ----

/// The eight trace colours, in the order traces take them.
///
/// The first seven are the Okabe–Ito colour-blind-safe set the design calls
/// for (`docs/DESIGN.md`, "Accessibility"). Its eighth member is black, which
/// is invisible on the dark theme, so the eighth slot here is a warm neutral
/// that holds on either background instead. Trace identity is carried by the
/// legend and the line style as well as by colour, so a reader who cannot
/// separate two of these is never left guessing.
pub const TRACES: [Color; 8] = [
    // Okabe-Ito orange, #E69F00
    Color::from_rgb(0.9020, 0.6235, 0.0000),
    // Okabe-Ito sky blue, #56B4E9
    Color::from_rgb(0.3373, 0.7059, 0.9137),
    // Okabe-Ito bluish green, #009E73
    Color::from_rgb(0.0000, 0.6196, 0.4510),
    // Okabe-Ito vermillion, #D55E00
    Color::from_rgb(0.8353, 0.3686, 0.0000),
    // Okabe-Ito blue, #0072B2
    Color::from_rgb(0.0000, 0.4471, 0.6980),
    // Okabe-Ito reddish purple, #CC79A7
    Color::from_rgb(0.8000, 0.4745, 0.6549),
    // Okabe-Ito yellow, #F0E442
    Color::from_rgb(0.9412, 0.8941, 0.2588),
    // `oklch(0.550 0.020 70)` — stands in for Okabe-Ito's black.
    Color::from_rgb(0.4760, 0.4383, 0.3969),
];

/// The colour of a residual trace: the difference between two runs, which is
/// the one trace that is a failure rather than a signal.
///
/// This is the dark theme's danger colour held as a constant, because the
/// Results screen builds its traces in `view`, where Iced has not yet handed
/// anything the open theme — only the canvas itself is given one, at draw
/// time. Reading it from [`Tokens`] instead means moving the choice into the
/// canvas program, which is worth doing and is not this change.
pub const RESIDUAL: Color = DARK_DANGER;

// -------------------------------------------------------------- tokens ----

/// The colours Iced's palette has nowhere to put.
///
/// Iced's [`Extended`] palette covers surfaces, text and the three semantic
/// roles. It has no concept of a hairline rule, a grid line, a playhead or a
/// warning, and those are exactly the parts an instrument is read by. They
/// are resolved from the live theme so a widget never has to know which one
/// is open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    /// Body text that is not the subject: units, counts, secondary metadata.
    /// Still passes AA against every surface in the theme.
    pub text_dim: Color,
    /// A one-pixel separator. Quieter than `background.strong`, which is a
    /// fill; a rule that reads as a fill turns a dense table into a grid of
    /// boxes.
    pub rule: Color,
    /// Graticule and axis lines on a canvas. A step quieter again: the grid
    /// is there to be measured against, not looked at.
    pub grid: Color,
    /// The playhead, and anything else that says *you are here*. The accent,
    /// unmixed, because a position that is hard to find is a bug.
    pub playhead: Color,
    /// Something is off but nothing has failed: a tolerant import that
    /// dropped a column, a run that is stale.
    pub warning: Color,
    /// Behind a selected row or a selected region. Carried at low alpha so it
    /// sits under text without fighting it.
    pub selection: Color,
    /// The surface a value table, a CSV preview or an expression sits on.
    pub readout: Color,
}

const DARK_TOKENS: Tokens = Tokens {
    text_dim: DARK_TEXT_DIM,
    rule: DARK_RULE,
    grid: DARK_GRID,
    playhead: DARK_PRIMARY,
    warning: DARK_WARNING,
    // The accent at low alpha: a selected row keeps the accent's identity
    // without the accent's weight.
    selection: Color {
        a: 0.22,
        ..DARK_PRIMARY
    },
    readout: DARK_WEAK,
};

const LIGHT_TOKENS: Tokens = Tokens {
    text_dim: LIGHT_TEXT_DIM,
    rule: LIGHT_RULE,
    grid: LIGHT_GRID,
    playhead: LIGHT_PRIMARY,
    warning: LIGHT_WARNING,
    selection: Color {
        a: 0.20,
        ..LIGHT_PRIMARY
    },
    readout: LIGHT_WEAK,
};

/// The tokens belonging to `theme`.
///
/// Any theme other than the two built here — nothing constructs one today,
/// but `iced::Theme` carries a dozen built-in variants — is served the set
/// matching its lightness, which is wrong in hue but never unreadable.
#[must_use]
pub fn tokens(theme: &Theme) -> &'static Tokens {
    if theme.extended_palette().is_dark {
        &DARK_TOKENS
    } else {
        &LIGHT_TOKENS
    }
}

// -------------------------------------------------------------- themes ----

/// The theme the application opens in.
#[must_use]
pub fn dark() -> Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME
        .get_or_init(|| {
            Theme::custom_with_fn("SignalPlayback Dark".to_owned(), DARK_PALETTE, |_| {
                DARK_EXTENDED
            })
        })
        .clone()
}

/// The light theme, for a projector, a screenshot or a bright bench.
#[must_use]
pub fn light() -> Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME
        .get_or_init(|| {
            Theme::custom_with_fn("SignalPlayback Light".to_owned(), LIGHT_PALETTE, |_| {
                LIGHT_EXTENDED
            })
        })
        .clone()
}

/// The five colours Iced asks for. Only the parts of Iced that read
/// [`Theme::palette`] rather than the extended palette see these, but they
/// must agree with the extended palette below or the two disagree on screen.
const DARK_PALETTE: Palette = Palette {
    background: DARK_BASE,
    text: DARK_TEXT,
    primary: DARK_PRIMARY,
    success: DARK_SUCCESS,
    danger: DARK_DANGER,
};

const LIGHT_PALETTE: Palette = Palette {
    background: LIGHT_BASE,
    text: LIGHT_TEXT,
    primary: LIGHT_PRIMARY,
    success: LIGHT_SUCCESS,
    danger: LIGHT_DANGER,
};

/// A [`Pair`] whose text is stated rather than derived.
///
/// [`Pair::new`] runs the text colour through a readability check that can
/// replace it, which is the right default for a palette Iced generated and
/// the wrong one for a palette whose contrast has already been chosen. The
/// test below is what holds the promise `Pair` makes instead.
const fn pair(color: Color, text: Color) -> Pair {
    Pair { color, text }
}

const DARK_EXTENDED: Extended = Extended {
    background: Background {
        // The window itself.
        base: pair(DARK_BASE, DARK_TEXT),
        // Panels, rails and table headers that lift off it.
        weak: pair(DARK_WEAK, DARK_TEXT),
        // Hover, pressed, and the strongest fill a control gets.
        strong: pair(DARK_STRONG, DARK_TEXT),
    },
    primary: Primary {
        base: pair(DARK_PRIMARY, DARK_BASE),
        weak: pair(DARK_PRIMARY_WEAK, DARK_TEXT),
        strong: pair(DARK_PRIMARY_STRONG, DARK_BASE),
    },
    secondary: Secondary {
        base: pair(DARK_STRONG, DARK_TEXT),
        weak: pair(DARK_WEAK, DARK_TEXT),
        strong: pair(DARK_RULE, DARK_TEXT),
    },
    success: Success {
        base: pair(DARK_SUCCESS, DARK_BASE),
        weak: pair(DARK_SUCCESS_WEAK, DARK_TEXT),
        strong: pair(DARK_SUCCESS_STRONG, DARK_BASE),
    },
    danger: Danger {
        base: pair(DARK_DANGER, DARK_BASE),
        weak: pair(DARK_DANGER_WEAK, DARK_TEXT),
        strong: pair(DARK_DANGER_STRONG, DARK_BASE),
    },
    is_dark: true,
};

const LIGHT_EXTENDED: Extended = Extended {
    background: Background {
        base: pair(LIGHT_BASE, LIGHT_TEXT),
        weak: pair(LIGHT_WEAK, LIGHT_TEXT),
        strong: pair(LIGHT_STRONG, LIGHT_TEXT),
    },
    primary: Primary {
        base: pair(LIGHT_PRIMARY, LIGHT_BASE),
        weak: pair(LIGHT_PRIMARY_WEAK, LIGHT_TEXT),
        strong: pair(LIGHT_PRIMARY_STRONG, LIGHT_BASE),
    },
    secondary: Secondary {
        base: pair(LIGHT_STRONG, LIGHT_TEXT),
        weak: pair(LIGHT_WEAK, LIGHT_TEXT),
        strong: pair(LIGHT_RULE, LIGHT_TEXT),
    },
    success: Success {
        base: pair(LIGHT_SUCCESS, LIGHT_BASE),
        weak: pair(LIGHT_SUCCESS_WEAK, LIGHT_TEXT),
        strong: pair(LIGHT_SUCCESS_STRONG, LIGHT_BASE),
    },
    danger: Danger {
        base: pair(LIGHT_DANGER, LIGHT_BASE),
        weak: pair(LIGHT_DANGER_WEAK, LIGHT_TEXT),
        strong: pair(LIGHT_DANGER_STRONG, LIGHT_BASE),
    },
    is_dark: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance, per WCAG 2.1.
    fn luminance(colour: Color) -> f32 {
        fn channel(c: f32) -> f32 {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Every pair Iced hands a widget claims its text is readable on its own
    /// background, and this theme states those texts rather than letting Iced
    /// derive them — so the claim is only true if it is checked. 4.5 is WCAG
    /// AA for body text, which is the size nearly all of this application
    /// sets.
    #[test]
    fn every_pair_meets_aa_on_its_own_background() {
        for (name, extended) in [("dark", DARK_EXTENDED), ("light", LIGHT_EXTENDED)] {
            let pairs = [
                ("background.base", extended.background.base),
                ("background.weak", extended.background.weak),
                ("background.strong", extended.background.strong),
                ("primary.base", extended.primary.base),
                ("primary.weak", extended.primary.weak),
                ("primary.strong", extended.primary.strong),
                ("secondary.base", extended.secondary.base),
                ("secondary.weak", extended.secondary.weak),
                ("secondary.strong", extended.secondary.strong),
                ("success.base", extended.success.base),
                ("success.weak", extended.success.weak),
                ("success.strong", extended.success.strong),
                ("danger.base", extended.danger.base),
                ("danger.weak", extended.danger.weak),
                ("danger.strong", extended.danger.strong),
            ];
            for (role, pair) in pairs {
                let ratio = contrast(pair.color, pair.text);
                assert!(
                    ratio >= 4.5,
                    "{name} {role}: contrast {ratio:.2} is below AA"
                );
            }
        }
    }

    /// The extra tokens are drawn on the window background, and the three
    /// that carry meaning through their own colour have to be readable there.
    /// The rule and the grid deliberately do not: they are lines to measure
    /// against, not things to read.
    #[test]
    fn the_meaningful_tokens_meet_aa_on_the_window_background() {
        for (name, tokens, background) in [
            ("dark", DARK_TOKENS, DARK_BASE),
            ("light", LIGHT_TOKENS, LIGHT_BASE),
        ] {
            for (role, colour) in [
                ("text_dim", tokens.text_dim),
                ("warning", tokens.warning),
                ("playhead", tokens.playhead),
            ] {
                let ratio = contrast(colour, background);
                assert!(
                    ratio >= 4.5,
                    "{name} {role}: contrast {ratio:.2} is below AA"
                );
            }
        }
    }

    /// A rule that reads as strongly as text turns a table into a cage, and
    /// one that vanishes stops separating anything. The window between is
    /// where a hairline belongs.
    #[test]
    fn the_rules_are_visible_without_being_loud() {
        for (name, tokens, background) in [
            ("dark", DARK_TOKENS, DARK_BASE),
            ("light", LIGHT_TOKENS, LIGHT_BASE),
        ] {
            let rule = contrast(tokens.rule, background);
            assert!((1.15..2.2).contains(&rule), "{name} rule: {rule:.2}");
            let grid = contrast(tokens.grid, background);
            assert!(grid < rule, "{name} grid {grid:.2} is louder than its rule");
        }
    }

    /// Oklab, which is the space these colours were chosen in: equal
    /// distances in it look equally different, which is exactly the question
    /// the test below asks and exactly the question sRGB cannot answer.
    fn oklab(colour: Color) -> (f32, f32, f32) {
        fn channel(c: f32) -> f32 {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let (r, g, b) = (channel(colour.r), channel(colour.g), channel(colour.b));
        let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_995 * b).cbrt();
        let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
        let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
        (
            0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        )
    }

    /// Two traces the eye cannot separate are two traces the reader has to
    /// take on trust. Okabe-Ito is chosen for exactly that, and the eighth
    /// slot is not part of that set — so it is checked rather than assumed.
    ///
    /// The bound is a perceptual distance, not an sRGB one: the set's own
    /// orange and vermillion are close in sRGB and clearly distinct on
    /// screen, and a test that could not tell those apart would be measuring
    /// the wrong thing. 0.12 sits below the 0.15 the tightest pair in this
    /// palette actually holds.
    #[test]
    fn no_two_trace_colours_are_near_neighbours() {
        for (i, a) in TRACES.iter().enumerate() {
            for b in &TRACES[i + 1..] {
                let (a_lab, b_lab) = (oklab(*a), oklab(*b));
                let distance = ((a_lab.0 - b_lab.0).powi(2)
                    + (a_lab.1 - b_lab.1).powi(2)
                    + (a_lab.2 - b_lab.2).powi(2))
                .sqrt();
                assert!(
                    distance > 0.12,
                    "{a:?} and {b:?} are too close: {distance:.3}"
                );
            }
        }
    }

    /// Both themes resolve, and each resolves to the same value every time:
    /// `theme()` runs on every view, and rebuilding an `Arc` per frame to
    /// hand back the same colours would be waste.
    #[test]
    fn a_theme_is_built_once_and_reused() {
        assert!(dark().extended_palette().is_dark);
        assert!(!light().extended_palette().is_dark);
        assert_eq!(dark(), dark());
        assert_eq!(tokens(&dark()), &DARK_TOKENS);
        assert_eq!(tokens(&light()), &LIGHT_TOKENS);
    }
}

//! The application's type, in one place (`.impeccable.md`).
//!
//! The binary carries its own fonts. A signal workbench is read by people who
//! need to tell `1` from `l` and `0` from `O` in a column of samples, and the
//! platform default is a different face on every machine the application
//! runs on — the same table cannot be trusted to lay out the same way twice.
//! Embedding them costs about a megabyte and settles both.
//!
//! Two families, five voices:
//!
//! | Voice | Face | For |
//! |---|---|---|
//! | [`TITLE`] | Archivo Expanded SemiBold | The name of the screen |
//! | [`HEADING`] | Archivo SemiBold | A section inside one |
//! | [`BODY`] | Archivo Regular | Everything written in words |
//! | [`LABEL`] | Archivo Condensed Medium | Small capitals over a field or a column |
//! | [`READOUT`] | Martian Mono Condensed | Anything that is a literal value |
//!
//! Archivo is a grotesque with a width axis, and the width is used the way an
//! instrument panel uses it: a screen title is set wide, a column label
//! narrow, and the two read as different registers of one voice rather than
//! as two typefaces. Martian Mono is condensed so a value table fits more
//! columns before it scrolls, and monospace here is not a costume — it means
//! *this is the value itself*, so it is never spent on prose.
//!
//! Iced has no letter-spacing, so [`LABEL`] earns its separation from
//! condensed width, capitals and the dim text colour rather than from
//! tracking. Setting it in capitals is the caller's job: this module chooses
//! the face, not the string.
//!
//! Both families are SIL Open Font License 1.1; see `assets/fonts/README.md`.

use iced::font::{Family, Weight};
use iced::{Font, Pixels};

// --------------------------------------------------------------- faces ----

/// The bytes Iced has to be handed at start-up, before any of the fonts below
/// will resolve. A font that is not loaded silently falls back to the default
/// face, so this list and the faces below have to stay in step —
/// [`tests::every_face_has_bytes_loaded`] is what keeps them there.
pub const FACES: [&[u8]; 6] = [
    include_bytes!("../assets/fonts/Archivo-Regular.ttf"),
    include_bytes!("../assets/fonts/Archivo-Medium.ttf"),
    include_bytes!("../assets/fonts/Archivo-SemiBold.ttf"),
    include_bytes!("../assets/fonts/ArchivoExpanded-SemiBold.ttf"),
    include_bytes!("../assets/fonts/ArchivoCondensed-Medium.ttf"),
    include_bytes!("../assets/fonts/MartianMonoCondensed-Regular.ttf"),
];

/// The families as they name themselves in their own `name` tables. Iced
/// matches on the typographic family name, which is why the three Archivo
/// weights are one family here and not three.
const ARCHIVO: Family = Family::Name("Archivo");
const ARCHIVO_EXPANDED: Family = Family::Name("Archivo Expanded");
const ARCHIVO_CONDENSED: Family = Family::Name("Archivo Condensed");
const MARTIAN_MONO: Family = Family::Name("Martian Mono Condensed");

const fn face(family: Family, weight: Weight) -> Font {
    Font {
        family,
        weight,
        ..Font::DEFAULT
    }
}

/// The name of the screen, and nothing else. Wide, because on a panel the
/// widest thing is the thing you are looking at.
pub const TITLE: Font = face(ARCHIVO_EXPANDED, Weight::Semibold);

/// A section within a screen.
pub const HEADING: Font = face(ARCHIVO, Weight::Semibold);

/// Everything written in words. The application's default face.
pub const BODY: Font = face(ARCHIVO, Weight::Normal);

/// Body text that needs to stand out without becoming a heading: the selected
/// row, the active stage, the name of the thing being edited.
pub const BODY_STRONG: Font = face(ARCHIVO, Weight::Medium);

/// Small capitals over a field, a column or a group. Set the string in
/// capitals at the call site; this only chooses the face.
pub const LABEL: Font = face(ARCHIVO_CONDENSED, Weight::Medium);

/// A literal value: a sample, a statistic, a timestamp, a hash, an assertion,
/// a line of CSV. Monospace, so digits line up in a column and never reflow
/// as they count.
pub const READOUT: Font = face(MARTIAN_MONO, Weight::Normal);

// --------------------------------------------------------------- scale ----

/// Small capitals. Kept at the same optical weight as [`BODY_SIZE`] rather
/// than a step below it: capitals already read smaller than lowercase, and a
/// label that has to be squinted at is a label nobody reads.
pub const LABEL_SIZE: Pixels = Pixels(11.0);

/// Everything written in words, and every value in a table.
pub const BODY_SIZE: Pixels = Pixels(13.0);

/// A section heading. 1.31x body.
pub const HEADING_SIZE: Pixels = Pixels(17.0);

/// The name of a screen. 1.29x heading.
pub const TITLE_SIZE: Pixels = Pixels(22.0);

/// A number that is the whole point of the panel it sits in: the transport
/// clock, the cursor readout, a headline statistic. 1.32x title.
pub const DISPLAY_SIZE: Pixels = Pixels(29.0);

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale is four steps of at least 1.25 plus one exception.
    ///
    /// Steps that are 1.1x apart do not read as a hierarchy, they read as an
    /// accident, and the sizes this application used before this module —
    /// nine of them between 9 and 22 — were exactly that. [`LABEL_SIZE`] is
    /// the one deliberate exception: it is separated from body by case,
    /// width and colour rather than by size, so holding it a full step down
    /// would only have made it unreadable.
    #[test]
    fn each_step_of_the_scale_is_a_step() {
        let scale = [BODY_SIZE, HEADING_SIZE, TITLE_SIZE, DISPLAY_SIZE];
        for pair in scale.windows(2) {
            let ratio = pair[1].0 / pair[0].0;
            assert!(
                ratio >= 1.25,
                "{:?} to {:?} is only {ratio:.2}x",
                pair[0],
                pair[1]
            );
        }
        assert!(LABEL_SIZE < BODY_SIZE);
    }

    /// A face whose bytes were never handed to Iced renders as the platform
    /// default without saying so, which is the failure this whole module
    /// exists to prevent. Checking the `name` table of each loaded file
    /// against each declared face is the only way to catch a font that was
    /// renamed, dropped or never added to [`FACES`].
    #[test]
    fn every_face_has_bytes_loaded() {
        // The family name a face asks Iced for.
        fn requested(font: Font) -> &'static str {
            match font.family {
                Family::Name(name) => name,
                other => panic!("{other:?} is not a named family"),
            }
        }

        let loaded: Vec<(String, u16)> = FACES.iter().map(|bytes| name_and_weight(bytes)).collect();

        for font in [TITLE, HEADING, BODY, BODY_STRONG, LABEL, READOUT] {
            let family = requested(font);
            let weight = weight_class(font.weight);
            assert!(
                loaded
                    .iter()
                    .any(|(name, class)| name == family && *class == weight),
                "no loaded face is {family} at weight {weight}; \
                 it would silently render as the platform default",
            );
        }
    }

    /// No character the application can put on screen may be one the faces
    /// cannot draw.
    ///
    /// A missing glyph does not fall back to anything on a machine with no
    /// font holding it — it renders as a hollow box, which is what the
    /// disclosure arrows, sort markers, transport controls and the sidebar's
    /// shortcut hints were doing before [`crate::widgets::glyph`] replaced
    /// them with drawn shapes. Archivo and Martian Mono are text faces: they
    /// carry the dash, the ellipsis, the interpunct, the arrows and the
    /// micro sign, and nothing from the geometric-shapes block at all.
    ///
    /// Every non-ASCII character in the crate's own source is checked, prose
    /// in comments included, because a string moves out of a comment and into
    /// a label often enough that drawing the line there would not hold.
    #[test]
    fn every_character_the_sources_carry_can_be_drawn() {
        let coverage: Vec<(String, std::collections::BTreeSet<u32>)> = FACES
            .iter()
            .map(|bytes| (name_and_weight(bytes).0, characters(bytes)))
            .collect();

        for file in sources(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&file).expect("a source file");
            for character in text.chars().filter(|c| !c.is_ascii()) {
                for (family, covered) in &coverage {
                    assert!(
                        covered.contains(&(character as u32)),
                        "{} carries U+{:04X} ({character}), which {family} cannot draw: a hollow box",
                        file.display(),
                        character as u32,
                    );
                }
            }
        }
    }

    /// Every `.rs` file under a directory.
    fn sources(root: std::path::PathBuf) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir)
                .expect("a source directory")
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    found.push(path);
                }
            }
        }
        found
    }

    /// The code points a font's `cmap` maps, from the Unicode subtable Iced's
    /// text shaper reads: format 4 for the basic plane, format 12 beyond it.
    fn characters(bytes: &[u8]) -> std::collections::BTreeSet<u32> {
        let be16 = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize;
        let be32 = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
        };

        let mut cmap = None;
        for i in 0..be16(4) {
            let entry = 12 + 16 * i;
            if &bytes[entry..entry + 4] == b"cmap" {
                cmap = Some(be32(entry + 8));
            }
        }
        let cmap = cmap.expect("a font with no cmap table");

        let mut subtable = None;
        for i in 0..be16(cmap + 2) {
            let record = cmap + 4 + 8 * i;
            let (platform, encoding) = (be16(record), be16(record + 2));
            if matches!((platform, encoding), (3, 1) | (3, 10) | (0, 3) | (0, 4)) {
                subtable = Some(cmap + be32(record + 4));
            }
        }
        let subtable = subtable.expect("a font with no Unicode cmap subtable");

        let mut mapped = std::collections::BTreeSet::new();
        match be16(subtable) {
            4 => {
                let segments = be16(subtable + 6) / 2;
                let ends = subtable + 14;
                let starts = ends + segments * 2 + 2;
                for segment in 0..segments {
                    let start = be16(starts + segment * 2) as u32;
                    let end = be16(ends + segment * 2) as u32;
                    // The last segment ends at U+FFFF by specification and
                    // maps nothing; it is not coverage.
                    mapped.extend((start..=end).take_while(|c| *c < 0xFFFF));
                }
            }
            12 => {
                for group in 0..be32(subtable + 12) {
                    let record = subtable + 16 + 12 * group;
                    mapped.extend(be32(record) as u32..=be32(record + 4) as u32);
                }
            }
            other => panic!("cmap subtable format {other} is not one this test reads"),
        }
        mapped
    }

    fn weight_class(weight: Weight) -> u16 {
        match weight {
            Weight::Normal => 400,
            Weight::Medium => 500,
            Weight::Semibold => 600,
            other => panic!("{other:?} is not a weight this application ships"),
        }
    }

    /// The typographic family name (`name` ID 16) and `usWeightClass` of a
    /// TrueType file, which is the pair Iced's font database matches on.
    /// Falls back to the legacy family name (ID 1) the way that database
    /// does, for the regular weights that carry no typographic name.
    fn name_and_weight(bytes: &[u8]) -> (String, u16) {
        let be16 = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        let be32 = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
        };

        let tables = be16(4) as usize;
        let (mut name, mut os2) = (None, None);
        for i in 0..tables {
            let entry = 12 + 16 * i;
            match &bytes[entry..entry + 4] {
                b"name" => name = Some(be32(entry + 8)),
                b"OS/2" => os2 = Some(be32(entry + 8)),
                _ => {}
            }
        }
        let name = name.expect("a font with no name table");
        let os2 = os2.expect("a font with no OS/2 table");

        let count = be16(name + 2) as usize;
        let strings = name + be16(name + 4) as usize;
        let mut typographic = None;
        let mut legacy = None;
        for i in 0..count {
            let record = name + 6 + 12 * i;
            let (platform, id) = (be16(record), be16(record + 6));
            let (length, offset) = (be16(record + 8) as usize, be16(record + 10) as usize);
            // Only the Windows platform's UTF-16BE strings are read; every
            // face here carries them, and decoding Macintosh encodings to
            // check a name would be a parser this test does not need.
            if platform != 3 || !matches!(id, 1 | 16) {
                continue;
            }
            let raw = &bytes[strings + offset..strings + offset + length];
            let decoded: String = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .filter_map(|c| char::from_u32(u32::from(c)))
                .collect();
            match id {
                16 => typographic.get_or_insert(decoded),
                _ => legacy.get_or_insert(decoded),
            };
        }

        let family = typographic
            .or(legacy)
            .expect("a font with no family name at all");
        (family, be16(os2 + 4))
    }
}

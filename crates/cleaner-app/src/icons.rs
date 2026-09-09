//! Phosphor icon glyphs (MIT, see `assets/fonts/PHOSPHOR-LICENSE`).
//!
//! `theme::install_fonts` puts the font in the proportional family, so these `&str` codepoints
//! render anywhere text does. Each constant names the glyph it points at; the font ships no
//! glyph names, so a wrong codepoint shows up as a missing box rather than a compile error.

use cleaner_core::Category;

// Category glyphs, in the order the Clean screen lists them.
pub(crate) const CAT_VIDEOS: &str = "\u{E4DA}"; // VIDEO_CAMERA
pub(crate) const CAT_STORYBOARDS: &str = "\u{E2CC}"; // IMAGES
pub(crate) const CAT_BACKGROUNDS: &str = "\u{E2CA}"; // IMAGE
pub(crate) const CAT_HITSOUNDS: &str = "\u{E340}"; // MUSIC_NOTES
pub(crate) const CAT_SKIN_ELEMENTS: &str = "\u{E6C8}"; // PALETTE
pub(crate) const CAT_JUNK: &str = "\u{E4A6}"; // TRASH
pub(crate) const CAT_UNREFERENCED: &str = "\u{E2E4}"; // LINK_BREAK

// Screens.
pub(crate) const CLEAN: &str = "\u{E464}"; // SQUARES_FOUR
pub(crate) const SNAPSHOTS: &str = "\u{E466}"; // STACK

// Actions.
pub(crate) const SCAN: &str = "\u{E30C}"; // MAGNIFYING_GLASS
pub(crate) const RESCAN: &str = "\u{E036}"; // ARROW_CLOCKWISE
pub(crate) const RESTORE: &str = "\u{E038}"; // ARROW_COUNTER_CLOCKWISE
pub(crate) const DELETE: &str = "\u{E4A6}"; // TRASH
pub(crate) const COMPACT: &str = "\u{E1DE}"; // DATABASE
pub(crate) const ABOUT: &str = "\u{E2CE}"; // INFO

// States.
pub(crate) const SUCCESS: &str = "\u{E184}"; // CHECK_CIRCLE
pub(crate) const WARNING: &str = "\u{E4E0}"; // WARNING
pub(crate) const ERROR: &str = "\u{E4E2}"; // WARNING_CIRCLE

/// The glyph for a category.
pub(crate) fn category(category: Category) -> &'static str {
    match category {
        Category::Videos => CAT_VIDEOS,
        Category::Storyboards => CAT_STORYBOARDS,
        Category::Backgrounds => CAT_BACKGROUNDS,
        Category::Hitsounds => CAT_HITSOUNDS,
        Category::SkinElements => CAT_SKIN_ELEMENTS,
        Category::Junk => CAT_JUNK,
        Category::Unreferenced => CAT_UNREFERENCED,
    }
}

/// A glyph followed by a label, for buttons and rows.
pub(crate) fn labelled(glyph: &str, label: &str) -> String {
    format!("{glyph}  {label}")
}

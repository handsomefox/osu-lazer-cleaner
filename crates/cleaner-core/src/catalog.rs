//! What each cleaning category means.
//!
//! A category is data, not code: it names a role a file plays inside a beatmap set. The
//! scanner decides which role each file has, and the user decides which roles to remove.

use serde::{Deserialize, Serialize};

/// A kind of content that can be removed from a beatmap set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Video backdrops.
    Videos,
    /// `.osb` files and the images and samples only a storyboard uses.
    Storyboards,
    /// Background images.
    Backgrounds,
    /// Custom hit samples.
    Hitsounds,
    /// Files that override skin elements for one beatmap.
    SkinElements,
    /// Operating-system leftovers with no role at all.
    Junk,
    /// Blobs no remaining usage points at.
    Unreferenced,
}

impl Category {
    /// Every category, in the order the interface shows them.
    pub const ALL: &'static [Self] = &[
        Self::Videos,
        Self::Storyboards,
        Self::Backgrounds,
        Self::Hitsounds,
        Self::SkinElements,
        Self::Junk,
        Self::Unreferenced,
    ];

    /// Short label for the interface.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Videos => "Videos",
            Self::Storyboards => "Storyboards",
            Self::Backgrounds => "Backgrounds",
            Self::Hitsounds => "Hitsounds",
            Self::SkinElements => "Skin elements",
            Self::Junk => "Junk files",
            Self::Unreferenced => "Unreferenced files",
        }
    }

    /// Name used on the command line and in manifests.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Videos => "videos",
            Self::Storyboards => "storyboards",
            Self::Backgrounds => "backgrounds",
            Self::Hitsounds => "hitsounds",
            Self::SkinElements => "skin-elements",
            Self::Junk => "junk",
            Self::Unreferenced => "unreferenced",
        }
    }

    /// Parses a slug, as accepted on the command line.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.slug() == slug)
    }

    /// One sentence describing what removing this category does.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Videos => "Video backdrops. Beatmaps play normally without them.",
            Self::Storyboards => {
                "Storyboard scripts and the art only they use. Backgrounds are kept."
            }
            Self::Backgrounds => "Background images. Beatmaps play against a blank background.",
            Self::Hitsounds => "Custom hit samples. Beatmaps fall back to the default skin.",
            Self::SkinElements => {
                "Per-beatmap skin overrides. Beatmaps fall back to your active skin."
            }
            Self::Junk => "Operating-system leftovers such as thumbs.db and .DS_Store.",
            Self::Unreferenced => {
                "Files no beatmap, skin, or replay refers to any more. Always safe."
            }
        }
    }

    /// Whether the category starts selected.
    ///
    /// Only categories that cannot change how a beatmap plays are on by default. Hitsounds and
    /// skin elements are off because a mapper can rely on them for timing cues, and backgrounds
    /// are off because most people want to keep them.
    #[must_use]
    pub fn default_selected(self) -> bool {
        matches!(self, Self::Junk | Self::Unreferenced)
    }
}

/// Filenames with no role inside a beatmap set.
///
/// Matched case-insensitively against the last path component, which is how these files
/// actually appear after an archive import.
const JUNK_FILENAMES: &[&str] = &["thumbs.db", "desktop.ini", ".ds_store"];

/// Reports whether a filename is operating-system leftover.
#[must_use]
pub fn is_junk(filename: &str) -> bool {
    let name = filename.rsplit('/').next().unwrap_or(filename);
    JUNK_FILENAMES
        .iter()
        .any(|junk| name.eq_ignore_ascii_case(junk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_matches_case_insensitively_in_subdirectories() {
        assert!(is_junk("Thumbs.db"));
        assert!(is_junk("sb/desktop.ini"));
        assert!(is_junk(".DS_Store"));
        assert!(!is_junk("audio.mp3"));
    }

    #[test]
    fn slugs_round_trip() {
        for category in Category::ALL {
            assert_eq!(Category::from_slug(category.slug()), Some(*category));
        }
    }

    #[test]
    fn only_harmless_categories_are_selected_by_default() {
        for category in Category::ALL {
            if category.default_selected() {
                assert!(matches!(category, Category::Junk | Category::Unreferenced));
            }
        }
    }
}

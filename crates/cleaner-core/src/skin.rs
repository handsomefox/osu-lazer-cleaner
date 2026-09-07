//! Recognising per-beatmap skin overrides.
//!
//! A beatmap set has no manifest saying which of its files are skin elements. `LegacyBeatmapSkin`
//! serves the whole set as a resource store, so any file whose name matches a skin element
//! lookup name acts as an override. Recognising them means matching those names.
//!
//! The prefix list comes from `TcNo osu! Cleaner`'s `Form1.cs`, which collected the names `osu!`
//! actually looks up. It is a prefix match because most elements have numbered and `@2x`
//! variants: `hit300`, `hit300-0@2x.png`, `sliderb0.png`.

/// Filename prefixes that identify an `osu!` skin element.
///
/// Kept in the source order from `TcNo`'s regex so the two can be compared.
const SKIN_PREFIXES: &[&str] = &[
    "applause",
    "approachcircle",
    "button-",
    "combobreak",
    "comboburst",
    "count1",
    "count2",
    "count3",
    "count",
    "cursor",
    "default-",
    "failsound",
    "fail-background",
    "followpoint",
    "fruit-",
    "go.png",
    "go@2x.png",
    "gos.png",
    "gos@2x.png",
    "hit0",
    "hit100",
    "hit300",
    "hit50",
    "hitcircle",
    "inputoverlay-",
    "lighting",
    "mania-",
    "menu.",
    "menu-",
    "particle100",
    "particle300",
    "particle50",
    "pause-",
    "pippidon",
    "play-",
    "ranking-",
    "ready",
    "reversearrow",
    "score-",
    "scorebar-",
    "sectionfail",
    "sectionpass",
    "section-",
    "selection-",
    "sliderb",
    "sliderfollowcircle",
    "sliderscorepoint",
    "spinnerbonus",
    "spinner-",
    "spinnerspin",
    "star.png",
    "star@2x.png",
    "star2.png",
    "star2@2x.png",
    "taiko-",
    "taikobigcircle",
    "taikohitcircle",
];

/// A beatmap-local skin configuration file.
///
/// `skin.ini` inside a beatmap set configures per-beatmap skinning, so it belongs to the skin
/// category rather than being junk.
const SKIN_CONFIG: &str = "skin.ini";

/// Reports whether a filename looks like a skin element override.
///
/// Compares the last path component, case-insensitively, as `osu!`'s lookups do.
#[must_use]
pub fn is_skin_element(filename: &str) -> bool {
    let name = filename.rsplit('/').next().unwrap_or(filename);
    let lowered = name.to_ascii_lowercase();

    if lowered == SKIN_CONFIG {
        return true;
    }

    SKIN_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_common_skin_elements() {
        for name in [
            "hitcircle.png",
            "hitcircleoverlay@2x.png",
            "approachcircle.png",
            "sliderb0.png",
            "spinner-circle.png",
            "mania-key1.png",
            "skin.ini",
            "SKIN.INI",
        ] {
            assert!(is_skin_element(name), "{name} should be a skin element");
        }
    }

    #[test]
    fn leaves_beatmap_content_alone() {
        for name in [
            "audio.mp3",
            "bg.jpg",
            "video.mp4",
            "storyboard.osb",
            "Artist - Title (Mapper) [Insane].osu",
            "soft-hitwhistle2.wav",
        ] {
            assert!(!is_skin_element(name), "{name} is not a skin element");
        }
    }

    #[test]
    fn matches_inside_subdirectories() {
        assert!(is_skin_element("elements/hit300.png"));
    }
}

//! Parsing `.osu` and `.osb` files for the files they reference.
//!
//! Every rule here mirrors osu!lazer's own decoders. The comments name the source file so the
//! two can be compared when lazer changes.

use std::collections::BTreeSet;

/// The filenames one beatmap set owns, indexed the way lazer looks them up.
///
/// Built once per set. Before this existed, every lookup walked the whole file list and folded
/// each name as it went, and `hitsounds` did that 36 times for every timing point and every hit
/// object naming a custom sample index. A set with a thousand files and a difficulty with
/// thousands of hit objects turned that into hundreds of millions of comparisons.
pub struct FileIndex {
    /// Owned names, spelled as the set spells them, in the order a sorted set gives.
    ///
    /// Ordered rather than hashed because `read_animation` walks the whole list looking for
    /// numbered frames, and a stable order keeps a scan's output stable.
    exact: BTreeSet<String>,
    /// Folded key to the owned spelling.
    folded: std::collections::HashMap<String, String>,
    /// Hitsound names already synthesised for a custom sample index.
    ///
    /// The answer depends only on the index and the files the set owns, so it is worth keeping
    /// for the rest of the set. One set is parsed by one thread, so a `RefCell` is enough.
    hitsounds: std::cell::RefCell<std::collections::HashMap<u32, Vec<String>>>,
}

impl FileIndex {
    /// Indexes the names a set owns.
    #[must_use]
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut exact = BTreeSet::new();
        let mut folded = std::collections::HashMap::new();
        for name in names {
            folded
                .entry(filename_key(name))
                .or_insert_with(|| name.to_owned());
            exact.insert(name.to_owned());
        }
        Self {
            exact,
            folded,
            hitsounds: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Finds a filename in the set, comparing case-insensitively as lazer's lookups do.
    ///
    /// Returns the name as the set spells it, so callers can match it against realm entries.
    fn lookup(&self, name: &str) -> Option<String> {
        if self.exact.contains(name) {
            return Some(name.to_owned());
        }
        self.folded.get(&filename_key(name)).cloned()
    }

    /// Every name the set owns.
    fn names(&self) -> impl Iterator<Item = &String> {
        self.exact.iter()
    }

    /// The hitsound filenames a custom sample index can refer to.
    ///
    /// `HitSampleInfo.LookupNames` resolves a sample to `{bank}-{name}{suffix}`, where the
    /// suffix is the custom index when it is not zero. The resulting filename never appears in
    /// the `.osu` text, so the candidates have to be generated and matched against the files
    /// the set owns.
    fn hitsounds(&self, index: u32) -> Vec<String> {
        if let Some(names) = self.hitsounds.borrow().get(&index) {
            return names.clone();
        }

        let suffix = if index <= 1 {
            String::new()
        } else {
            index.to_string()
        };

        let mut names = Vec::new();
        for bank in SAMPLE_BANKS {
            for sound in HIT_SOUNDS {
                for extension in AUDIO_EXTENSIONS {
                    if let Some(owned) = self.lookup(&format!("{bank}-{sound}{suffix}{extension}"))
                    {
                        names.push(owned);
                    }
                }
            }
        }

        self.hitsounds.borrow_mut().insert(index, names.clone());
        names
    }
}

/// Key for a filename lookup, independent of case and path separators.
///
/// `RealmBackedResourceStore` standardises paths and lowercases both lookup names and owned
/// filenames. `BeatmapSetInfoExtensions.GetFile` also compares without regard to case.
pub(crate) fn filename_key(name: &str) -> String {
    name.replace('\\', "/").to_lowercase()
}

/// Video container extensions lazer recognises.
///
/// From `SupportedExtensions.VIDEO_EXTENSIONS`.
pub const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".mov", ".avi", ".flv", ".mpg", ".wmv", ".m4v"];

/// Whether `name` has one of the [`VIDEO_EXTENSIONS`], compared the way
/// `Path.GetExtension(filename).ToLowerInvariant()` does: from the last dot of
/// the last path component.
fn is_video_file(name: &str) -> bool {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    file.rfind('.').is_some_and(|dot| {
        VIDEO_EXTENSIONS
            .iter()
            .any(|extension| file[dot..].eq_ignore_ascii_case(extension))
    })
}

/// Audio extensions lazer recognises.
///
/// From `SupportedExtensions.AUDIO_EXTENSIONS`.
pub const AUDIO_EXTENSIONS: &[&str] = &[".mp3", ".ogg", ".wav"];

/// Image extensions lazer recognises, in the order it tries them.
///
/// The order matters: `Storyboard.GetStoragePathFromStoryboardPath` appends each in turn to a
/// storyboard reference that has no extension, and stops at the first file that exists.
pub const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png"];

/// Sample bank names that can prefix a hitsound filename.
///
/// From `HitSampleInfo.BANK_NORMAL`, `BANK_SOFT`, and `BANK_DRUM`.
const SAMPLE_BANKS: &[&str] = &["normal", "soft", "drum"];

/// Hit sound names that can follow a bank in a hitsound filename.
///
/// From `HitSampleInfo.HIT_NORMAL`, `HIT_WHISTLE`, `HIT_FINISH`, and `HIT_CLAP`.
const HIT_SOUNDS: &[&str] = &["hitnormal", "hitwhistle", "hitfinish", "hitclap"];

/// What a beatmap set's files are referenced as.
///
/// A file can hold several roles at once. A background that a storyboard also draws as a
/// sprite is both a background and a storyboard asset, which is why these are separate sets
/// rather than one classification per file.
#[derive(Debug, Default, Clone)]
pub struct References {
    /// Audio track named by `[General] AudioFilename`.
    pub audio: BTreeSet<String>,
    /// Images named by `Background` events.
    pub backgrounds: BTreeSet<String>,
    /// Files named by `Video` events.
    pub videos: BTreeSet<String>,
    /// Files drawn or played by storyboard events.
    pub storyboard: BTreeSet<String>,
    /// Hitsound files, both named outright and synthesised from sample banks.
    pub hitsounds: BTreeSet<String>,
}

impl References {
    /// Merges another set of references into this one.
    pub fn absorb(&mut self, other: Self) {
        self.audio.extend(other.audio);
        self.backgrounds.extend(other.backgrounds);
        self.videos.extend(other.videos);
        self.storyboard.extend(other.storyboard);
        self.hitsounds.extend(other.hitsounds);
    }
}

/// Which part of a beatmap set a file's text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A `.osu` difficulty file.
    Difficulty,
    /// A `.osb` storyboard file.
    Storyboard,
}

/// Extracts every file a difficulty or storyboard refers to.
///
/// `known` is the set of filenames the beatmap set actually owns. It resolves the two
/// cases where a reference does not name a file directly: storyboard references without an
/// extension, and hitsounds implied by a sample bank index.
#[must_use]
pub fn parse(text: &str, kind: SourceKind, known: &FileIndex) -> References {
    parse_with(text, kind, known, &Variables::new(), &variables(text))
}

/// Storyboard variables, `$name=value`, in the order they were first defined. A later
/// definition of the same name replaces the value and keeps the place, as the dictionary in
/// `LegacyStoryboardDecoder` does.
type Variables = Vec<(String, String)>;

/// The `[Variables]` a source defines.
///
/// `LegacyStoryboardDecoder` reads a line as `key=value`, splitting at the first `=` without
/// trimming either side, after dropping a `//` comment that does not start the line and any
/// trailing whitespace.
fn variables(text: &str) -> Variables {
    let mut variables = Variables::new();
    let mut section = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_ascii_lowercase();
            continue;
        }
        if section == "variables" {
            define_from(&mut variables, line);
        }
    }
    variables
}

/// Defines the variable on one line of a `[Variables]` section.
fn define_from(variables: &mut Variables, line: &str) {
    let line = match line.find("//") {
        Some(index) if index > 0 => &line[..index],
        _ => line,
    };
    let (key, value) = line
        .trim_end()
        .split_once('=')
        .unwrap_or((line.trim_end(), ""));
    define(variables, key, value);
}

fn define(variables: &mut Variables, key: &str, value: &str) {
    match variables.iter_mut().find(|(name, _)| name == key) {
        Some((_, existing)) => value.clone_into(existing),
        None => variables.push((key.to_owned(), value.to_owned())),
    }
}

/// Substitutes each variable once, in definition order, as `decodeVariables` does. An empty
/// name is skipped: .NET refuses to replace an empty string, so osu!lazer drops that line.
fn substitute<'a>(line: &'a str, variables: &Variables) -> std::borrow::Cow<'a, str> {
    if !line.contains('$') || variables.is_empty() {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut line = line.to_owned();
    for (name, value) in variables {
        if !name.is_empty() {
            line = line.replace(name.as_str(), value);
        }
    }
    std::borrow::Cow::Owned(line)
}

/// [`parse`], substituting variables into event lines first.
///
/// The game substitutes the variables defined so far: `start`, then each definition in `text`
/// as it comes. A later definition of the same name changes what the lines after it name. Each
/// event line is read that way and also with `last`, the variables in force at the end, so a
/// line before its definition still counts. That finds every file the game draws, and perhaps
/// a few it does not, which only keeps more files.
fn parse_with(
    text: &str,
    kind: SourceKind,
    known: &FileIndex,
    start: &Variables,
    last: &Variables,
) -> References {
    let mut references = References::default();
    let mut section = String::new();
    let mut variables = start.clone();
    let read_events = |line: &str, variables: &Variables, references: &mut References| {
        let in_order = substitute(line, variables);
        read_event(&in_order, known, references);
        let at_end = substitute(line, last);
        if at_end != in_order {
            read_event(&at_end, known, references);
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_ascii_lowercase();
            continue;
        }

        match section.as_str() {
            "general" => read_general(trimmed, &mut references),
            "variables" => define_from(&mut variables, line),
            "events" => read_events(line, &variables, &mut references),
            "hitobjects" if kind == SourceKind::Difficulty => {
                read_hit_object(trimmed, known, &mut references);
            }
            "timingpoints" if kind == SourceKind::Difficulty => {
                read_timing_point(trimmed, known, &mut references);
            }
            // A `.osb` has no section header before its events, so treat bare lines as events.
            "" if kind == SourceKind::Storyboard => {
                read_events(line, &variables, &mut references);
            }
            _ => {}
        }
    }

    references
}

/// Parses every difficulty and storyboard in a set, merging their references.
///
/// osu!lazer decodes a storyboard with each difficulty, the difficulty first, through one
/// decoder, so a `.osb` sees the variables of whichever difficulty is played as well as its own.
/// The storyboard is therefore read once on its own and once with each difficulty's variables,
/// starting from those the difficulty ends with.
#[must_use]
pub fn parse_all<'a>(
    sources: impl IntoIterator<Item = (&'a str, SourceKind)>,
    known: &FileIndex,
) -> References {
    let sources: Vec<(&str, SourceKind, Variables)> = sources
        .into_iter()
        .map(|(text, kind)| (text, kind, variables(text)))
        .collect();
    let mut references = References::default();
    for (text, kind, own) in &sources {
        references.absorb(parse_with(text, *kind, known, &Variables::new(), own));
        if *kind != SourceKind::Storyboard {
            continue;
        }
        for (_, _, difficulty) in sources
            .iter()
            .filter(|(_, kind, vars)| *kind == SourceKind::Difficulty && !vars.is_empty())
        {
            let mut combined = difficulty.clone();
            for (name, value) in own {
                define(&mut combined, name, value);
            }
            references.absorb(parse_with(text, *kind, known, difficulty, &combined));
        }
    }
    references
}

/// Reads the `[General]` keys that name files.
fn read_general(line: &str, references: &mut References) {
    if let Some(value) = line.strip_prefix("AudioFilename:") {
        let name = clean_filename(value);
        if !name.is_empty() {
            references.audio.insert(name);
        }
    }
}

/// Reads one `[Events]` line.
///
/// Event type codes come from `LegacyEventType`: `Background = 0`, `Video = 1`, `Sprite = 4`,
/// `Sample = 5`, `Animation = 6`. The filename sits at a different field for each.
fn read_event(line: &str, known: &FileIndex, references: &mut References) {
    // Command lines inside a storyboard are indented with spaces or underscores and never
    // name a file.
    if line.starts_with([' ', '_']) {
        return;
    }

    let fields: Vec<&str> = line.trim().split(',').collect();
    let Some(kind) = fields.first() else {
        return;
    };

    match kind.trim() {
        "0" | "Background" => {
            if let Some(name) = field(&fields, 2) {
                references.backgrounds.insert(name);
            }
        }
        "1" | "Video" => {
            // Some very old beatmaps typed their background as a video.
            // `LegacyBeatmapDecoder` takes a `Video` event whose file is not a
            // video as the background, so a Videos clean must leave it alone.
            if let Some(name) = field(&fields, 2) {
                if is_video_file(&name) {
                    references.videos.insert(name);
                } else {
                    references.backgrounds.insert(name);
                }
            }
        }
        "4" | "Sprite" => {
            if let Some(name) = field(&fields, 3) {
                references
                    .storyboard
                    .extend(resolve_extension(&name, known));
            }
        }
        "5" | "Sample" => {
            if let Some(name) = field(&fields, 3) {
                references.storyboard.insert(name);
            }
        }
        "6" | "Animation" => read_animation(&fields, known, references),
        _ => {}
    }
}

/// Expands an `Animation` event into the frame files it actually loads.
///
/// The literal path in the event is never opened.
/// `DrawableStoryboardAnimation.addFramesFromStoryboardSource` builds each frame with
/// `Animation.Path.Replace(".", $"{i}.")`, so `sb/foo.png` with three frames means
/// `sb/foo0.png`, `sb/foo1.png`, and `sb/foo2.png`. Missing this is the difference between
/// keeping an animation and deleting every frame of it.
fn read_animation(fields: &[&str], known: &FileIndex, references: &mut References) {
    let Some(path) = field(fields, 3) else {
        return;
    };

    let frame_count = field(fields, 6)
        .and_then(|f| f.parse::<usize>().ok())
        .unwrap_or(0);

    // Keep the literal path too: some storyboards ship it alongside the numbered frames.
    references
        .storyboard
        .extend(resolve_extension(&path, known));

    let Some((prefix, _)) = path.split_once('.') else {
        return;
    };
    // Work is bounded by the files the set owns, even if an untrusted storyboard claims
    // billions of frames. lazer inserts the frame number before every dot in the path.
    for owned in known.names() {
        let Some(head) = owned.get(..prefix.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(prefix) {
            continue;
        }
        let Some(number) = owned[prefix.len()..].split('.').next() else {
            continue;
        };
        let Ok(frame) = number.parse::<usize>() else {
            continue;
        };
        if frame < frame_count
            && path
                .replace('.', &format!("{frame}."))
                .eq_ignore_ascii_case(owned)
        {
            references.storyboard.insert(owned.clone());
        }
    }
}

/// Reads a hit object's trailing sample field.
///
/// The final comma-separated field is `normalSet:additionSet:index:volume:filename`, per
/// `ConvertHitObjectParser.readCustomSampleBanks`. An explicit filename in the fifth position
/// names a file outright. Otherwise the sample set and index imply one.
fn read_hit_object(line: &str, known: &FileIndex, references: &mut References) {
    let Some(sample) = line.rsplit(',').next() else {
        return;
    };

    let parts: Vec<&str> = sample.split(':').collect();
    if parts.len() < 2 {
        return;
    }

    if let Some(name) = parts.get(4) {
        let name = clean_filename(name);
        if !name.is_empty() {
            references.hitsounds.insert(name);
            return;
        }
    }

    let index = parts.get(2).and_then(|i| i.trim().parse::<u32>().ok());
    if let Some(index) = index {
        references.hitsounds.extend(known.hitsounds(index));
    }
}

/// Reads a timing point's custom sample index.
///
/// Fields are `time,beatLength,meter,sampleSet,sampleIndex,volume,uninherited,effects`. The
/// custom sample index at position 4 implies hitsound filenames that appear nowhere in the
/// file's text.
fn read_timing_point(line: &str, known: &FileIndex, references: &mut References) {
    let fields: Vec<&str> = line.split(',').collect();
    let Some(index) = fields.get(4).and_then(|i| i.trim().parse::<u32>().ok()) else {
        return;
    };

    references.hitsounds.extend(known.hitsounds(index));
}

/// Resolves a storyboard reference that may have no extension.
///
/// `Storyboard.GetStoragePathFromStoryboardPath` uses the path as-is when it has an extension,
/// and otherwise tries each image extension in turn. A file can therefore be live without its
/// name appearing verbatim anywhere in the storyboard.
fn resolve_extension(path: &str, known: &FileIndex) -> Vec<String> {
    if let Some(owned) = known.lookup(path) {
        return vec![owned];
    }

    if std::path::Path::new(path).extension().is_some() {
        // Named a file the set does not own. Keep it so callers see the reference.
        return vec![path.to_owned()];
    }

    IMAGE_EXTENSIONS
        .iter()
        .filter_map(|extension| known.lookup(&format!("{path}{extension}")))
        .take(1)
        .collect()
}

/// Reads a comma-separated field and cleans it, returning `None` when empty.
fn field(fields: &[&str], index: usize) -> Option<String> {
    let name = clean_filename(fields.get(index)?);
    (!name.is_empty()).then_some(name)
}

/// Normalises a filename the way `LegacyDecoder.CleanFilename` does.
///
/// Unescapes backslashes, strips surrounding quotes, and converts separators to forward
/// slashes, which is how `RealmNamedFileUsage.Filename` stores them.
fn clean_filename(raw: &str) -> String {
    raw.trim()
        .replace("\\\\", "\\")
        .trim_matches('"')
        .replace('\\', "/")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> FileIndex {
        FileIndex::new(names.iter().copied())
    }

    #[test]
    fn reads_background_and_video_events() {
        let text =
            "[Events]\n//Background and Video events\n0,0,\"bg.jpg\",0,0\nVideo,0,\"intro.mp4\"\n";
        let refs = parse(text, SourceKind::Difficulty, &files(&[]));

        assert!(refs.backgrounds.contains("bg.jpg"));
        assert!(refs.videos.contains("intro.mp4"));
    }

    #[test]
    fn a_video_event_naming_an_image_is_the_background() {
        let text = "[Events]\n1,0,\"BG.JPG\"\nVideo,0,\"sb/intro.MP4\"\n1,0,\"clip.mp4.png\"\n";
        let refs = parse(text, SourceKind::Difficulty, &files(&[]));

        assert!(refs.backgrounds.contains("BG.JPG"));
        assert!(refs.backgrounds.contains("clip.mp4.png"));
        assert!(refs.videos.contains("sb/intro.MP4"));
        assert_eq!(refs.videos.len(), 1);
    }

    #[test]
    fn reads_audio_filename() {
        let refs = parse(
            "[General]\nAudioFilename: audio.mp3\n",
            SourceKind::Difficulty,
            &files(&[]),
        );
        assert!(refs.audio.contains("audio.mp3"));
    }

    #[test]
    fn animation_expands_to_numbered_frames() {
        let known = files(&["sb/foo0.png", "sb/foo1.png", "sb/foo2.png"]);
        let refs = parse(
            "[Events]\nAnimation,Foreground,Centre,\"sb/foo.png\",320,240,3,50\n",
            SourceKind::Difficulty,
            &known,
        );

        for frame in ["sb/foo0.png", "sb/foo1.png", "sb/foo2.png"] {
            assert!(refs.storyboard.contains(frame), "missing frame {frame}");
        }
    }

    #[test]
    fn animation_work_is_bounded_by_owned_files() {
        let known = files(&["sb/foo0.png", "sb/foo999999.png", "other.png"]);
        let text = format!(
            "[Events]\nAnimation,Foreground,Centre,\"sb/foo.png\",0,0,{},50\n",
            usize::MAX
        );
        let refs = parse(&text, SourceKind::Difficulty, &known);
        assert!(refs.storyboard.contains("sb/foo999999.png"));
        assert!(!refs.storyboard.contains("other.png"));
        assert!(refs.storyboard.len() <= known.names().count());
    }

    #[test]
    fn extensionless_sprite_resolves_against_owned_files() {
        let known = files(&["sb/flash.png"]);
        let refs = parse(
            "[Events]\nSprite,Foreground,Centre,\"sb/flash\",320,240\n",
            SourceKind::Difficulty,
            &known,
        );

        assert!(refs.storyboard.contains("sb/flash.png"));
    }

    #[test]
    fn custom_sample_index_implies_hitsound_filenames() {
        // The filename appears nowhere in the text; only the index 2 does.
        let known = files(&["soft-hitwhistle2.wav", "normal-hitnormal2.ogg"]);
        let refs = parse(
            "[TimingPoints]\n0,500,4,2,2,60,1,0\n",
            SourceKind::Difficulty,
            &known,
        );

        assert!(refs.hitsounds.contains("soft-hitwhistle2.wav"));
        assert!(refs.hitsounds.contains("normal-hitnormal2.ogg"));
    }

    #[test]
    fn explicit_hit_object_sample_filename_is_read() {
        let refs = parse(
            "[HitObjects]\n256,192,1000,1,0,0:0:0:0:custom.wav\n",
            SourceKind::Difficulty,
            &files(&[]),
        );
        assert!(refs.hitsounds.contains("custom.wav"));
    }

    #[test]
    fn storyboard_variables_are_substituted_before_reading_events() {
        let known = FileIndex::new(["star.png", "glow.png", "bg.jpg"]);
        let storyboard = "[Variables]\n$sprite=star.png\n$layer=Foreground\n\n[Events]\n\
                          Sprite,$layer,Centre,\"$sprite\",320,240\n";
        let references = parse(storyboard, SourceKind::Storyboard, &known);
        assert!(references.storyboard.contains("star.png"), "{references:?}");

        // A storyboard can use a variable only the difficulty defines.
        let difficulty = "[Variables]\n$glow=glow.png\n[Events]\n0,0,\"bg.jpg\",0,0\n";
        let uses = "[Events]\nSprite,Foreground,Centre,\"$glow\",320,240\n";
        let references = parse_all(
            [
                (difficulty, SourceKind::Difficulty),
                (uses, SourceKind::Storyboard),
            ],
            &known,
        );
        assert!(references.storyboard.contains("glow.png"), "{references:?}");
        assert!(references.backgrounds.contains("bg.jpg"));
    }

    #[test]
    fn a_redefined_variable_keeps_what_it_named_before() {
        let known = FileIndex::new(["star.png", "glow.png", "early.png", "late.png"]);
        // osu!lazer substitutes the definitions read so far, so the first event draws
        // star.png and the second glow.png.
        let storyboard = "[Variables]\n$sprite=star.png\n[Events]\n\
                          Sprite,Foreground,Centre,\"$sprite\",320,240\n\
                          [Variables]\n$sprite=glow.png\n[Events]\n\
                          Sprite,Foreground,Centre,\"$sprite\",320,240\n";
        let references = parse(storyboard, SourceKind::Storyboard, &known);
        assert!(references.storyboard.contains("star.png"), "{references:?}");
        assert!(references.storyboard.contains("glow.png"), "{references:?}");

        // A storyboard starts from the variables its difficulty ends with.
        let difficulty = "[Variables]\n$s=early.png\n";
        let uses = "[Events]\nSprite,Foreground,Centre,\"$s\",320,240\n\
                    [Variables]\n$s=late.png\n[Events]\n\
                    Sprite,Foreground,Centre,\"$s\",320,240\n";
        let references = parse_all(
            [
                (difficulty, SourceKind::Difficulty),
                (uses, SourceKind::Storyboard),
            ],
            &known,
        );
        assert!(
            references.storyboard.contains("early.png"),
            "{references:?}"
        );
        assert!(references.storyboard.contains("late.png"), "{references:?}");
    }

    #[test]
    fn storyboard_commands_are_not_filenames() {
        let text = "[Events]\nSprite,Foreground,Centre,\"sb/a.png\",0,0\n F,0,0,1000,0,1\n";
        let refs = parse(text, SourceKind::Difficulty, &files(&["sb/a.png"]));

        assert_eq!(refs.storyboard.len(), 1);
        assert!(refs.storyboard.contains("sb/a.png"));
    }

    #[test]
    fn osb_events_parse_without_a_section_header() {
        let known = files(&["sb/bg.png"]);
        let refs = parse(
            "Sprite,Background,Centre,\"sb/bg.png\",320,240\n",
            SourceKind::Storyboard,
            &known,
        );
        assert!(refs.storyboard.contains("sb/bg.png"));
    }

    #[test]
    fn filenames_are_cleaned_like_lazer_does() {
        assert_eq!(clean_filename("\"sb\\\\a.png\""), "sb/a.png");
        assert_eq!(clean_filename("  plain.jpg  "), "plain.jpg");
    }
}

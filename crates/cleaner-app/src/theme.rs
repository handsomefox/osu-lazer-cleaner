//! Colours, type, and spacing.
//!
//! The palette sits beside osu!lazer, so it is dark, and it borrows the game's pink. That pink
//! carries exactly one meaning throughout the window: this is leaving the library. Nothing that
//! stays is ever pink, so a glance at the screen answers "what am I about to remove".

use eframe::egui;

/// Window background. Purple-grey rather than black, to sit beside osu!lazer without pretending
/// to be part of it.
pub(crate) const BASE: egui::Color32 = egui::Color32::from_rgb(0x22, 0x1c, 0x20);

/// Raised surfaces: the header, the status bar, striped rows.
pub(crate) const SURFACE: egui::Color32 = egui::Color32::from_rgb(0x2b, 0x24, 0x29);

/// Rules and control outlines.
pub(crate) const LINE: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x31, 0x38);

/// Body text.
pub(crate) const TEXT: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xe0, 0xe4);

/// Supporting text: descriptions, hints, units.
pub(crate) const MUTED: egui::Color32 = egui::Color32::from_rgb(0x96, 0x8a, 0x91);

/// osu!'s pink. Means "this is leaving", and means nothing else.
pub(crate) const REMOVE: egui::Color32 = egui::Color32::from_rgb(0xff, 0x66, 0xaa);

/// A dimmer pink for the pressed and hovered states of pink controls.
pub(crate) const REMOVE_DIM: egui::Color32 = egui::Color32::from_rgb(0xd1, 0x4f, 0x89);

/// What stays: the part of the library a clean would not touch.
pub(crate) const KEEP: egui::Color32 = egui::Color32::from_rgb(0x4d, 0x42, 0x49);

/// Confirmation that something worked.
pub(crate) const GOOD: egui::Color32 = egui::Color32::from_rgb(0x7d, 0xd6, 0xa8);

/// Something went wrong.
pub(crate) const BAD: egui::Color32 = egui::Color32::from_rgb(0xff, 0x8b, 0x7d);

/// Applies the fonts, the palette, and the type scale.
pub(crate) fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(style);
}

// The vendored Inter files have their Private Use Area cmap entries stripped (upstream Inter
// maps ~1.5k stylistic-set alternates into U+E000..U+F8FF, which would shadow the Phosphor icon
// glyphs that live in the same range because Inter sits earlier in the family). Re-vendoring
// Inter from upstream without stripping them turns every icon into a Latin alternate.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "inter-bold".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Bold.ttf")).into(),
    );
    fonts.font_data.insert(
        "phosphor".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/Phosphor.ttf")).into(),
    );

    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    proportional.insert(0, "inter".to_owned());
    // Phosphor is a fallback, so glyphs render inside ordinary text runs.
    proportional.insert(1, "phosphor".to_owned());
    fonts.families.insert(
        egui::FontFamily::Name("bold".into()),
        vec!["inter-bold".to_owned(), "phosphor".to_owned()],
    );

    ctx.set_fonts(fonts);
}

/// The bold Inter family, for headings and the numbers worth reading first.
pub(crate) fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Fills in the palette and type scale on a style.
fn style(style: &mut egui::Style) {
    // A small scale with clear steps. The 26pt figure is for the library total, which is the
    // one number people open this for.
    style.text_styles = [
        (
            egui::TextStyle::Name("Figure".into()),
            egui::FontId::new(26.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Heading,
            egui::FontId::new(16.0, egui::FontFamily::Proportional),
        ),
        // The category names, which are the thing being chosen between.
        (
            egui::TextStyle::Name("Row".into()),
            egui::FontId::new(15.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Small,
            egui::FontId::new(11.5, egui::FontFamily::Proportional),
        ),
        // Numbers only. Tabular figures let byte counts line up and compare down a column.
        (
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        ),
    ]
    .into();

    // Labels are chrome, not text to copy. Without this, dragging across the library path in
    // the header leaves it highlighted in the selection colour, which reads as an error.
    style.interaction.selectable_labels = false;

    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);

    let visuals = &mut style.visuals;
    visuals.dark_mode = true;
    visuals.panel_fill = BASE;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = BASE;
    visuals.faint_bg_color = SURFACE;
    visuals.override_text_color = Some(TEXT);
    visuals.window_stroke = egui::Stroke::new(1.0, LINE);
    visuals.selection.bg_fill = REMOVE_DIM;
    visuals.selection.stroke = egui::Stroke::new(1.0, TEXT);
    visuals.hyperlink_color = REMOVE;

    // Flat controls. Depth comes from the composition bar, not from shadows on everything.
    visuals.window_shadow = egui::epaint::Shadow::NONE;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;

    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(4);

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, LINE);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, MUTED);

    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.weak_bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);

    visuals.widgets.hovered.bg_fill = LINE;
    visuals.widgets.hovered.weak_bg_fill = LINE;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, REMOVE_DIM);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);

    visuals.widgets.active.bg_fill = REMOVE_DIM;
    visuals.widgets.active.weak_bg_fill = REMOVE_DIM;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, REMOVE);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, TEXT);
}

/// Text in the row size, for a category name.
pub(crate) fn row(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).text_style(egui::TextStyle::Name("Row".into()))
}

/// Text in the figure size, for the library total.
pub(crate) fn figure(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).text_style(egui::TextStyle::Name("Figure".into()))
}

/// A number, set in the tabular face so columns of them align.
pub(crate) fn number(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).monospace()
}

/// Groups a count in threes, because six-figure file counts are unreadable run together.
///
/// This stays in the window rather than in `cleaner_core`, where the same counts are printed
/// for scripts to parse.
pub(crate) fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::grouped;

    #[test]
    fn groups_digits_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(154_002), "154,002");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}

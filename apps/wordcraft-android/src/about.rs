//! Help › About, as Folio Writer: its name and sources, the licences of everything it ships, and
//! WordCraft's contributors. The ArtCraft brand licence (upstream's `docs/brand/LICENSE-brand.txt`) keeps its marks out
//! of modified versions, so the host shows this instead of the app's own About (which names ArtCraft);
//! the copyright notices stay, as the licences require.

use egui::{Id, Modal, RichText, ScrollArea};
use wordcraft_ui_egui::WordApp;
use wordcraft_ui_egui::dialogs::Dialog;

const SOURCE: &str = "https://github.com/luthfy13/wordcraft-android";
const UPSTREAM: &str = "https://github.com/storytold/wordcraft";

/// Licence texts, as shipped.
const TEXTS: &[(&str, &str)] = &[
    ("Folio Writer and WordCraft: NOTICE", include_str!("../../../NOTICE")),
    ("MIT License", include_str!("../../../LICENSE-MIT")),
    ("Apache License 2.0", include_str!("../../../LICENSE-APACHE")),
    ("Inter: SIL Open Font License 1.1", include_str!("../../../assets/fonts/OFL-Inter.txt")),
    ("JetBrains Mono: SIL Open Font License 1.1", include_str!("../../../assets/fonts/OFL-JetBrainsMono.txt")),
    ("Source Sans 3: SIL Open Font License 1.1", include_str!("../../../assets/fonts/OFL-SourceSans3.txt")),
    ("Source Serif 4: SIL Open Font License 1.1", include_str!("../../../assets/fonts/OFL-SourceSerif4.txt")),
];

/// The Rust crates in the build and their licences (`ci/licenses.py`).
const CRATES: &str = include_str!("../licenses/crates.txt");

/// The Android libraries in the APK.
const ANDROID: &[(&str, &str)] = &[
    ("AndroidX AppCompat, Core, Activity and their dependencies", "Apache-2.0"),
    ("Android Games Activity (GameActivity, GameTextInput)", "Apache-2.0"),
    ("Kotlin standard library", "Apache-2.0"),
];

#[derive(Default)]
pub struct About {
    /// Open, on this tab.
    tab: Option<u8>,
}

impl About {
    /// The app asked for its About: this one opens instead.
    pub fn intercept(&mut self, app: &mut WordApp) {
        if let Some(Dialog::About { tab }) = &app.dialog {
            self.tab = Some((*tab).min(2));
            app.dialog = None;
        }
    }

    pub fn is_open(&self) -> bool {
        self.tab.is_some()
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tab else { return };
        let mut close = false;
        let mut next = tab;
        Modal::new(Id::new("folio-about")).show(ctx, |ui| {
            ui.set_width(660.0_f32.min(ctx.content_rect().width() - 48.0).max(240.0));
            ui.horizontal(|ui| {
                for (i, label) in ["About", "Licenses", "Contributors"].into_iter().enumerate() {
                    let i = u8::try_from(i).unwrap_or(0);
                    if ui.selectable_label(tab == i, label).clicked() {
                        next = i;
                    }
                }
            });
            ui.separator();
            let height = (ctx.content_rect().height() * 0.6).max(160.0);
            match tab {
                1 => {
                    ScrollArea::vertical().max_height(height).show(ui, licenses);
                }
                2 => {
                    ScrollArea::vertical().max_height(height).show(ui, wordcraft_ui_egui::credits::contributors_ui);
                }
                _ => about(ui),
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.tab = None;
        } else {
            self.tab = Some(next);
        }
    }
}

fn about(ui: &mut egui::Ui) {
    ui.label(RichText::new("Folio Writer").heading());
    let build = option_env!("WORDCRAFT_BUILD_SHA").map(|s| s.get(..8).unwrap_or(s)).unwrap_or("development build");
    ui.label(format!("Version {} ({build})", env!("CARGO_PKG_VERSION")));
    ui.label("A word processor for Android tablets, compatible with .docx, .odt, .rtf, Markdown and text.");
    ui.label("Based on WordCraft by the ArtCraft team, a free, open-source word processor written in Rust.");
    ui.add_space(6.0);
    ui.hyperlink_to("Source code: github.com/luthfy13/wordcraft-android", SOURCE);
    ui.hyperlink_to("WordCraft: github.com/storytold/wordcraft", UPSTREAM);
    ui.add_space(6.0);
    ui.label(
        RichText::new("MIT OR Apache-2.0. Copyright (c) 2026 ArtCraft Team, the WordCraft contributors and the Folio Writer contributors.")
            .small()
            .weak(),
    );
}

fn licenses(ui: &mut egui::Ui) {
    ui.label("Folio Writer and WordCraft are released under the MIT License or the Apache License 2.0, at your option. They include:");
    ui.add_space(4.0);
    for (name, text) in TEXTS {
        egui::CollapsingHeader::new(*name).show(ui, |ui| {
            ui.label(RichText::new(*text).monospace().small());
        });
    }
    egui::CollapsingHeader::new("Noto Emoji and Ubuntu fonts in the interface (egui): SIL Open Font License 1.1, Ubuntu Font Licence 1.0").show(
        ui,
        |ui| {
            ui.label("Bundled with egui's default fonts; see crate epaint_default_fonts below.");
        },
    );
    egui::CollapsingHeader::new("Android libraries").show(ui, |ui| {
        for (name, licence) in ANDROID {
            ui.label(format!("{name}: {licence}"));
        }
    });
    egui::CollapsingHeader::new(format!("Rust crates ({})", CRATES.lines().count())).show(ui, |ui| {
        ui.label(RichText::new(CRATES).monospace().small());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_licences_are_all_there() {
        assert!(TEXTS.iter().all(|(_, t)| t.len() > 500), "every text is complete");
        assert!(CRATES.lines().count() > 100);
        assert!(CRATES.lines().any(|l| l.starts_with("egui ")));
    }
}

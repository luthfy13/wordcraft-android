//! What the host itself shows on top of the app: the question when a document changed elsewhere, and the
//! offer to share a file just exported. Drawn with egui in the app's own style.

use std::path::PathBuf;

use egui::{Align2, Id, Modal, RichText};
use serde_json::json;
use wordcraft_ui_egui::WordApp;

use crate::files::{ConflictChoice, Files, Question};
use crate::platform::Platform;

/// How long the share offer stays.
const SHARE_SECONDS: f64 = 10.0;

/// A file exported to a document, which can be shared from here.
#[derive(Clone, Debug, PartialEq)]
pub struct Exported {
    pub path: PathBuf,
    pub name: String,
    pub mime: String,
}

#[derive(Default)]
pub struct Prompts {
    /// The offer to share, and when it appeared.
    share: Option<(Exported, f64)>,
}

impl Prompts {
    pub fn exported(&mut self, file: Exported, now: f64) {
        self.share = Some((file, now));
    }

    /// Draws what's pending, after the app's own interface.
    pub fn show(&mut self, ui: &mut egui::Ui, app: &mut WordApp, files: Option<&mut Files>, platform: Option<&dyn Platform>) {
        let ctx = ui.ctx().clone();
        if let Some(files) = files
            && let Some(Question::Conflict { entry, name }) = files.question().cloned()
        {
            let choice = Modal::new(Id::new("folio-conflict"))
                .show(&ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(RichText::new(format!("“{name}” changed")).heading());
                    ui.add_space(6.0);
                    ui.label("The document was changed outside Folio Writer since it was opened here. Replace it with your version, or save your version as a new document?");
                    ui.add_space(10.0);
                    let mut choice = None;
                    ui.horizontal(|ui| {
                        if ui.button("Replace").clicked() {
                            choice = Some(ConflictChoice::Replace);
                        }
                        if ui.button("Save a Copy…").clicked() {
                            choice = Some(ConflictChoice::SaveCopy);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(ConflictChoice::Keep);
                        }
                    });
                    choice
                })
                .inner;
            let choice = choice.or_else(|| ctx.input(|i| i.key_pressed(egui::Key::Escape)).then_some(ConflictChoice::Keep));
            if let Some(choice) = choice {
                files.resolve(entry, choice, app);
            }
        }
        self.show_share(&ctx, platform);
    }

    fn show_share(&mut self, ctx: &egui::Context, platform: Option<&dyn Platform>) {
        let Some((file, since)) = &self.share else { return };
        let now = ctx.input(|i| i.time);
        let left = SHARE_SECONDS - (now - since);
        if left <= 0.0 || platform.is_none() {
            self.share = None;
            return;
        }
        let mut close = false;
        egui::Area::new(Id::new("folio-share")).anchor(Align2::CENTER_BOTTOM, [0.0, -48.0]).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Saved {}", file.name));
                    if ui.button("Share…").clicked() {
                        if let Some(p) = platform {
                            p.call("share", json!({"path": file.path.to_string_lossy(), "mime": file.mime, "name": file.name}));
                        }
                        close = true;
                    }
                    if ui.button("Dismiss").clicked() {
                        close = true;
                    }
                });
            });
        });
        if close {
            self.share = None;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(left.max(0.05)));
        }
    }
}

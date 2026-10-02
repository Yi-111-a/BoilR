use eframe::egui;
use egui::ScrollArea;
use futures::executor::block_on;

use steam_shortcuts_util::shortcut::ShortcutOwned;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::get_renames_file;
use crate::platforms::ShortcutToImport;
use crate::renames::{apply_rename, seed_rename_edit};
#[cfg(target_family = "unix")]
use crate::steam::setup_proton_games;
use crate::sync;

use crate::sync::{download_images, SyncProgress};

use super::{all_ready, get_all_games};
use super::{
    ui_colors::{BACKGROUND_COLOR, EXTRA_BACKGROUND_COLOR},
    MyEguiApp,
};
use crate::backups::backup_shortcuts;

const SECTION_SPACING: f32 = 25.0;

pub enum FetchStatus<T> {
    NeedsFetched,
    Fetching,
    Fetched(T),
}

impl<T> FetchStatus<T> {
    pub fn is_some(&self) -> bool {
        match self {
            FetchStatus::NeedsFetched => false,
            FetchStatus::Fetching => false,
            FetchStatus::Fetched(_) => true,
        }
    }
}

/// One frame of the rename editor: draws the name field and the confirm button.
///
/// Split out of the Import tab so the field's binding can be driven in a test. `buffer` is the
/// caller's own `String` and is written in place — that is the whole point of the function, since
/// egui is immediate mode and `TextEdit` keeps no copy of the text, so a field bound to a value
/// rebuilt each frame shows a keystroke for one frame and then drops it. The button's `Response`
/// is returned so a caller can tell a click from a frame that only rendered.
fn rename_editor_frame(ui: &mut egui::Ui, buffer: &mut String) -> egui::Response {
    ui.text_edit_singleline(buffer).request_focus();
    ui.button("Rename")
}

impl MyEguiApp {
    pub(crate) fn render_import_games(&mut self, ui: &mut egui::Ui) {
        ui.heading("Import Games");

        let scroll_style = ui.style_mut();
        scroll_style.visuals.extreme_bg_color = BACKGROUND_COLOR;
        scroll_style.visuals.widgets.inactive.bg_fill = EXTRA_BACKGROUND_COLOR;
        scroll_style.visuals.widgets.active.bg_fill = EXTRA_BACKGROUND_COLOR;
        scroll_style.visuals.selection.bg_fill = EXTRA_BACKGROUND_COLOR;
        scroll_style.visuals.widgets.hovered.bg_fill = EXTRA_BACKGROUND_COLOR;

        ScrollArea::vertical()
        .stick_to_right(true)
        .auto_shrink([false,true])
        .show(ui,|ui| {
            ui.reset_style();
            ui.label("Select the games you want to import into steam");
            for (name,status) in &self.games_to_sync{
                ui.heading(name);
                match &*status.borrow(){
                    FetchStatus::NeedsFetched => {ui.label("Need to find games");},
                    FetchStatus::Fetching => {
                        ui.horizontal(|ui|{
                            ui.spinner();
                            ui.label("Finding installed games");
                        });
                    },
                    FetchStatus::Fetched(shortcuts) => {
                        match shortcuts{
                            Ok(shortcuts) => {
                                if shortcuts.is_empty(){
                                    ui.label("Did not find any games");
                                }
                                for shortcut_to_import in shortcuts {
                                    let shortcut = &shortcut_to_import.shortcut;
                                    let mut import_game = !self.settings.blacklisted_games.contains(&shortcut.app_id);
                                    ui.horizontal(|ui|{
                                        if self.current_edit == Option::Some(shortcut.app_id){
                                            // The edit buffer lives on the app state, not in the map.
                                            // egui is immediate mode: TextEdit writes each keystroke into
                                            // the String it is handed and keeps no copy of the text, so
                                            // the String has to outlive the frame for the keystroke to
                                            // outlive it. Holding it here rather than in rename_map
                                            // also means an editor that is walked away from leaves the
                                            // map untouched, so an unconfirmed name is never recorded.
                                            let clicked = rename_editor_frame(ui, &mut self.rename_edit).clicked();
                                            if clicked {
                                                let name = self.rename_edit.clone();
                                                self.current_edit = Option::None;
                                                // apply_rename is the only rule for what renames.json
                                                // holds: an empty field, or the launcher's own name,
                                                // removes the entry instead of storing it (#561).
                                                apply_rename(&mut self.rename_map, shortcut.app_id, &shortcut.app_name, &name);
                                                let rename_file_path = get_renames_file();
                                                let contents = serde_json::to_string(&self.rename_map);
                                                if let Ok(contents) = contents{
                                                    let res = std::fs::write(&rename_file_path, contents);
                                                    println!("Write rename file at {rename_file_path:?} with result: {res:?}");
                                                }
                                            }
                                        }  else {
                                            let name = self.rename_map.get(&shortcut.app_id).unwrap_or(&shortcut.app_name);
                                            let checkbox = egui::Checkbox::new(&mut import_game,name);
                                            let response = ui.add(checkbox);
                                            if response.double_clicked(){
                                                // Open on the recorded rename, or on the launcher's own
                                                // name. Nothing goes into rename_map here: a confirm
                                                // writes the whole map out, so a seed inserted now would
                                                // be recorded by the next confirm even if this editor is
                                                // abandoned (#561).
                                                self.rename_edit = seed_rename_edit(&self.rename_map, shortcut.app_id, &shortcut.app_name);
                                                self.current_edit = Option::Some(shortcut.app_id);
                                            }
                                            if response.clicked(){
                                                if !self.settings.blacklisted_games.contains(&shortcut.app_id){
                                                    self.settings.blacklisted_games.push(shortcut.app_id);
                                                } else {
                                                    self.settings.blacklisted_games.retain(|id| *id != shortcut.app_id);
                                                }
                                            }
                                        }
                                    });
                                }
                            },
                            Err(err) => {
                                ui.label("Failed finding games").on_hover_text(format!("Error message: {err}"));
                            },
                        };
                    },
                }

            };
            ui.add_space(SECTION_SPACING);

            ui.label("Check the settings if BoilR didn't find the game you where looking for");
        });
    }

    pub fn run_sync_blocking(&mut self) -> eyre::Result<()> {
        self.run_sync(true)
    }

    pub fn run_sync_async(&mut self) {
        let _ = self.run_sync(false);
    }
    fn run_sync(&mut self, wait: bool) -> eyre::Result<()> {
        let (sender, reciever) = watch::channel(SyncProgress::NotStarted);
        let settings = self.settings.clone();
        if settings.steam.stop_steam {
            crate::steam::ensure_steam_stopped(&settings.steam);
        }

        self.status_reciever = reciever;
        let renames = self.rename_map.clone();
        let all_ready = all_ready(&self.games_to_sync);
        let _ = sender.send(SyncProgress::Starting);
        if all_ready {
            let shortcuts_to_import = get_all_games(&self.games_to_sync);
            let handle: JoinHandle<eyre::Result<()>> = self.rt.spawn_blocking(move || {
                #[cfg(target_family = "unix")]
                setup_proton(shortcuts_to_import.iter());

                let import_games = to_shortcut_owned(shortcuts_to_import);

                let mut some_sender = Some(sender);
                backup_shortcuts(&settings.steam);
                let usersinfo =
                    sync::sync_shortcuts(&settings, &import_games, &mut some_sender, &renames)?;
                let task = download_images(&settings, &usersinfo, &mut some_sender);
                block_on(task);
                //Run a second time to fix up shortcuts after images are downloaded
                if let Err(e) = sync::fix_all_shortcut_icons(&settings) {
                    eprintln!("Could not fix shortcuts with error {e}");
                }

                if let Some(sender) = some_sender {
                    let _ = sender.send(SyncProgress::Done);
                }
                if settings.steam.start_steam {
                    crate::steam::ensure_steam_started(&settings.steam);
                }
                Ok(())
            });
            if wait {
                self.rt.block_on(handle)??;
            }
        }
        Ok(())
    }
}

fn to_shortcut_owned(
    shortcuts_to_import: Vec<(String, Vec<ShortcutToImport>)>,
) -> Vec<(String, Vec<ShortcutOwned>)> {
    let mut import_games = vec![];
    for (name, infos) in shortcuts_to_import {
        let mut shortcuts = vec![];
        for info in infos {
            shortcuts.push(info.shortcut);
        }
        import_games.push((name, shortcuts));
    }
    import_games
}

#[cfg(target_family = "unix")]
fn setup_proton<'a, I>(shortcut_infos: I)
where
    I: IntoIterator<Item = &'a (String, Vec<ShortcutToImport>)>,
{
    let mut shortcuts_to_proton = vec![];

    for (name, shortcuts) in shortcut_infos {
        for shortcut_info in shortcuts {
            if shortcut_info.needs_proton {
                crate::sync::symlinks::ensure_links_folder_created(name);
            }
            if shortcut_info.needs_proton {
                shortcuts_to_proton.push(format!("{}", shortcut_info.shortcut.app_id));
            }

            if shortcut_info.needs_symlinks {
                crate::sync::symlinks::create_sym_links(&shortcut_info.shortcut);
            }
        }
        if let Err(err) = setup_proton_games(&shortcuts_to_proton) {
            eprintln!("failed to save proton settings: {err:?}");
        }
    }
}

#[cfg(test)]
mod rename_field_tests {
    use super::*;

    /// One frame of the real rename editor, as the Import tab calls it. `per_frame_rebuild`
    /// picks the binding under test: the caller's own buffer, which is what the Import tab
    /// passes, or a value cloned and dropped inside the frame, which is the bug this guards.
    /// `typed` is delivered through `RawInput` rather than `Context::input_mut`, because the
    /// next `run` starts from whatever input it is handed and discards anything queued in
    /// between frames.
    fn editor_frame(
        ctx: &egui::Context,
        buffer: &mut String,
        per_frame_rebuild: bool,
        typed: Option<&str>,
    ) -> (bool, egui::Rect) {
        let mut input: egui::RawInput = Default::default();
        if let Some(typed) = typed {
            input.events.push(egui::Event::Text(typed.to_owned()));
        }
        let mut clicked = false;
        let mut button = egui::Rect::ZERO;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = if per_frame_rebuild {
                    let mut rebuilt = buffer.clone();
                    rename_editor_frame(ui, &mut rebuilt)
                } else {
                    rename_editor_frame(ui, buffer)
                };
                clicked = response.clicked();
                button = response.rect;
            });
        });
        (clicked, button)
    }

    /// egui is immediate mode: `TextEdit` writes each keystroke into the `String` it is handed and
    /// keeps no copy of the text itself. So the buffer has to be state that outlives the frame.
    /// Otherwise the field shows a keystroke for one frame and drops it on the next, which leaves
    /// the only confirmable text being the name the editor opened with.
    #[test]
    fn typed_text_survives_into_the_next_frame() {
        let mut buffer = "Hades".to_string();
        let ctx = egui::Context::default();

        editor_frame(&ctx, &mut buffer, false, None); // take focus
        editor_frame(&ctx, &mut buffer, false, Some(" II"));
        assert_eq!(buffer, "Hades II");

        // The next frame renders with no input at all; what was typed is still what the field holds.
        editor_frame(&ctx, &mut buffer, false, None);
        assert_eq!(
            buffer, "Hades II",
            "the field has to still hold what was typed on the following frame"
        );
    }

    /// The regression this guards: a buffer cloned and dropped inside the frame never keeps the
    /// keystroke, so confirming can only ever record the name the editor opened with.
    #[test]
    fn a_per_frame_rebuild_loses_the_keystroke() {
        let mut buffer = "Hades".to_string();
        let ctx = egui::Context::default();

        editor_frame(&ctx, &mut buffer, false, None);
        editor_frame(&ctx, &mut buffer, true, Some(" II"));
        editor_frame(&ctx, &mut buffer, false, None);

        assert_eq!(
            buffer, "Hades",
            "a buffer rebuilt every frame cannot hold what was typed"
        );
    }

    /// Clicking Rename returns the buffer as it stands after the keystrokes — the text that gets
    /// recorded, read out of the buffer rather than out of the field's widget state.
    #[test]
    fn clicking_rename_returns_the_buffer_contents() {
        let mut buffer = "Hades".to_string();
        let ctx = egui::Context::default();

        editor_frame(&ctx, &mut buffer, false, None); // take focus
        editor_frame(&ctx, &mut buffer, false, Some(" II"));
        let (clicked, rect) = editor_frame(&ctx, &mut buffer, false, None);
        assert!(!clicked, "nothing is confirmed without a click");

        let centre = rect.center();
        let mut input: egui::RawInput = Default::default();
        input.events.push(egui::Event::PointerMoved(centre));
        input.events.push(egui::Event::PointerButton {
            pos: centre,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
        input.events.push(egui::Event::PointerButton {
            pos: centre,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        });
        let mut clicked = false;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                clicked = rename_editor_frame(ui, &mut buffer).clicked();
            });
        });

        assert!(clicked, "the click has to land on the Rename button");
        assert_eq!(
            buffer, "Hades II",
            "and the text it records is what was typed"
        );
    }
}

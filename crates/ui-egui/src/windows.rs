//! Multiple native views of one session. State is parked, never the document or undo history.
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

use egui::{ViewportBuilder, ViewportCommand, ViewportId};
use serde_json::{Value, json};
use wordcraft_engine::EditingView;

use crate::*;

const MAX_WINDOWS: usize = 8;

#[derive(Default)]
pub(crate) struct Windows {
    parked: BTreeMap<u64, WindowState>,
    active: u64,
    next: u64,
    close: Vec<u64>,
    root_hidden: bool,
    pub(crate) native: bool,
}

// The fields below belong to the current window. Services, autosave, recovery, printing,
// the command registry and the Session itself remain owned once by WordApp.
macro_rules! window_fields {
    ($($(#[$attr:meta])* $field:ident: $ty:ty),* $(,)?) => {
        struct WindowState {
            engine: EditingView,
            document: u64,
            revision: u64,
            $($(#[$attr])* $field: $ty,)*
        }
        impl WindowState {
            fn new(app: &WordApp, id: u64) -> Self {
                let mut state = Self {
                    engine: app.session.new_view(id),
                    document: app.session.document_id(), revision: app.session.rev(),
                    $($(#[$attr])* $field: Default::default(),)*
                };
                state.ui = app.ui.clone();
                state.ui.backstage = false;
                state.canvas.want_focus = true;
                state
            }
            fn exchange(&mut self, app: &mut WordApp) {
                if self.document != app.session.document_id() {
                    self.dialog = None;
                    self.colors = None;
                    self.canvas = Default::default();
                    self.comment_drafts = Default::default();
                    self.file_dialog = None;
                } else if self.revision != app.session.rev() {
                    // Cached geometry refers to the previous revision; ordinary scrolling stays.
                    self.canvas.scroll_to_caret = false;
                }
                app.session.exchange_view(&mut self.engine);
                $($(#[$attr])* std::mem::swap(&mut self.$field, &mut app.$field);)*
                self.document = app.session.document_id();
                self.revision = app.session.rev();
            }
        }
    }
}

window_fields! {
    ui: UiState,
    canvas: canvas::CanvasState,
    controls: content_controls::ControlUi,
    dialog: Option<dialogs::Dialog>,
    colors: Option<colors::ColorsDialog>,
    restrict: dialogs_review::RestrictPane,
    status_msg: Option<(String, f64)>,
    change_picture_target: Option<String>,
    recipient_list_pending: bool,
    dialog_pick: Option<String>,
    integrated_titlebar: bool,
    mac_titlebar: chrome::MacTitleBar,
    synthetic: Vec<egui::Event>,
    control_click: bool,
    keytip_rects: Vec<(egui::Rect, String)>,
    nav_search_focus: bool,
    native_scale: f32,
    sent_title: String,
    sent_full_screen: bool,
    seen_full_screen: Option<bool>,
    escape_free: bool,
    equation_prev_tab: Option<String>,
    smart_art_item: Option<usize>,
    smart_art_typing: Option<usize>,
    outline_prev_tab: Option<String>,
    file_dialog: Option<file_dialogs::PendingDialog>,
    #[cfg(not(target_arch = "wasm32"))]
    file_browser: file_browser::State,
    info_editing: Option<(&'static str, u64)>,
    comment_drafts: panes::CommentDrafts,
    queued_shots: Vec<(u64, f64, u32)>,
}

// Immediate viewports bypass eframe::App::raw_input_hook. Run the same input preparation
// through egui's public plugin hook, before it computes pointer/key state for each child.
#[derive(Default)]
struct ViewInput {
    viewport: ViewportId,
    synthetic: Vec<egui::Event>,
    control_click: bool,
    focused: bool,
}
impl egui::Plugin for ViewInput {
    fn debug_name(&self) -> &'static str {
        "WordCraft document window input"
    }

    fn input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if raw.viewport_id != ViewportId::ROOT && raw.viewport_id == self.viewport {
            self.focused = prepare_view_input(raw, &mut self.synthetic, &mut self.control_click);
        }
    }
}

/// Restores the caller's context even if a callback unwinds. No document is ever swapped out.
struct Scope<'a> {
    app: &'a mut WordApp,
    previous: u64,
    target: u64,
}
impl Deref for Scope<'_> {
    type Target = WordApp;
    fn deref(&self) -> &WordApp {
        self.app
    }
}
impl DerefMut for Scope<'_> {
    fn deref_mut(&mut self) -> &mut WordApp {
        self.app
    }
}
impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if let Some(mut state) = self.app.windows.parked.remove(&self.previous) {
            state.exchange(self.app);
            self.app.windows.active = self.previous;
            self.app.windows.parked.insert(self.target, state);
        }
    }
}

pub fn viewport(id: u64) -> ViewportId {
    if id == 0 { ViewportId::ROOT } else { ViewportId::from_hash_of(("wordcraft-document-view", id)) }
}

impl WordApp {
    /// The desktop host opts in; headless and web callers receive an explicit unsupported error.
    pub fn enable_document_windows(&mut self) {
        self.windows.native = true;
    }

    pub(crate) fn window_id(&self) -> u64 {
        self.windows.active
    }

    pub fn window_ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = self.windows.parked.keys().copied().collect();
        ids.push(self.windows.active);
        ids.retain(|id| *id != 0 || !self.windows.root_hidden);
        ids.retain(|id| !self.windows.close.contains(id));
        ids.sort_unstable();
        ids
    }

    pub fn with_window<T>(&mut self, id: u64, f: impl FnOnce(&mut WordApp) -> T) -> Result<T, String> {
        if id == self.windows.active {
            return Ok(f(self));
        }
        let Some(mut state) = self.windows.parked.remove(&id) else { return Err(format!("no document window {id}")) };
        let previous = self.windows.active;
        state.exchange(self);
        self.windows.active = id;
        self.windows.parked.insert(previous, state);
        let mut scope = Scope { app: self, previous, target: id };
        Ok(f(&mut scope))
    }

    pub(crate) fn new_document_window(&mut self) -> Result<Value, String> {
        if !self.windows.native {
            return Err("New Window requires the native desktop application".into());
        }
        if self.windows.parked.len().saturating_add(1) >= MAX_WINDOWS {
            return Err("at most eight document windows can be open".into());
        }
        self.windows.next = self.windows.next.checked_add(1).ok_or("window identifiers exhausted")?;
        let id = self.windows.next;
        let state = WindowState::new(self, id);
        self.windows.parked.insert(id, state);
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
        Ok(json!({"window": id, "document": self.session.document_id()}))
    }

    pub(crate) fn another_window_has_dialog(&self) -> bool {
        self.windows.parked.values().any(|w| {
            let pending = w.dialog.is_some() || w.colors.is_some() || w.file_dialog.is_some();
            #[cfg(not(target_arch = "wasm32"))]
            let pending = pending || w.file_browser.open.is_some();
            pending
        })
    }

    pub(crate) fn flush_window_comments(&mut self) -> Result<(), String> {
        for id in self.window_ids() {
            self.with_window(id, panes::flush_comment_drafts)??;
        }
        Ok(())
    }

    pub(crate) fn repaint_document_windows(&self) {
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_of(ViewportId::ROOT);
            for id in self.window_ids() {
                ctx.request_repaint_of(viewport(id));
            }
        }
    }

    /// Close only this view while others exist. A refused comment draft stays in its window.
    pub fn close_document_window(&mut self, id: u64) -> Result<bool, String> {
        // A completed final-close prompt or an explicit application quit is already
        // authorized. Do not turn the host's resulting close event into another view close.
        if self.quit_requested {
            return self.with_window(id, |app| app.close_requested());
        }
        self.with_window(id, |app| {
            if app.dialog.is_some() || app.colors.is_some() || app.file_dialog_open() {
                return Err("Finish this window's dialog before closing it".to_string());
            }
            Ok(())
        })??;
        if self.window_ids().len() <= 1 {
            return self.with_window(id, |app| app.close_requested());
        }
        self.with_window(id, panes::flush_comment_drafts)??;
        if !self.windows.close.contains(&id) {
            self.windows.close.push(id);
        }
        self.repaint_document_windows();
        Ok(false) // the host must cancel native close; the manager removes/hides this view
    }

    pub fn root_window_hidden(&self) -> bool {
        self.windows.root_hidden
    }

    /// Render children from the root every pass. The root remains an invisible host after its
    /// document view is closed, until the final visible view's save/close flow allows exit.
    pub fn document_windows_ui(&mut self, ctx: &egui::Context) {
        ctx.add_plugin(ViewInput::default());
        for id in std::mem::take(&mut self.windows.close) {
            if id == 0 {
                self.windows.root_hidden = true;
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
            } else {
                self.windows.parked.remove(&id);
            }
        }
        let ids: Vec<_> = self.windows.parked.keys().copied().collect();
        for id in ids {
            let _ = self.with_window(id, |app| {
                ctx.with_plugin(|input: &mut ViewInput| {
                    input.viewport = viewport(id);
                    input.synthetic = std::mem::take(&mut app.synthetic);
                    input.control_click = app.control_click;
                    input.focused = false;
                });
            });
            ctx.show_viewport_immediate(
                viewport(id),
                ViewportBuilder::default().with_title("WordCraft").with_inner_size([1100.0, 800.0]),
                |ui, _| {
                    let ctx = ui.ctx().clone();
                    let _ = self.with_window(id, |app| {
                        if ctx.with_plugin(|input: &mut ViewInput| input.focused).unwrap_or(false) {
                            app.sync_synthetic_focus();
                        }
                        app.logic(&ctx);
                        ui.scope_builder(egui::UiBuilder::new().id_salt(("document-view", id)), |ui| app.ui(ui));
                    });
                    if ctx.input(|i| i.viewport().close_requested()) {
                        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                        match self.close_document_window(id) {
                            Ok(true) => self.quit_requested = true,
                            Err(e) => {
                                let _ = self.with_window(id, |app| app.status(e));
                            }
                            _ => {}
                        }
                    }
                },
            );
            let _ = self.with_window(id, |app| {
                ctx.with_plugin(|input: &mut ViewInput| {
                    app.synthetic = std::mem::take(&mut input.synthetic);
                    app.control_click = input.control_click;
                });
                if !app.synthetic.is_empty() {
                    ctx.request_repaint_of(viewport(id));
                }
            });
        }
        if self.quit_requested {
            // The desktop's existing final-exit handling (including macOS) owns termination.
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    pub fn window_raw_input(&mut self, raw: &mut egui::RawInput) {
        let id = self.windows.parked.keys().copied().find(|id| viewport(*id) == raw.viewport_id).unwrap_or(0);
        let _ = self.with_window(id, |app| app.raw_input_hook(raw));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_engine::{Session, doc::Document};

    #[test]
    fn windows_edit_one_document_and_close_without_discarding_it() {
        let mut app = WordApp::new(Session::new(Document::from_text("Hello")), Services::default());
        app.enable_document_windows();
        let id = app.execute("view.newWindow", json!({})).unwrap()["window"].as_u64().unwrap();
        let document = app.session.document_id();
        app.with_window(id, |a| {
            a.run("caret.docEnd", json!({})).unwrap();
            a.run("text.insert", json!({"text":" world"})).unwrap();
            a.session.view.zoom = 2.0;
        })
        .unwrap();
        assert_eq!(app.session.document_id(), document);
        assert_eq!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body).trim(), "Hello world");
        assert_eq!(app.session.view.zoom, 1.0);
        assert!(!app.close_document_window(0).unwrap());
        assert!(app.dialog.is_none(), "closing one view must not prompt to discard shared work");
        assert!(app.session.dirty);
    }

    #[test]
    fn context_restored_on_error_and_unwind() {
        let mut app = WordApp::new(Session::new(Document::new()), Services::default());
        app.enable_document_windows();
        app.new_document_window().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = app.with_window(1, |a| {
                a.session.view.zoom = 2.0;
                panic!("test unwind");
            });
        }));
        assert!(result.is_err());
        assert_eq!(app.window_id(), 0);
        assert_eq!(app.session.view.zoom, 1.0);
        assert_eq!(app.with_window(1, |a| a.session.view.zoom).unwrap(), 2.0);
    }

    #[test]
    fn headless_new_window_reports_unsupported_and_window_count_is_bounded() {
        let mut app = WordApp::new(Session::new(Document::new()), Services::default());
        assert!(app.execute("view.newWindow", json!({})).is_err());
        app.enable_document_windows();
        for _ in 1..MAX_WINDOWS {
            app.new_document_window().unwrap();
        }
        assert!(app.new_document_window().is_err());
        assert_eq!(app.window_ids().len(), MAX_WINDOWS);
    }

    #[test]
    fn a_modal_dialog_blocks_other_views_but_not_its_owner() {
        let mut app = WordApp::new(Session::new(Document::new()), Services::default());
        app.enable_document_windows();
        app.new_document_window().unwrap();
        app.dialog = Some(dialogs::Dialog::SaveChanges {
            name: "Document".into(),
            then: "file.close".into(),
            params: json!({}),
            document: app.session.document_id(),
            message: String::new(),
        });
        let result = app.with_window(1, |a| a.run("text.insert", json!({"text":"must not land"}))).unwrap();
        assert!(result.is_err());
        assert!(app.close_document_window(0).is_err(), "a hidden modal owner would trap the remaining view");
        assert!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body).trim().is_empty());
        app.dialog = None;
        app.with_window(1, |a| a.run("text.insert", json!({"text":"allowed"}))).unwrap().unwrap();
    }

    #[test]
    fn closing_both_views_in_one_frame_still_prompts_for_the_last_dirty_view() {
        let mut app = WordApp::new(Session::new(Document::new()), Services::default());
        app.enable_document_windows();
        app.new_document_window().unwrap();
        app.run("text.insert", json!({"text":"keep me"})).unwrap();
        assert!(!app.close_document_window(0).unwrap());
        assert_eq!(app.window_ids(), vec![1]);
        assert!(!app.close_document_window(1).unwrap());
        assert!(app.with_window(1, |a| matches!(a.dialog, Some(dialogs::Dialog::SaveChanges { .. }))).unwrap());
        assert!(app.session.dirty);
        assert!(!app.quit_requested);
    }

    #[test]
    fn save_from_another_view_has_one_path_and_contains_both_edits() {
        let mut app = WordApp::new(Session::new(Document::new()), Services::default());
        app.enable_document_windows();
        app.new_document_window().unwrap();
        app.run("text.insert", json!({"text":"one"})).unwrap();
        let path = std::env::temp_dir().join(format!("wordcraft-shared-views-{}.docx", std::process::id()));
        app.with_window(1, |a| {
            a.run("caret.docEnd", json!({})).unwrap();
            a.run("text.insert", json!({"text":" two"})).unwrap();
            a.execute("file.saveAs", json!({"path":path})).unwrap();
        })
        .unwrap();
        assert_eq!(app.session.path.as_deref(), Some(path.as_path()));
        assert!(!app.session.dirty);
        let mut reopened = Session::new(Document::new());
        reopened.run("file.open", &json!({"path":path})).unwrap();
        assert_eq!(reopened.doc.plain_text(wordcraft_doc::StoryRef::Body).trim(), "one two");
        std::fs::remove_file(path).unwrap();
    }
}

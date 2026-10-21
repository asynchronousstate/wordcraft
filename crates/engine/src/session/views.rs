//! Parked editing state for another view of the SAME session. No file, history or recovery owner.
use super::*;
use wordcraft_doc::props::ParaProps;

// Keep the list and exchange together: adding view-local state needs one entry here.
macro_rules! view_fields {
    ($($field:ident: $ty:ty),* $(,)?) => {
        #[allow(clippy::type_complexity)]
        pub struct EditingView {
            id: u64,
            document: u64,
            revision: u64,
            before: Document,
            $($field: $ty,)*
        }
        impl Session {
            /// Make another view; the document, undo/redo, path and recovery remain in `self`.
            pub fn new_view(&self, id: u64) -> EditingView {
                EditingView {
                    id, document: self.document_id, revision: self.rev, before: self.doc.clone(),
                    $($field: self.$field.clone(),)*
                }
            }
            /// Exchange only editing state. Call again to restore the previous view.
            pub fn exchange_view(&mut self, other: &mut EditingView) {
                other.refresh(&self.doc, self.document_id, self.rev);
                $(std::mem::swap(&mut self.$field, &mut other.$field);)*
                std::mem::swap(&mut self.active_view, &mut other.id);
                other.document = self.document_id;
                other.revision = self.rev;
                other.before = self.doc.clone();
                self.clamp_selection();
            }
        }
    }
}

view_fields! {
    sel: Selection,
    view: ViewState,
    pending: Option<CharProps>,
    find: FindState,
    goal_x: Option<f32>,
    page_hint: usize,
    painter: Option<(CharProps, ParaProps, bool)>,
    typing: Option<TypedWord>,
    math: Option<MathEdit>,
    math_latex: bool,
    math_normal_text: bool,
    also_selected: Vec<Pos>,
    column: Option<ColumnBlock>,
    column_mode: bool,
    widened: Option<Widened>,
    edit_marks: Vec<Pos>,
    go_back_step: usize,
    layout: Option<(u64, f32, f32, ViewMode, Option<u32>, Arc<DocLayout>, bool,
        Option<wordcraft_layout::outline::OutlineView>, (bool, wordcraft_layout::HideDeleted, bool),
        NumeralMode, Option<wordcraft_layout::TypingWord>)>,
}

impl EditingView {
    fn refresh(&mut self, doc: &Document, document: u64, revision: u64) {
        if self.document == document && self.revision == revision {
            return;
        }
        if self.document != document {
            self.sel = Selection::caret(doc.start_of(StoryRef::Body));
            self.pending = None;
            self.find = FindState::default();
            self.edit_marks.clear();
        } else {
            self.sel.anchor = map_position(&self.before, doc, &self.sel.anchor);
            self.sel.focus = map_position(&self.before, doc, &self.sel.focus);
            self.find.results.clear();
            self.find.highlights = None;
            self.find.current = 0;
        }
        // Multi-part selections and in-object editors refer to the old structure. Retain their
        // ordinary text selection, but require the user to enter those modes again after edits.
        self.column = None;
        self.column_mode = false;
        self.widened = None;
        self.also_selected.clear();
        self.math = None;
        self.typing = None;
        self.goal_x = None;
    }
}

/// Follow unchanged paragraphs by their shared block identity, including moves into tables.
/// For edited text, map the replaced range with right affinity. Whole-range replacements
/// collapse an interior caret to the end, like a deletion followed by insertion.
fn map_position(before: &Document, after: &Document, pos: &Pos) -> Pos {
    let story = pos.story;
    let old_block = before.container(story, &pos.path).and_then(|b| b.get(pos.path.last()));
    let paths = after.para_paths(story);
    if let Some(old) = old_block {
        for path in &paths {
            if after.container(story, path).and_then(|b| b.get(path.last())).is_some_and(|b| Arc::ptr_eq(old, b)) {
                return after.clamp(&Pos::new(story, path.clone(), pos.off));
            }
        }
    }
    // Bound temporary text. Large/hostile documents safely fall back to a valid nearby caret.
    const MAX_TEXT: usize = 4 * 1024 * 1024;
    fn flatten(doc: &Document, story: StoryRef, paths: &[Path], pos: Option<&Pos>) -> Option<(String, usize)> {
        let mut text = String::new();
        let mut offset = 0;
        for path in paths {
            let p = doc.para(story, path)?;
            if text.len().saturating_add(p.text.len()).saturating_add(1) > MAX_TEXT {
                return None;
            }
            if let Some(at) = pos.filter(|at| at.path == *path) {
                offset = text.len().saturating_add(at.off.min(p.text.len()));
            }
            text.push_str(&p.text);
            text.push('\n');
        }
        Some((text, offset))
    }
    let old_paths = before.para_paths(story);
    let Some((old, offset)) = flatten(before, story, &old_paths, Some(pos)) else { return after.clamp(pos) };
    let Some((new, _)) = flatten(after, story, &paths, None) else { return after.clamp(pos) };
    let prefix: usize = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum();
    let suffix: usize = old
        .get(prefix..)
        .unwrap_or("")
        .chars()
        .rev()
        .zip(new.get(prefix..).unwrap_or("").chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let old_end = old.len().saturating_sub(suffix);
    let new_end = new.len().saturating_sub(suffix);
    let mut at = if offset < prefix {
        offset
    } else if offset >= old_end {
        new_end.saturating_add(offset.saturating_sub(old_end))
    } else {
        new_end
    };
    for path in paths {
        let Some(p) = after.para(story, &path) else { continue };
        if at <= p.len() {
            return after.clamp(&Pos::new(story, path, at));
        }
        at = at.saturating_sub(p.len().saturating_add(1));
    }
    after.clamp(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn two_views_share_edits_and_undo_but_keep_selection_and_zoom() {
        let mut s = Session::new(Document::from_text("First\nSecond"));
        let mut other = s.new_view(1);
        s.exchange_view(&mut other);
        s.sel = Selection::caret(Pos::body(1, 3));
        s.view.zoom = 2.0;
        s.exchange_view(&mut other);
        s.run("text.insert", &json!({"text":"Hello "})).unwrap();
        s.exchange_view(&mut other);
        assert_eq!(s.sel.focus, Pos::body(1, 3));
        assert_eq!(s.view.zoom, 2.0);
        s.run("text.insert", &json!({"text":"!"})).unwrap();
        assert_eq!(s.undo_depth(), 2, "typing in another view is a new undo step");
        s.exchange_view(&mut other);
        assert_eq!(s.view.zoom, 1.0);
        assert!(s.doc.plain_text(StoryRef::Body).contains("Sec!ond"));
        s.run("edit.undo", &json!({})).unwrap();
        assert!(s.doc.plain_text(StoryRef::Body).contains("Second"));
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(s.doc.plain_text(StoryRef::Body).trim(), "First\nSecond");
    }

    #[test]
    fn render_switches_do_not_split_typing_and_failures_keep_shared_content() {
        let mut s = Session::new(Document::new());
        let mut other = s.new_view(1);
        s.run("text.insert", &json!({"text":"a"})).unwrap();
        s.exchange_view(&mut other);
        s.exchange_view(&mut other);
        s.run("text.insert", &json!({"text":"b"})).unwrap();
        assert_eq!(s.undo_depth(), 1);
        let doc = s.doc.clone();
        assert!(s.run("no.such.command", &json!({})).is_err());
        assert_eq!(s.doc, doc);
    }

    #[test]
    fn inactive_positions_follow_unicode_insert_delete_split_and_replacement() {
        let mut s = Session::new(Document::from_text("aé🙂z\nSecond"));
        s.sel = Selection::caret(Pos::body(0, 7));
        let mut other = s.new_view(1);
        s.sel = Selection::caret(Pos::body(0, 1));
        s.run("text.insert", &json!({"text":"XY"})).unwrap();
        s.exchange_view(&mut other);
        assert_eq!(s.sel.focus, Pos::body(0, 9));
        s.exchange_view(&mut other);
        s.doc.split_paragraph(&Pos::body(0, 3)).unwrap();
        s.touch();
        s.exchange_view(&mut other);
        assert_eq!(s.sel.focus, Pos::body(1, 6));
        s.exchange_view(&mut other);
        s.set_document(Document::from_text("Replacement"));
        s.exchange_view(&mut other);
        assert_eq!(s.sel.focus, Pos::body(0, 0));
    }
}

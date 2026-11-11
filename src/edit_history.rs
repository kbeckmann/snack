//! Bounded undo/redo snapshots, including the selection and cursor.
use iced::widget::text_editor::{self, Content};
use std::time::{Duration, Instant};

struct Snapshot {
    text: String,
    cursor: text_editor::Cursor,
}
impl Snapshot {
    fn capture(content: &Content) -> Self {
        Self {
            text: content.text(),
            cursor: content.cursor(),
        }
    }
    fn restore(self, content: &mut Content) {
        *content = Content::with_text(&self.text);
        content.move_to(self.cursor);
    }
}
#[derive(Default)]
pub struct EditHistory {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    group: Option<(u8, Instant)>,
}
impl EditHistory {
    pub fn break_group(&mut self) {
        self.group = None;
    }
    fn edit_kind(action: &text_editor::Action) -> Option<u8> {
        use text_editor::{Action, Edit};
        match action {
            Action::Edit(Edit::Insert(_)) => Some(1),
            Action::Edit(Edit::Backspace) => Some(2),
            Action::Edit(Edit::Delete) => Some(3),
            Action::Edit(_) => Some(0),
            _ => None,
        }
    }
    pub fn record(&mut self, content: &Content, action: &text_editor::Action) {
        if let Some(kind) = Self::edit_kind(action) {
            self.record_snapshot(Snapshot::capture(content), kind);
        } else {
            self.break_group();
        }
    }
    // Record an editor action only if it actually changed the draft. Empty
    // deletions, unindents and pastes must not clear redo or consume undo slots.
    pub fn perform(&mut self, content: &mut Content, action: text_editor::Action) {
        let Some(kind) = Self::edit_kind(&action) else {
            self.break_group();
            content.perform(action);
            return;
        };
        let before = Snapshot::capture(content);
        content.perform(action);
        if before.text != content.text() {
            self.record_snapshot(before, kind);
        } else {
            self.break_group();
        }
    }
    fn record_snapshot(&mut self, snapshot: Snapshot, kind: u8) {
        let now = Instant::now();
        let grouped = kind != 0
            && snapshot.cursor.selection.is_none()
            && self.group.is_some_and(|(k, t)| {
                k == kind && now.duration_since(t) < Duration::from_millis(750)
            });
        if !grouped {
            if self.undo.len() == 100 {
                self.undo.remove(0);
            }
            self.undo.push(snapshot);
        }
        self.redo.clear();
        self.group = if kind == 0 { None } else { Some((kind, now)) };
    }
    pub fn undo(&mut self, content: &mut Content) {
        self.break_group();
        if let Some(snapshot) = self.undo.pop() {
            self.redo.push(Snapshot::capture(content));
            snapshot.restore(content);
        }
    }
    pub fn redo(&mut self, content: &mut Content) {
        self.break_group();
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push(Snapshot::capture(content));
            snapshot.restore(content);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use text_editor::{Action, Edit, Motion};
    #[test]
    fn typing_groups_and_redo_restores_cursor() {
        let mut c = Content::new();
        let mut h = EditHistory::default();
        for ch in "hej".chars() {
            let a = Action::Edit(Edit::Insert(ch));
            h.record(&c, &a);
            c.perform(a);
        }
        let cursor = c.cursor();
        h.undo(&mut c);
        assert_eq!(c.text(), "");
        h.redo(&mut c);
        assert_eq!(c.text(), "hej");
        assert_eq!(c.cursor(), cursor);
        let a = Action::Move(Motion::Left);
        h.record(&c, &a);
        c.perform(a);
        let a = Action::Edit(Edit::Insert('!'));
        h.record(&c, &a);
        c.perform(a);
        h.undo(&mut c);
        assert_eq!(c.text(), "hej");
        assert_eq!(c.cursor().position.column, 2);
        let a = Action::Edit(Edit::Insert('?'));
        h.record(&c, &a);
        c.perform(a);
        h.redo(&mut c);
        assert_eq!(c.text(), "he?j");
    }
}

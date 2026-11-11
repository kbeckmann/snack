//! Platform-native editing keys for the message composer.
//!
//! Iced's text editor only knows a small, mostly platform-neutral key map:
//! modified Backspace/Delete always remove one character, Cmd-Up/Down do not
//! reach the ends of the text.
//! This module maps navigation, selection and deletion keys to each
//! platform's own conventions. Typing, IME, clipboard and Enter remain with
//! iced's defaults.
//!
//! Sources:
//! - macOS: AppKit's `StandardKeyBinding.dict` (the system-wide Cocoa text
//!   bindings) plus NSTextView's behavior of moving to the start/end of the
//!   text when moving up/down past the first/last line.
//! - Windows: the common edit-control conventions (RichEdit, WinUI, Office):
//!   Ctrl moves and deletes by word, Ctrl-Home/End reach the ends of the text,
//!   Ctrl-Up/Down move by paragraph, and Shift/Ctrl-Insert/Delete clipboard keys.
//! - Linux: GTK's text view bindings, which match Windows except that
//!   Ctrl-Down moves to the end of the paragraph.
use iced::keyboard::{self, Modifiers, key::Named};
use iced::widget::text_editor::{Binding, Motion};

use crate::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform { MacOs, Windows, Linux }

pub const CURRENT: Platform = if cfg!(target_os = "macos") { Platform::MacOs }
    else if cfg!(target_os = "windows") { Platform::Windows } else { Platform::Linux };

/// Edits that need the composer's text, so the update handler performs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditCommand {
    /// Delete the selection, or else from the caret to a boundary.
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToLineStart,
    /// Cocoa up/down: past the first/last line, go to the start/end.
    Vertical { down: bool, select: bool },
    /// Move by paragraph. Backward goes to the paragraph start (the previous
    /// one when already there). Forward goes to the next paragraph's start
    /// when `next_start`, otherwise to the paragraph end.
    ParagraphStep { forward: bool, select: bool, next_start: bool },
}

fn edit(command: EditCommand) -> Option<Binding<Message>> {
    Some(Binding::Custom(Message::ComposerEdit(command)))
}

fn go(motion: Motion, select: bool) -> Option<Binding<Message>> {
    Some(if select { Binding::Select(motion) } else { Binding::Move(motion) })
}

/// The native behavior that consumes a key without changing the text or caret,
/// such as scrolling-only keys on macOS.
fn nothing() -> Option<Binding<Message>> {
    Some(Binding::Sequence(Vec::new()))
}

/// The platform binding for a key, or `None` to use iced's default handling.
pub fn binding(platform: Platform, key: &keyboard::Key, modifiers: Modifiers) -> Option<Binding<Message>> {
    match platform {
        Platform::MacOs => macos(key, modifiers),
        Platform::Windows | Platform::Linux => pc(platform, key, modifiers),
    }
}

fn macos(key: &keyboard::Key, m: Modifiers) -> Option<Binding<Message>> {
    let (shift, ctrl, alt, cmd) = (m.shift(), m.control(), m.alt(), m.logo());
    if let keyboard::Key::Named(named) = key {
        return match named {
            Named::ArrowLeft | Named::ArrowRight => {
                let right = *named == Named::ArrowRight;
                match (ctrl || cmd, alt) {
                    (true, false) => go(if right { Motion::End } else { Motion::Home }, shift),
                    (false, true) => go(if right { Motion::WordRight } else { Motion::WordLeft }, shift),
                    (false, false) => go(if right { Motion::Right } else { Motion::Left }, shift),
                    (true, true) => nothing(),
                }
            }
            Named::ArrowUp | Named::ArrowDown => {
                let down = *named == Named::ArrowDown;
                match (ctrl, cmd, alt) {
                    (false, false, false) => edit(EditCommand::Vertical { down, select: shift }),
                    (false, true, false) => go(if down { Motion::DocumentEnd } else { Motion::DocumentStart }, shift),
                    (false, false, true) => edit(EditCommand::ParagraphStep { forward: down, select: shift, next_start: false }),
                    // Ctrl-Up/Down only scroll; Mission Control usually owns them.
                    _ => nothing(),
                }
            }
            // Home/End and Page Up/Down scroll without moving the caret.
            Named::Home | Named::End if !ctrl && !cmd && !alt => {
                if shift { go(if *named == Named::End { Motion::DocumentEnd } else { Motion::DocumentStart }, true) }
                else { nothing() }
            }
            Named::PageUp | Named::PageDown if !ctrl && !cmd => {
                let motion = if *named == Named::PageDown { Motion::PageDown } else { Motion::PageUp };
                if shift { go(motion, true) } else if alt { go(motion, false) } else { nothing() }
            }
            Named::Backspace => match (ctrl, alt, cmd) {
                (_, true, false) => edit(EditCommand::DeleteWordBackward),
                (false, false, true) => edit(EditCommand::DeleteToLineStart),
                (_, false, false) => Some(Binding::Backspace),
                _ => nothing(),
            },
            Named::Delete => match (ctrl, alt, cmd) {
                (false, true, false) => edit(EditCommand::DeleteWordForward),
                (false, false, false) => Some(Binding::Delete),
                _ => nothing(),
            },
            _ => None,
        };
    }
    None
}

fn pc(platform: Platform, key: &keyboard::Key, m: Modifiers) -> Option<Binding<Message>> {
    let (shift, ctrl, alt) = (m.shift(), m.control(), m.alt());
    let keyboard::Key::Named(named) = key else {
        // Ctrl-Y redoes on Windows and in GTK; Ctrl-Shift-Z is handled with undo.
        if ctrl && !alt && !shift && matches!(key, keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case("y")) {
            return Some(Binding::Custom(Message::RedoInput));
        }
        return None;
    };
    if alt { return None; }
    match named {
        Named::ArrowLeft | Named::ArrowRight => {
            let right = *named == Named::ArrowRight;
            go(match (right, ctrl) {
                (true, true) => Motion::WordRight, (false, true) => Motion::WordLeft,
                (true, false) => Motion::Right, (false, false) => Motion::Left,
            }, shift)
        }
        Named::ArrowUp | Named::ArrowDown => {
            let down = *named == Named::ArrowDown;
            if ctrl {
                edit(EditCommand::ParagraphStep { forward: down, select: shift, next_start: platform == Platform::Windows })
            } else { go(if down { Motion::Down } else { Motion::Up }, shift) }
        }
        Named::Home | Named::End => {
            let end = *named == Named::End;
            go(match (end, ctrl) {
                (true, true) => Motion::DocumentEnd, (false, true) => Motion::DocumentStart,
                (true, false) => Motion::End, (false, false) => Motion::Home,
            }, shift)
        }
        Named::PageUp | Named::PageDown if !ctrl => go(if *named == Named::PageDown { Motion::PageDown } else { Motion::PageUp }, shift),
        Named::Backspace => if ctrl { edit(EditCommand::DeleteWordBackward) } else { Some(Binding::Backspace) },
        Named::Delete => match (ctrl, shift) {
            (true, false) => edit(EditCommand::DeleteWordForward),
            (false, true) => Some(Binding::Cut),
            (false, false) => Some(Binding::Delete),
            (true, true) => nothing(),
        },
        Named::Insert => match (ctrl, shift) {
            (true, false) => Some(Binding::Copy),
            (false, true) => Some(Binding::Paste),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Key;

    const NONE: Modifiers = Modifiers::empty();
    fn named(key: Named) -> Key { Key::Named(key) }
    fn letter(c: char) -> Key { Key::Character(c.to_string().into()) }
    fn mac(key: Key, m: Modifiers) -> Option<Binding<Message>> { binding(Platform::MacOs, &key, m) }
    fn pc(platform: Platform, key: Key, m: Modifiers) -> Option<Binding<Message>> { binding(platform, &key, m) }
    fn command(b: Option<Binding<Message>>) -> Option<EditCommand> {
        match b { Some(Binding::Custom(Message::ComposerEdit(c))) => Some(c), _ => None }
    }
    fn motion(b: Option<Binding<Message>>) -> Option<(Motion, bool)> {
        match b { Some(Binding::Move(m)) => Some((m, false)), Some(Binding::Select(m)) => Some((m, true)), _ => None }
    }
    fn swallowed(b: Option<Binding<Message>>) -> bool { matches!(b, Some(Binding::Sequence(s)) if s.is_empty()) }

    #[test]
    fn macos_follows_the_cocoa_standard_key_bindings() {
        let (alt, cmd, ctrl, shift) = (Modifiers::ALT, Modifiers::LOGO, Modifiers::CTRL, Modifiers::SHIFT);
        assert_eq!(command(mac(named(Named::Backspace), alt)), Some(EditCommand::DeleteWordBackward));
        assert_eq!(command(mac(named(Named::Backspace), alt | ctrl)), Some(EditCommand::DeleteWordBackward));
        assert_eq!(command(mac(named(Named::Backspace), cmd)), Some(EditCommand::DeleteToLineStart));
        assert!(matches!(mac(named(Named::Backspace), NONE), Some(Binding::Backspace)));
        assert!(matches!(mac(named(Named::Backspace), shift), Some(Binding::Backspace)));
        assert_eq!(command(mac(named(Named::Delete), alt)), Some(EditCommand::DeleteWordForward));
        assert_eq!(motion(mac(named(Named::ArrowLeft), alt)), Some((Motion::WordLeft, false)));
        assert_eq!(motion(mac(named(Named::ArrowRight), alt | shift)), Some((Motion::WordRight, true)));
        assert_eq!(motion(mac(named(Named::ArrowLeft), cmd)), Some((Motion::Home, false)));
        assert_eq!(motion(mac(named(Named::ArrowRight), cmd | shift)), Some((Motion::End, true)));
        assert_eq!(motion(mac(named(Named::ArrowLeft), ctrl)), Some((Motion::Home, false)));
        assert_eq!(motion(mac(named(Named::ArrowUp), cmd)), Some((Motion::DocumentStart, false)));
        assert_eq!(motion(mac(named(Named::ArrowDown), cmd | shift)), Some((Motion::DocumentEnd, true)));
        assert_eq!(command(mac(named(Named::ArrowUp), NONE)), Some(EditCommand::Vertical { down: false, select: false }));
        assert_eq!(command(mac(named(Named::ArrowDown), shift)), Some(EditCommand::Vertical { down: true, select: true }));
        assert_eq!(command(mac(named(Named::ArrowUp), alt | shift)),
            Some(EditCommand::ParagraphStep { forward: false, select: true, next_start: false }));
        assert!(swallowed(mac(named(Named::ArrowUp), ctrl)));
        // Home/End and Page Up/Down scroll without moving the caret.
        assert!(swallowed(mac(named(Named::Home), NONE)));
        assert!(swallowed(mac(named(Named::PageDown), NONE)));
        assert_eq!(motion(mac(named(Named::Home), shift)), Some((Motion::DocumentStart, true)));
        assert_eq!(motion(mac(named(Named::End), shift)), Some((Motion::DocumentEnd, true)));
        assert_eq!(motion(mac(named(Named::PageUp), shift)), Some((Motion::PageUp, true)));
        assert_eq!(motion(mac(named(Named::PageDown), alt)), Some((Motion::PageDown, false)));
    }

    #[test]
    fn windows_and_linux_follow_pc_text_field_conventions() {
        let (ctrl, shift) = (Modifiers::CTRL, Modifiers::SHIFT);
        for platform in [Platform::Windows, Platform::Linux] {
            assert_eq!(command(pc(platform, named(Named::Backspace), ctrl)), Some(EditCommand::DeleteWordBackward));
            assert_eq!(command(pc(platform, named(Named::Delete), ctrl)), Some(EditCommand::DeleteWordForward));
            assert!(matches!(pc(platform, named(Named::Backspace), shift), Some(Binding::Backspace)));
            assert_eq!(motion(pc(platform, named(Named::ArrowLeft), ctrl)), Some((Motion::WordLeft, false)));
            assert_eq!(motion(pc(platform, named(Named::ArrowRight), ctrl | shift)), Some((Motion::WordRight, true)));
            assert_eq!(motion(pc(platform, named(Named::Home), NONE)), Some((Motion::Home, false)));
            assert_eq!(motion(pc(platform, named(Named::End), shift)), Some((Motion::End, true)));
            assert_eq!(motion(pc(platform, named(Named::Home), ctrl)), Some((Motion::DocumentStart, false)));
            assert_eq!(motion(pc(platform, named(Named::End), ctrl | shift)), Some((Motion::DocumentEnd, true)));
            assert_eq!(motion(pc(platform, named(Named::ArrowUp), shift)), Some((Motion::Up, true)));
            assert_eq!(motion(pc(platform, named(Named::PageDown), NONE)), Some((Motion::PageDown, false)));
            assert!(matches!(pc(platform, named(Named::Delete), shift), Some(Binding::Cut)));
            assert!(matches!(pc(platform, named(Named::Insert), ctrl), Some(Binding::Copy)));
            assert!(matches!(pc(platform, named(Named::Insert), shift), Some(Binding::Paste)));
            assert!(matches!(pc(platform, letter('y'), ctrl), Some(Binding::Custom(Message::RedoInput))));
            // macOS conventions do not apply.
            assert!(pc(platform, named(Named::Backspace), Modifiers::ALT).is_none());
        }
        assert_eq!(command(pc(Platform::Windows, named(Named::ArrowDown), ctrl)),
            Some(EditCommand::ParagraphStep { forward: true, select: false, next_start: true }));
        assert_eq!(command(pc(Platform::Linux, named(Named::ArrowDown), ctrl | shift)),
            Some(EditCommand::ParagraphStep { forward: true, select: true, next_start: false }));
    }
}

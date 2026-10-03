//! Phone input as the window events the desktop backends already send.
//!
//! Kotlin classifies what the IME, a keyboard and the touchscreen did into
//! an [`Input`]; [`window_events`] turns it into the `WindowEvent`s that
//! reach `TermWindow`'s existing paths: composing text is
//! `AdviseDeadKeyStatus` and stays local, committed text and key presses
//! are `KeyEvent`s, a paste is the clipboard text Kotlin read, delivered
//! as `DroppedString` (`send_paste` on the active pane, in order with the
//! keys around it), and touches are mouse events.  Nothing here reaches a
//! pane directly.
//!
//! Committed text is one key event per character rather than one composed
//! write.  A mux server writes key presses, pastes and mouse reports to the
//! pane through its terminal's writer thread but a composed write straight
//! to the pty, so text written right after a key press (an IME deleting,
//! then committing) can reach the program first.

#![forbid(unsafe_code)]

use std::num::NonZeroU32;
use window::{
    DeadKeyStatus, KeyCode, KeyEvent, KeyboardLedStatus, Modifiers, MouseButtons, MouseEvent,
    MouseEventKind, MousePress, Point, ScreenPoint, WindowEvent,
};

/// `android.view.KeyEvent.META_*_ON` bits.
const META_SHIFT_ON: i32 = 0x1;
const META_ALT_ON: i32 = 0x2;
const META_CTRL_ON: i32 = 0x1000;
const META_META_ON: i32 = 0x10000;

/// One act of the user, as Kotlin observed it.  `Debug` shows no text,
/// key or character: input is never logged.
#[derive(Clone, PartialEq)]
pub enum Input {
    /// The IME's composing text changed; empty when composition ended.
    Preedit(String),
    /// The IME's committed text changed: erase characters the laptop holds
    /// before its cursor, then type `text`.  `meta` carries the modifiers
    /// armed on the on-screen key row; they apply to a single typed
    /// character.
    Commit {
        /// Characters to erase first, if any.
        erase: Option<Erase>,
        /// The text to type.
        text: String,
        /// `KeyEvent` meta state.
        meta: i32,
    },
    /// A key press from a keyboard or the on-screen key row.
    Key {
        /// `KeyEvent.getKeyCode()`.
        code: i32,
        /// The character the key produces without Ctrl, Alt and Meta; 0
        /// when it produces none.
        unicode: u32,
        /// `KeyEvent` meta state.
        meta: i32,
    },
    /// Paste this clipboard text into the active pane.
    Paste(String),
    /// A touch gesture at surface pixel `x`, `y`.
    Touch {
        /// Surface pixel column.
        x: f32,
        /// Surface pixel row.
        y: f32,
        /// What the finger did.
        gesture: Gesture,
    },
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preedit(_) => f.write_str("Preedit(<redacted>)"),
            Self::Commit { .. } => f.write_str("Commit(<redacted>)"),
            Self::Key { .. } => f.write_str("Key(<redacted>)"),
            Self::Paste(_) => f.write_str("Paste(<redacted>)"),
            Self::Touch { gesture, .. } => write!(f, "Touch({gesture:?})"),
        }
    }
}

/// The pane the bound window's keyboard input reaches, as the GUI thread
/// published it: its local id and the generation of that assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputTarget {
    /// Local pane id; `None` while the bound window shows no pane.
    pub pane: Option<usize>,
    /// How many times the target changed before it became this one.
    pub generation: u64,
}

/// Characters an IME edit erases with Backspace, in the pane it typed them
/// into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Erase {
    /// How many.
    pub characters: NonZeroU32,
    /// The input target that received them.
    pub target: InputTarget,
}

impl Erase {
    /// `characters` typed into `target`; `None` for none.
    pub fn new(characters: u32, target: InputTarget) -> Option<Self> {
        Some(Self {
            characters: NonZeroU32::new(characters)?,
            target,
        })
    }
}

/// What a finger did on the terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// A short touch.
    Tap,
    /// A drag that scrolls.  `from` and `to` are the vertical finger
    /// offsets in pixels from where the drag started, before and after this
    /// step; downward is positive.
    Scroll {
        /// Offset before this step.
        from: f32,
        /// Offset after this step.
        to: f32,
    },
    /// A long press: selection starts here.
    SelectStart,
    /// The finger moved while selecting.
    SelectMove,
    /// The finger lifted while selecting.
    SelectEnd,
}

impl Gesture {
    /// The code Kotlin passes; see `TerminalView`.
    pub fn from_code(code: i32, from: f32, to: f32) -> Option<Self> {
        Some(match code {
            0 => Self::Tap,
            1 => Self::Scroll { from, to },
            2 => Self::SelectStart,
            3 => Self::SelectMove,
            4 => Self::SelectEnd,
            _ => return None,
        })
    }
}

/// The window events for `input`.  `cell_height` is the bound window's
/// cell height in pixels, once it has painted; a scroll waits for it.
pub fn window_events(input: Input, cell_height: Option<f32>) -> Vec<WindowEvent> {
    match input {
        Input::Preedit(text) => vec![WindowEvent::AdviseDeadKeyStatus(if text.is_empty() {
            DeadKeyStatus::None
        } else {
            DeadKeyStatus::Composing(text)
        })],
        Input::Commit { erase, text, meta } => {
            // As the X11 and Wayland IME commits: the text, then
            // composition ended.
            let backspace = key_event(KeyCode::Char('\u{8}'), Modifiers::NONE);
            let erase = erase.map_or(0, |erase| erase.characters.get() as usize);
            let mut events: Vec<_> = std::iter::repeat_n(backspace, erase)
                .chain(commit(&text, modifiers(meta)))
                .map(WindowEvent::KeyEvent)
                .collect();
            events.push(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
            events
        }
        Input::Key {
            code,
            unicode,
            meta,
        } => pressed_key(code, unicode, meta)
            .map(WindowEvent::KeyEvent)
            .into_iter()
            .collect(),
        Input::Paste(text) => vec![WindowEvent::DroppedString(text)],
        Input::Touch { x, y, gesture } => touch(x, y, gesture, cell_height),
    }
}

fn modifiers(meta: i32) -> Modifiers {
    [
        (META_SHIFT_ON, Modifiers::SHIFT),
        (META_ALT_ON, Modifiers::ALT),
        (META_CTRL_ON, Modifiers::CTRL),
        (META_META_ON, Modifiers::SUPER),
    ]
    .into_iter()
    .filter(|(bit, _)| meta & bit != 0)
    .fold(Modifiers::NONE, |all, (_, modifier)| all | modifier)
}

fn key_event(key: KeyCode, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        key,
        modifiers,
        leds: KeyboardLedStatus::empty(),
        repeat_count: 1,
        key_is_down: true,
        raw: None,
    }
    .normalize_shift()
}

/// One key event per character.  Armed modifiers apply to a single
/// committed character only.  IMEs commit a line break as `\n`; a
/// terminal's Enter is `\r`.
fn commit(text: &str, modifiers: Modifiers) -> Vec<KeyEvent> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    let modifiers = if text.chars().nth(1).is_none() {
        modifiers
    } else {
        Modifiers::NONE
    };
    text.chars()
        .map(|c| key_event(KeyCode::Char(c), modifiers))
        .collect()
}

/// The key event an [`Input::Key`] is; `None` for a key that produces
/// nothing.
pub fn pressed_key(code: i32, unicode: u32, meta: i32) -> Option<KeyEvent> {
    key(code, unicode, modifiers(meta))
}

fn key(code: i32, unicode: u32, modifiers: Modifiers) -> Option<KeyEvent> {
    let key = match code {
        19 => KeyCode::UpArrow,
        20 => KeyCode::DownArrow,
        21 => KeyCode::LeftArrow,
        22 => KeyCode::RightArrow,
        61 => KeyCode::Char('\t'),
        66 | 160 => KeyCode::Char('\r'),
        67 => KeyCode::Char('\u{8}'),
        92 => KeyCode::PageUp,
        93 => KeyCode::PageDown,
        111 => KeyCode::Char('\u{1b}'),
        112 => KeyCode::Char('\u{7f}'),
        122 => KeyCode::Home,
        123 => KeyCode::End,
        124 => KeyCode::Insert,
        279 => KeyCode::Paste,
        131..=142 => KeyCode::Function((code - 130) as u8),
        // A dead key sets bit 31 and is no character.
        _ => KeyCode::Char(char::from_u32(unicode).filter(|c| *c != '\0')?),
    };
    Some(key_event(key, modifiers))
}

fn touch(x: f32, y: f32, gesture: Gesture, cell_height: Option<f32>) -> Vec<WindowEvent> {
    let at = |kind, mouse_buttons| {
        let coords = Point::new(x.round() as isize, y.round() as isize);
        WindowEvent::MouseEvent(MouseEvent {
            kind,
            coords,
            screen_coords: ScreenPoint::new(coords.x, coords.y),
            mouse_buttons,
            modifiers: Modifiers::NONE,
        })
    };
    let left = MousePress::Left;
    match gesture {
        Gesture::Tap => vec![
            at(MouseEventKind::Move, MouseButtons::NONE),
            at(MouseEventKind::Press(left), MouseButtons::LEFT),
            at(MouseEventKind::Release(left), MouseButtons::NONE),
        ],
        Gesture::SelectStart => vec![
            at(MouseEventKind::Move, MouseButtons::NONE),
            at(MouseEventKind::Press(left), MouseButtons::LEFT),
        ],
        Gesture::SelectMove => vec![at(MouseEventKind::Move, MouseButtons::LEFT)],
        Gesture::SelectEnd => vec![at(MouseEventKind::Release(left), MouseButtons::NONE)],
        Gesture::Scroll { from, to } => {
            let Some(height) = cell_height.filter(|h| *h > 0.0) else {
                return Vec::new();
            };
            // A finger moving down reveals older lines, as a wheel turned up.
            let lines = (to / height).floor() - (from / height).floor();
            if lines == 0.0 {
                return Vec::new();
            }
            let lines = lines.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            vec![at(MouseEventKind::VertWheel(lines), MouseButtons::NONE)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::input::{KeyCode as TermKey, KeyCodeEncodeModes, KeyboardEncoding};

    /// The bytes a pane receives for a key event: `TermWindow`'s key
    /// translation for the keys used here, then termwiz's xterm encoder,
    /// which the mux server runs for `SendKeyDown`.
    fn bytes(event: &KeyEvent) -> String {
        let key = match &event.key {
            KeyCode::Char('\r') => TermKey::Enter,
            KeyCode::Char('\t') => TermKey::Tab,
            KeyCode::Char('\u{8}') => TermKey::Backspace,
            KeyCode::Char('\u{7f}') => TermKey::Delete,
            KeyCode::Char('\u{1b}') => TermKey::Escape,
            KeyCode::Char(c) => TermKey::Char(*c),
            KeyCode::UpArrow => TermKey::UpArrow,
            KeyCode::DownArrow => TermKey::DownArrow,
            KeyCode::LeftArrow => TermKey::LeftArrow,
            KeyCode::RightArrow => TermKey::RightArrow,
            KeyCode::Home => TermKey::Home,
            KeyCode::Function(n) => TermKey::Function(*n),
            other => panic!("no translation for {other:?} in this test"),
        };
        let modes = KeyCodeEncodeModes {
            encoding: KeyboardEncoding::Xterm,
            application_cursor_keys: false,
            newline_mode: false,
            modify_other_keys: None,
        };
        key.encode(event.modifiers, modes, true).unwrap()
    }

    fn key_events(events: Vec<WindowEvent>) -> Vec<KeyEvent> {
        events
            .into_iter()
            .filter_map(|event| match event {
                WindowEvent::KeyEvent(key) => Some(key),
                _ => None,
            })
            .collect()
    }

    fn sent(input: Input) -> String {
        key_events(window_events(input, None))
            .iter()
            .map(bytes)
            .collect()
    }

    fn press(code: i32, unicode: char, meta: i32) -> Input {
        Input::Key {
            code,
            unicode: unicode as u32,
            meta,
        }
    }

    fn commit(text: &str, meta: i32) -> Input {
        Input::Commit {
            erase: None,
            text: text.into(),
            meta,
        }
    }

    const TARGET: InputTarget = InputTarget {
        pane: Some(3),
        generation: 2,
    };

    fn rewrite(erase: u32, text: &str) -> Input {
        Input::Commit {
            erase: Some(Erase::new(erase, TARGET).unwrap()),
            text: text.into(),
            meta: 0,
        }
    }

    fn status(events: &[WindowEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None) => Some("none".into()),
                WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::Composing(s)) => {
                    Some(format!("composing:{s}"))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn composing_text_is_presentation_only_and_a_commit_is_one_key_per_character() {
        let composing = window_events(Input::Preedit("ni".into()), None);
        assert_eq!(status(&composing), ["composing:ni"]);
        assert!(key_events(composing).is_empty(), "preedit sends nothing");
        assert_eq!(
            status(&window_events(Input::Preedit(String::new()), None)),
            ["none"]
        );

        let committed = window_events(commit("日本", 0), None);
        assert_eq!(status(&committed), ["none"]);
        let keys: Vec<_> = key_events(committed)
            .into_iter()
            .map(|key| key.key)
            .collect();
        assert_eq!(keys, [KeyCode::Char('日'), KeyCode::Char('本')]);
        assert_eq!(
            sent(commit("日本", 0)).as_bytes(),
            b"\xe6\x97\xa5\xe6\x9c\xac"
        );
    }

    #[test]
    fn committed_unicode_reaches_the_pane_as_its_literal_utf8_bytes() {
        assert_eq!(sent(commit("a", 0)).as_bytes(), b"a");
        assert_eq!(sent(commit("é", 0)).as_bytes(), b"\xc3\xa9");
        assert_eq!(sent(commit("e\u{301}", 0)).as_bytes(), b"e\xcc\x81");
        assert_eq!(sent(commit("😀", 0)).as_bytes(), b"\xf0\x9f\x98\x80");
        assert_eq!(
            sent(commit("x😀e\u{301}", 0)).as_bytes(),
            b"x\xf0\x9f\x98\x80e\xcc\x81"
        );
        assert_eq!(sent(commit("", 0)), "", "an empty commit sends nothing");
    }

    #[test]
    fn a_committed_line_break_is_enter() {
        assert_eq!(sent(commit("\n", 0)), "\r");
        assert_eq!(sent(commit("ls\n", 0)), "ls\r");
        assert_eq!(sent(commit("a\r\nb", 0)), "a\rb");
    }

    #[test]
    fn armed_modifiers_apply_to_a_single_committed_character_only() {
        assert_eq!(sent(commit("c", META_CTRL_ON)), "\x03");
        assert_eq!(sent(commit("x", META_ALT_ON)), "\x1bx");
        assert_eq!(sent(commit("ok", META_CTRL_ON)), "ok");
    }

    #[test]
    fn keys_encode_as_the_terminal_expects() {
        assert_eq!(sent(press(31, 'c', META_CTRL_ON)), "\x03");
        assert_eq!(sent(press(52, 'x', META_ALT_ON)), "\x1bx");
        assert_eq!(sent(press(29, 'A', META_SHIFT_ON)), "A");
        assert_eq!(sent(press(111, '\0', 0)), "\x1b");
        assert_eq!(sent(press(19, '\0', 0)), "\x1b[A");
        assert_eq!(sent(press(20, '\0', 0)), "\x1b[B");
        assert_eq!(sent(press(21, '\0', 0)), "\x1b[D");
        assert_eq!(sent(press(22, '\0', 0)), "\x1b[C");
        assert_eq!(sent(press(19, '\0', META_SHIFT_ON)), "\x1b[1;2A");
        assert_eq!(sent(press(67, '\0', 0)), "\x7f");
        assert_eq!(sent(press(112, '\0', 0)), "\x1b[3~");
        assert_eq!(sent(press(66, '\n', 0)), "\r");
        assert_eq!(sent(press(160, '\n', 0)), "\r");
        assert_eq!(sent(press(61, '\t', 0)), "\t");
        assert_eq!(sent(press(122, '\0', 0)), "\x1b[H");
        assert_eq!(sent(press(131, '\0', 0)), "\x1bOP");
        assert_eq!(sent(press(62, ' ', META_CTRL_ON)), "\0");
        assert_eq!(
            pressed_key(279, 0, 0).map(|key| key.key),
            Some(KeyCode::Paste),
            "the Paste key is the key the default key table pastes with"
        );
    }

    #[test]
    fn keys_that_produce_nothing_send_nothing() {
        assert!(window_events(press(59, '\0', META_SHIFT_ON), None).is_empty());
        let dead_acute = Input::Key {
            code: 33,
            unicode: 0x8000_0000 | 0xb4,
            meta: 0,
        };
        assert!(window_events(dead_acute, None).is_empty());
    }

    #[test]
    fn erased_characters_are_backspaces_before_the_typed_text() {
        assert_eq!(Erase::new(0, TARGET), None, "erasing nothing is no erase");
        assert_eq!(sent(rewrite(1, "")), "\x7f", "one emoji, one Backspace");
        assert_eq!(
            sent(rewrite(1, "e")),
            "\x7fe",
            "an accent removed: erase é, type e"
        );
        assert_eq!(sent(rewrite(3, "p ")), "\x7f\x7f\x7fp ");
        assert_eq!(
            status(&window_events(rewrite(2, ""), None)),
            ["none"],
            "a rewrite ends composition like any commit"
        );
    }

    #[test]
    fn a_paste_is_its_text_delivered_as_the_dropped_string_paste() {
        let events = window_events(Input::Paste("a\nb\tü".into()), None);
        assert!(
            matches!(events.as_slice(), [WindowEvent::DroppedString(text)] if text == "a\nb\tü"),
            "{events:?}"
        );
    }

    #[test]
    fn input_debug_output_carries_no_text_keys_or_characters() {
        let shown = [
            Input::Preedit("hunter2".into()),
            Input::Commit {
                erase: Erase::new(7, TARGET),
                text: "hunter2".into(),
                meta: 0x1000,
            },
            Input::Key {
                code: 36,
                unicode: 'h' as u32,
                meta: 0,
            },
            Input::Paste("hunter2".into()),
            Input::Touch {
                x: 1.0,
                y: 2.0,
                gesture: Gesture::Tap,
            },
        ]
        .map(|input| format!("{input:?}"));
        assert_eq!(
            shown,
            [
                "Preedit(<redacted>)",
                "Commit(<redacted>)",
                "Key(<redacted>)",
                "Paste(<redacted>)",
                "Touch(Tap)"
            ]
        );
    }

    fn mouse(events: Vec<WindowEvent>) -> Vec<(String, isize, isize, MouseButtons)> {
        events
            .into_iter()
            .map(|event| match event {
                WindowEvent::MouseEvent(m) => (
                    format!("{:?}", m.kind),
                    m.coords.x,
                    m.coords.y,
                    m.mouse_buttons,
                ),
                other => panic!("not a mouse event: {other:?}"),
            })
            .collect()
    }

    fn touch(x: f32, y: f32, gesture: Gesture, cell: Option<f32>) -> Vec<WindowEvent> {
        window_events(Input::Touch { x, y, gesture }, cell)
    }

    #[test]
    fn a_tap_is_a_left_click_at_the_touched_pixel() {
        assert_eq!(
            mouse(touch(10.4, 20.6, Gesture::Tap, None)),
            [
                ("Move".to_string(), 10, 21, MouseButtons::NONE),
                ("Press(Left)".to_string(), 10, 21, MouseButtons::LEFT),
                ("Release(Left)".to_string(), 10, 21, MouseButtons::NONE),
            ]
        );
    }

    #[test]
    fn a_long_press_drag_is_a_left_button_drag() {
        assert_eq!(
            mouse(touch(5.0, 6.0, Gesture::SelectStart, None)),
            [
                ("Move".to_string(), 5, 6, MouseButtons::NONE),
                ("Press(Left)".to_string(), 5, 6, MouseButtons::LEFT),
            ]
        );
        assert_eq!(
            mouse(touch(50.0, 6.0, Gesture::SelectMove, None)),
            [("Move".to_string(), 50, 6, MouseButtons::LEFT)]
        );
        assert_eq!(
            mouse(touch(50.0, 6.0, Gesture::SelectEnd, None)),
            [("Release(Left)".to_string(), 50, 6, MouseButtons::NONE)]
        );
    }

    #[test]
    fn scrolling_moves_one_line_per_cell_height_crossed() {
        let scroll = |from, to, cell| mouse(touch(1.0, 2.0, Gesture::Scroll { from, to }, cell));
        assert_eq!(scroll(0.0, 39.0, Some(40.0)), []);
        assert_eq!(
            scroll(39.0, 40.0, Some(40.0)),
            [("VertWheel(1)".to_string(), 1, 2, MouseButtons::NONE)]
        );
        assert_eq!(
            scroll(10.0, 130.0, Some(40.0)),
            [("VertWheel(3)".to_string(), 1, 2, MouseButtons::NONE)]
        );
        assert_eq!(
            scroll(0.0, -1.0, Some(40.0)),
            [("VertWheel(-1)".to_string(), 1, 2, MouseButtons::NONE)]
        );
        assert_eq!(
            scroll(-1.0, -40.0, Some(40.0)),
            [],
            "inside the first cell above the start"
        );
        assert_eq!(
            scroll(0.0, 400.0, None),
            [],
            "no cell height before the first paint"
        );
        assert_eq!(scroll(0.0, 400.0, Some(0.0)), []);
    }

    #[test]
    fn gesture_codes_are_parsed_at_the_boundary() {
        assert_eq!(
            Gesture::from_code(1, 2.0, 3.0),
            Some(Gesture::Scroll { from: 2.0, to: 3.0 })
        );
        assert_eq!(Gesture::from_code(4, 0.0, 0.0), Some(Gesture::SelectEnd));
        assert_eq!(Gesture::from_code(5, 0.0, 0.0), None);
    }
}

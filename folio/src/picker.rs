//! The picker: choose a thread from a list, when a command is given none.
//!
//! The repo's threads, most recently active first, with their state. Move with j/k or the arrow
//! keys (g and G for the ends), Enter to choose, q or Esc to leave without choosing. A picker can
//! have an entry at the top for a new thread.
//!
//! What it does with a key is Picker's, and testable without a terminal; what it draws is
//! `lines()`. crossterm only shows those and reads keys.

use std::io::Write;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::{cursor, execute, queue, terminal};

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    /// None: a new thread.
    pub thread: Option<String>,
}

pub fn new_thread() -> Choice {
    Choice {
        label: "+ new thread".into(),
        thread: None,
    }
}

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Enter,
    Down,
    Up,
    First,
    Last,
    Leave,
    Other,
}

impl Key {
    fn of(key: KeyEvent) -> Key {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => Key::Enter,
            KeyCode::Char('j' | 'm') if control => Key::Enter,
            KeyCode::Char('n') if control => Key::Down,
            KeyCode::Char('p') if control => Key::Up,
            KeyCode::Char('c') if control => Key::Leave,
            KeyCode::Char('j') | KeyCode::Down => Key::Down,
            KeyCode::Char('k') | KeyCode::Up => Key::Up,
            KeyCode::Char('g') | KeyCode::Home => Key::First,
            KeyCode::Char('G') | KeyCode::End => Key::Last,
            KeyCode::Char('q') | KeyCode::Esc => Key::Leave,
            _ => Key::Other,
        }
    }
}

pub struct Picker {
    title: String,
    choices: Vec<Choice>,
    pub index: usize,
    /// The first choice shown, when they don't all fit.
    top: usize,
}

impl Picker {
    pub fn new(title: &str, choices: Vec<Choice>) -> Picker {
        Picker {
            title: title.into(),
            choices,
            index: 0,
            top: 0,
        }
    }

    /// Act on a key: (done, what was chosen, or None for nothing).
    pub fn press(&mut self, key: Key) -> (bool, Option<Choice>) {
        let last = self.choices.len().saturating_sub(1);
        match key {
            Key::Enter if !self.choices.is_empty() => {
                return (true, Some(self.choices[self.index].clone()));
            }
            Key::Leave => return (true, None),
            Key::Down => self.index = (self.index + 1).min(last),
            Key::Up => self.index = self.index.saturating_sub(1),
            Key::First => self.index = 0,
            Key::Last => self.index = last,
            _ => {}
        }
        (false, None)
    }

    /// What to draw in `height` rows: (text, whether it's the current choice).
    pub fn lines(&mut self, height: usize) -> Vec<(String, bool)> {
        let rows = height.saturating_sub(2).max(1); // the title, and a blank line under it
        if self.index < self.top {
            self.top = self.index;
        } else if self.index >= self.top + rows {
            self.top = self.index + 1 - rows;
        }
        let mut lines = vec![(self.title.clone(), false), (String::new(), false)];
        for (i, choice) in self.choices.iter().enumerate().skip(self.top).take(rows) {
            let current = i == self.index;
            lines.push((
                format!("{} {}", if current { '>' } else { ' ' }, choice.label),
                current,
            ));
        }
        lines
    }
}

/// The terminal's size, as curses had it: $COLUMNS and $LINES if they're set, else the terminal's
/// own, else 80 by 24.
fn size() -> (u16, u16) {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|n| *n > 0)
    };
    let (width, height) = terminal::size().unwrap_or((0, 0));
    let or = |given: u16, default: u16| if given > 0 { given } else { default };
    (
        env("COLUMNS").unwrap_or(or(width, 80)),
        env("LINES").unwrap_or(or(height, 24)),
    )
}

/// Show the picker on this terminal; what was chosen, or None.
pub fn pick(title: &str, choices: Vec<Choice>) -> Result<Option<Choice>, String> {
    let mut picker = Picker::new(title, choices);
    let mut out = std::io::stdout();
    let failed = |e: std::io::Error| format!("the terminal: {e}");
    terminal::enable_raw_mode().map_err(failed)?;
    let shown = execute!(out, terminal::EnterAlternateScreen, cursor::Hide);
    let chosen = shown.and_then(|()| {
        loop {
            let (width, height) = size();
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
            for (row, (text, current)) in picker.lines(height as usize).into_iter().enumerate() {
                let text: String = text
                    .chars()
                    .take((width as usize).saturating_sub(1))
                    .collect();
                queue!(out, cursor::MoveTo(0, row as u16))?;
                if current {
                    queue!(
                        out,
                        SetAttribute(Attribute::Reverse),
                        Print(text),
                        SetAttribute(Attribute::Reset)
                    )?;
                } else {
                    queue!(out, Print(text))?;
                }
            }
            out.flush()?;
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                let (done, choice) = picker.press(Key::of(key));
                if done {
                    break Ok(choice);
                }
            }
        }
    });
    let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    chosen.map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker_of(n: usize, new: bool) -> Picker {
        let mut choices: Vec<Choice> = if new { vec![new_thread()] } else { vec![] };
        choices.extend((0..n).map(|i| Choice {
            label: format!("t{i}"),
            thread: Some(format!("t{i}")),
        }));
        Picker::new("title", choices)
    }

    fn key(code: KeyCode) -> Key {
        Key::of(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn moves_and_stops_at_the_ends() {
        let mut p = picker_of(3, false);
        for (k, index) in [
            (key(KeyCode::Char('k')), 0),
            (key(KeyCode::Char('j')), 1),
            (key(KeyCode::Down), 2),
            (key(KeyCode::Char('j')), 2),
            (key(KeyCode::Up), 1),
            (key(KeyCode::Char('G')), 2),
            (key(KeyCode::Char('g')), 0),
        ] {
            assert_eq!(p.press(k), (false, None));
            assert_eq!(p.index, index);
        }
    }

    #[test]
    fn enter_chooses_and_q_or_esc_or_control_c_leave() {
        let mut p = picker_of(3, false);
        p.press(Key::Down);
        assert_eq!(
            p.press(key(KeyCode::Enter)).1.unwrap().thread.as_deref(),
            Some("t1")
        );
        let control_c = Key::of(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        for k in [key(KeyCode::Char('q')), key(KeyCode::Esc), control_c] {
            assert_eq!(picker_of(3, false).press(k), (true, None));
        }
    }

    #[test]
    fn enter_on_nothing_does_nothing() {
        assert_eq!(picker_of(0, false).press(Key::Enter), (false, None));
    }

    #[test]
    fn the_new_entry_comes_first() {
        assert_eq!(
            picker_of(2, true).press(Key::Enter),
            (true, Some(new_thread()))
        );
    }

    #[test]
    fn draws_the_title_and_marks_the_current_choice() {
        let mut p = picker_of(2, false);
        p.press(Key::Down);
        let expected: Vec<(String, bool)> = [
            ("title", false),
            ("", false),
            ("  t0", false),
            ("> t1", true),
        ]
        .iter()
        .map(|(t, c)| (t.to_string(), *c))
        .collect();
        assert_eq!(p.lines(10), expected);
    }

    #[test]
    fn scrolls_to_keep_the_current_choice_in_view() {
        let mut p = picker_of(10, false);
        for _ in 0..6 {
            p.press(Key::Down);
        }
        let shown: Vec<String> = p.lines(5)[2..].iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(shown, ["  t4", "  t5", "> t6"]);
        for _ in 0..5 {
            p.press(Key::Up);
        }
        let shown: Vec<String> = p.lines(5)[2..].iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(shown, ["> t1", "  t2", "  t3"]);
    }
}

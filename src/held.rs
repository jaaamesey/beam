use std::collections::{HashMap, HashSet};

/// Keys and mouse buttons the client has pressed and not yet released.
#[derive(Default)]
pub struct Held {
    keys: HashMap<String, String>,
    buttons: HashSet<u16>,
}

impl Held {
    /// Returns the key to press. Repeats reuse the first one, since some keys map from
    /// the `key` character, which changes with modifiers (`-` vs `_`).
    pub fn press_key(&mut self, code: &str, key: &str) -> String {
        self.keys.entry(code.into()).or_insert_with(|| key.into()).clone()
    }

    /// Returns the key that was pressed for `code`, so the same key is released.
    pub fn release_key(&mut self, code: &str) -> Option<String> {
        self.keys.remove(code)
    }

    pub fn button(&mut self, button: u16, down: bool) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }

    pub fn take(&mut self) -> (Vec<(String, String)>, Vec<u16>) {
        (self.keys.drain().collect(), self.buttons.drain().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_presses_and_releases() {
        let mut held = Held::default();
        held.press_key("KeyW", "w");
        held.press_key("ShiftLeft", "Shift");
        held.release_key("KeyW");
        held.button(0, true);
        held.button(2, true);
        held.button(2, false);

        let (mut keys, buttons) = held.take();
        keys.sort();
        assert_eq!(keys, [("ShiftLeft".to_string(), "Shift".to_string())]);
        assert_eq!(buttons, [0]);
        assert_eq!(held.take(), (vec![], vec![]));
    }

    #[test]
    fn releases_and_repeats_the_key_that_was_pressed() {
        let mut held = Held::default();
        assert_eq!(held.press_key("Minus", "-"), "-");
        // Shift goes down while Minus is held: repeats and the release say `_`.
        assert_eq!(held.press_key("Minus", "_"), "-");
        assert_eq!(held.release_key("Minus").as_deref(), Some("-"));
        assert_eq!(held.release_key("Minus"), None);
    }
}

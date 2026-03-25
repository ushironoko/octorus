use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

use crate::keybinding::{
    event_to_keybinding, KeyBinding, KeySequence, SequenceMatch, SEQUENCE_TIMEOUT,
};

use super::App;

impl App {
    pub(crate) fn check_sequence_timeout(&mut self) {
        if let Some(since) = self.pending_since {
            if since.elapsed() > SEQUENCE_TIMEOUT {
                self.pending_keys.clear();
                self.pending_since = None;
            }
        }
    }

    /// Add a key to pending sequence
    pub(crate) fn push_pending_key(&mut self, key: KeyBinding) {
        if self.pending_keys.is_empty() {
            self.pending_since = Some(Instant::now());
        }
        self.pending_keys.push(key);
    }

    /// Clear pending keys
    pub(crate) fn clear_pending_keys(&mut self) {
        self.pending_keys.clear();
        self.pending_since = None;
    }

    /// Check if a KeyEvent matches a KeySequence (single-key sequences only).
    /// Also checks alternative sequences.
    pub(crate) fn matches_single_key(&self, event: &KeyEvent, seq: &KeySequence) -> bool {
        // Check primary
        if seq.keys.len() == 1 {
            if let Some(first) = seq.keys.first() {
                if first.matches(event) {
                    return true;
                }
            }
        }
        // Check alternatives
        for alt in &seq.alt {
            if alt.len() == 1 {
                if let Some(first) = alt.first() {
                    if first.matches(event) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// True for uppercase shortcuts like `J`/`K` without Ctrl/Alt modifiers.
    pub(crate) fn is_shift_char_shortcut(event: &KeyEvent, lower: char) -> bool {
        if event.modifiers.contains(KeyModifiers::CONTROL)
            || event.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }

        let upper = lower.to_ascii_uppercase();
        match event.code {
            KeyCode::Char(c) if c == upper => true,
            KeyCode::Char(c) if c == lower && event.modifiers.contains(KeyModifiers::SHIFT) => true,
            _ => false,
        }
    }

    /// Try to match pending keys against a sequence (primary + alternatives).
    /// Returns SequenceMatch::Full if fully matched, Partial if prefix matches, None otherwise.
    pub(crate) fn try_match_sequence(&self, seq: &KeySequence) -> SequenceMatch {
        if self.pending_keys.is_empty() {
            return SequenceMatch::None;
        }

        let mut best = SequenceMatch::None;
        for keys in seq.all_sequences() {
            let result = self.match_against_keys(keys);
            match result {
                SequenceMatch::Full => return SequenceMatch::Full,
                SequenceMatch::Partial => best = SequenceMatch::Partial,
                SequenceMatch::None => {}
            }
        }
        best
    }

    fn match_against_keys(&self, keys: &[KeyBinding]) -> SequenceMatch {
        let pending_len = self.pending_keys.len();
        if pending_len > keys.len() {
            return SequenceMatch::None;
        }
        for (i, pending) in self.pending_keys.iter().enumerate() {
            if *pending != keys[i] {
                return SequenceMatch::None;
            }
        }
        if pending_len == keys.len() {
            SequenceMatch::Full
        } else {
            SequenceMatch::Partial
        }
    }

    /// Check if current key event starts or continues a sequence that could match (primary + alternatives)
    pub(crate) fn key_could_match_sequence(&self, event: &KeyEvent, seq: &KeySequence) -> bool {
        let Some(kb) = event_to_keybinding(event) else {
            return false;
        };

        for keys in seq.all_sequences() {
            if self.could_match_keys(&kb, keys) {
                return true;
            }
        }
        false
    }

    fn could_match_keys(&self, kb: &KeyBinding, keys: &[KeyBinding]) -> bool {
        if self.pending_keys.is_empty() {
            return keys.first().map(|first| *first == *kb).unwrap_or(false);
        }

        let pending_len = self.pending_keys.len();
        if pending_len >= keys.len() {
            return false;
        }

        for (i, pending) in self.pending_keys.iter().enumerate() {
            if *pending != keys[i] {
                return false;
            }
        }

        keys.get(pending_len)
            .map(|expected| *expected == *kb)
            .unwrap_or(false)
    }
}

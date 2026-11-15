//! 全拼组词与候选选择。只有 confirm 生成正文，取消和失焦丢弃预编辑。

use crate::dictionary::{Candidate, Dictionary};

pub const PAGE: usize = 5;
const MAX_SPELLING: usize = 48;

#[derive(Default)]
pub struct Composition {
    pub spell: String,
    pub cursor: usize,
    candidates: Vec<Candidate>,
    selected: usize,
}

impl Composition {
    pub fn clear(&mut self) {
        self.spell.clear();
        self.cursor = 0;
        self.candidates.clear();
        self.selected = 0;
    }

    fn search(&mut self, dictionary: &Dictionary<'_>) {
        self.selected = 0;
        self.candidates = dictionary.candidates(&self.spell);
    }

    pub fn insert(&mut self, key: char, dictionary: &Dictionary<'_>) {
        if self.spell.len() == MAX_SPELLING {
            return;
        }
        self.spell.insert(self.cursor, key);
        self.cursor += 1;
        self.search(dictionary);
    }

    pub fn backspace(&mut self, dictionary: &Dictionary<'_>) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.spell.remove(self.cursor);
        self.search(dictionary);
    }

    pub fn delete(&mut self, dictionary: &Dictionary<'_>) {
        if self.cursor == self.spell.len() {
            return;
        }
        self.spell.remove(self.cursor);
        self.search(dictionary);
    }

    pub fn page(&self) -> &[Candidate] {
        let start = self.selected / PAGE * PAGE;
        let end = (start + PAGE).min(self.candidates.len());
        &self.candidates[start..end]
    }

    pub fn position(&self) -> usize {
        self.selected % PAGE
    }

    pub fn navigate(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.candidates.len().saturating_sub(1));
    }

    pub fn confirm(&mut self, index: usize, dictionary: &Dictionary<'_>) -> Option<String> {
        let candidate = self.page().get(index)?;
        let consumed = candidate.consumed;
        let text = candidate.text.clone();
        self.spell.drain(..consumed);
        self.cursor = self.spell.len();
        self.search(dictionary);
        Some(text)
    }

    pub fn raw(&mut self) -> String {
        let text = std::mem::take(&mut self.spell);
        self.clear();
        text
    }
}

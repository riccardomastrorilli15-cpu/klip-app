//! In-memory history. Index 0 is the newest entry. Nothing here touches the disk.

pub const MAX_ITEMS: usize = 50;
pub const MAX_PINS: usize = 20;
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub text: String,
    pub pinned: bool,
}

#[derive(Default)]
pub struct History {
    items: Vec<Entry>,
    next_id: u64,
}

impl History {
    pub fn items(&self) -> &[Entry] {
        &self.items
    }

    pub fn text(&self, id: u64) -> Option<&str> {
        self.items
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.text.as_str())
    }

    /// Adds `text` as the newest entry. A duplicate is moved to the top and keeps its pin.
    pub fn push(&mut self, text: String) -> bool {
        if text.trim().is_empty() || text.len() > MAX_ENTRY_BYTES {
            return false;
        }
        let pinned = match self.items.iter().position(|e| e.text == text) {
            Some(i) => self.items.remove(i).pinned,
            None => false,
        };
        self.next_id += 1;
        self.items.insert(
            0,
            Entry {
                id: self.next_id,
                text,
                pinned,
            },
        );
        while self.items.len() > MAX_ITEMS {
            match self.items.iter().rposition(|e| !e.pinned) {
                Some(i) => {
                    self.items.remove(i);
                }
                None => break,
            }
        }
        true
    }

    pub fn set_pinned(&mut self, id: u64, pinned: bool) {
        let pins = self.items.iter().filter(|e| e.pinned).count();
        if let Some(e) = self.items.iter_mut().find(|e| e.id == id) {
            if pinned && !e.pinned && pins >= MAX_PINS {
                return;
            }
            e.pinned = pinned;
        }
    }

    pub fn delete(&mut self, id: u64) {
        self.items.retain(|e| e.id != id);
    }

    pub fn clear_unpinned(&mut self) {
        self.items.retain(|e| e.pinned);
        self.items.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_oversized() {
        let mut h = History::default();
        assert!(!h.push("  \n\t".into()));
        assert!(!h.push("x".repeat(MAX_ENTRY_BYTES + 1)));
        assert!(h.push("x".repeat(MAX_ENTRY_BYTES)));
    }

    #[test]
    fn duplicate_moves_to_top_and_keeps_pin() {
        let mut h = History::default();
        h.push("a".into());
        h.push("b".into());
        let a = h.items()[1].id;
        h.set_pinned(a, true);
        h.push("a".into());
        assert_eq!(h.items().len(), 2);
        assert_eq!(h.items()[0].text, "a");
        assert!(h.items()[0].pinned);
    }

    #[test]
    fn eviction_spares_pins() {
        let mut h = History::default();
        h.push("keep".into());
        let keep = h.items()[0].id;
        h.set_pinned(keep, true);
        for i in 0..MAX_ITEMS + 10 {
            h.push(format!("item {i}"));
        }
        assert_eq!(h.items().len(), MAX_ITEMS);
        assert!(h.items().iter().any(|e| e.text == "keep"));
        assert_eq!(h.items()[0].text, format!("item {}", MAX_ITEMS + 9));
    }

    #[test]
    fn pin_cap_and_clear() {
        let mut h = History::default();
        for i in 0..MAX_PINS + 3 {
            h.push(format!("p{i}"));
            let id = h.items()[0].id;
            h.set_pinned(id, true);
        }
        assert_eq!(h.items().iter().filter(|e| e.pinned).count(), MAX_PINS);
        h.clear_unpinned();
        assert_eq!(h.items().len(), MAX_PINS);
        let id = h.items()[0].id;
        h.delete(id);
        assert_eq!(h.items().len(), MAX_PINS - 1);
        assert!(h.text(id).is_none());
    }
}

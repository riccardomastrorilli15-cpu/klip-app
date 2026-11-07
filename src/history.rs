//! In-memory history. Index 0 is the newest entry. Nothing here touches the disk.

use crate::image::Thumb;

pub const MAX_ITEMS: usize = 50;
pub const MAX_PINS: usize = 20;
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
pub const MAX_IMAGES: usize = 8;
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGES_TOTAL: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Hash of the raw pixels, used to spot duplicates.
    pub key: u64,
    pub thumb: Thumb,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Image(Picture),
}

impl Content {
    fn same(&self, other: &Content) -> bool {
        match (self, other) {
            (Content::Text(a), Content::Text(b)) => a == b,
            (Content::Image(a), Content::Image(b)) => a.key == b.key,
            _ => false,
        }
    }

    fn image_bytes(&self) -> usize {
        match self {
            Content::Image(p) => p.png.len(),
            Content::Text(_) => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub pinned: bool,
    pub content: Content,
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

    pub fn content(&self, id: u64) -> Option<&Content> {
        self.items.iter().find(|e| e.id == id).map(|e| &e.content)
    }

    /// Adds `content` as the newest entry. A duplicate is moved to the top and keeps its pin.
    pub fn push(&mut self, content: Content) -> bool {
        let ok = match &content {
            Content::Text(t) => !t.trim().is_empty() && t.len() <= MAX_ENTRY_BYTES,
            Content::Image(p) => p.png.len() <= MAX_IMAGE_BYTES,
        };
        if !ok {
            return false;
        }
        let pinned = match self.items.iter().position(|e| e.content.same(&content)) {
            Some(i) => self.items.remove(i).pinned,
            None => false,
        };
        self.next_id += 1;
        let id = self.next_id;
        self.items.insert(
            0,
            Entry {
                id,
                pinned,
                content,
            },
        );

        while self.items.len() > MAX_ITEMS {
            if !self.evict(|_| true) {
                break;
            }
        }
        loop {
            let count = self
                .items
                .iter()
                .filter(|e| e.content.image_bytes() > 0)
                .count();
            let bytes: usize = self.items.iter().map(|e| e.content.image_bytes()).sum();
            if count <= MAX_IMAGES && bytes <= MAX_IMAGES_TOTAL {
                break;
            }
            if !self.evict(|e| e.content.image_bytes() > 0) {
                break;
            }
        }
        self.items.iter().any(|e| e.id == id)
    }

    // Drops the oldest unpinned entry matching `pred`.
    fn evict(&mut self, pred: impl Fn(&Entry) -> bool) -> bool {
        match self.items.iter().rposition(|e| !e.pinned && pred(e)) {
            Some(i) => {
                self.items.remove(i);
                true
            }
            None => false,
        }
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

    fn text(s: &str) -> Content {
        Content::Text(s.into())
    }

    fn pic(key: u64, bytes: usize) -> Content {
        Content::Image(Picture {
            png: vec![0; bytes],
            width: 2,
            height: 2,
            key,
            thumb: Thumb {
                w: 1,
                h: 1,
                rgba: vec![0; 4],
            },
        })
    }

    #[test]
    fn rejects_empty_and_oversized() {
        let mut h = History::default();
        assert!(!h.push(text("  \n\t")));
        assert!(!h.push(text(&"x".repeat(MAX_ENTRY_BYTES + 1))));
        assert!(h.push(text(&"x".repeat(MAX_ENTRY_BYTES))));
        assert!(!h.push(pic(1, MAX_IMAGE_BYTES + 1)));
    }

    #[test]
    fn duplicate_moves_to_top_and_keeps_pin() {
        let mut h = History::default();
        h.push(text("a"));
        h.push(text("b"));
        let a = h.items()[1].id;
        h.set_pinned(a, true);
        h.push(text("a"));
        assert_eq!(h.items().len(), 2);
        assert_eq!(h.items()[0].content, text("a"));
        assert!(h.items()[0].pinned);
    }

    #[test]
    fn duplicate_image_detected_by_key() {
        let mut h = History::default();
        h.push(pic(7, 100));
        h.push(text("x"));
        h.push(pic(7, 100));
        assert_eq!(h.items().len(), 2);
        assert!(matches!(h.items()[0].content, Content::Image(_)));
    }

    #[test]
    fn eviction_spares_pins() {
        let mut h = History::default();
        h.push(text("keep"));
        let keep = h.items()[0].id;
        h.set_pinned(keep, true);
        for i in 0..MAX_ITEMS + 10 {
            h.push(text(&format!("item {i}")));
        }
        assert_eq!(h.items().len(), MAX_ITEMS);
        assert!(h.items().iter().any(|e| e.content == text("keep")));
        assert_eq!(
            h.items()[0].content,
            text(&format!("item {}", MAX_ITEMS + 9))
        );
    }

    #[test]
    fn image_count_and_byte_budgets() {
        let mut h = History::default();
        for k in 0..MAX_IMAGES as u64 + 3 {
            h.push(pic(k, 1000));
        }
        let imgs = |h: &History| {
            h.items()
                .iter()
                .filter(|e| e.content.image_bytes() > 0)
                .count()
        };
        assert_eq!(imgs(&h), MAX_IMAGES);
        // byte budget: 5 x 7 MB would be 35 MB > 32 MB, so the oldest goes
        let mut h = History::default();
        for k in 0..5 {
            h.push(pic(100 + k, 7 * 1024 * 1024));
        }
        assert_eq!(imgs(&h), 4);
        assert!(h
            .items()
            .iter()
            .all(|e| e.content != pic(100, 7 * 1024 * 1024)));
    }

    #[test]
    fn pinned_images_are_never_evicted_and_new_one_is_rejected_instead() {
        let mut h = History::default();
        for k in 0..MAX_IMAGES as u64 {
            h.push(pic(k, 1000));
            let id = h.items()[0].id;
            h.set_pinned(id, true);
        }
        assert!(!h.push(pic(99, 1000)));
        assert_eq!(h.items().len(), MAX_IMAGES);
    }

    #[test]
    fn pin_cap_and_clear() {
        let mut h = History::default();
        for i in 0..MAX_PINS + 3 {
            h.push(text(&format!("p{i}")));
            let id = h.items()[0].id;
            h.set_pinned(id, true);
        }
        assert_eq!(h.items().iter().filter(|e| e.pinned).count(), MAX_PINS);
        h.clear_unpinned();
        assert_eq!(h.items().len(), MAX_PINS);
        let id = h.items()[0].id;
        h.delete(id);
        assert_eq!(h.items().len(), MAX_PINS - 1);
        assert!(h.content(id).is_none());
    }
}

//! Wire format between the daemon and the picker process. Plain lines over pipes,
//! text is hex-encoded so any clipboard content survives. No serde, no sockets.
//!
//! daemon -> picker (stdin):  `<id> <0|1 pinned> <hex(utf8)>\n` per entry, then EOF
//! picker -> daemon (stdout): `copy <id>` | `pin <id>` | `unpin <id>` | `del <id>`

use crate::history::Entry;

#[derive(Debug, PartialEq)]
pub enum Action {
    Copy(u64),
    Pin(u64, bool),
    Delete(u64),
}

impl Action {
    pub fn encode(&self) -> String {
        match self {
            Action::Copy(id) => format!("copy {id}"),
            Action::Pin(id, true) => format!("pin {id}"),
            Action::Pin(id, false) => format!("unpin {id}"),
            Action::Delete(id) => format!("del {id}"),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let (cmd, id) = line.trim().split_once(' ')?;
        let id: u64 = id.parse().ok()?;
        match cmd {
            "copy" => Some(Action::Copy(id)),
            "pin" => Some(Action::Pin(id, true)),
            "unpin" => Some(Action::Pin(id, false)),
            "del" => Some(Action::Delete(id)),
            _ => None,
        }
    }
}

pub fn encode_entries(items: &[Entry]) -> String {
    let mut out = String::new();
    for e in items {
        out.push_str(&format!(
            "{} {} {}\n",
            e.id,
            u8::from(e.pinned),
            hex(&e.text)
        ));
    }
    out
}

pub fn decode_entry(line: &str) -> Option<Entry> {
    let mut it = line.trim_end().splitn(3, ' ');
    let id = it.next()?.parse().ok()?;
    let pinned = it.next()? == "1";
    let text = unhex(it.next()?)?;
    Some(Entry { id, text, pinned })
}

fn hex(s: &str) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(D[(b >> 4) as usize] as char);
        out.push(D[(b & 15) as usize] as char);
    }
    out
}

fn unhex(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() & 1 == 1 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut it = b.iter();
    while let (Some(&h), Some(&l)) = (it.next(), it.next()) {
        out.push(nib(h)? << 4 | nib(l)?);
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_roundtrip_including_newlines_and_unicode() {
        let items = vec![
            Entry {
                id: 7,
                text: "line1\nline2\t\"quoted\"".into(),
                pinned: true,
            },
            Entry {
                id: 9,
                text: "日本語 🙂 ñ".into(),
                pinned: false,
            },
        ];
        let wire = encode_entries(&items);
        let back: Vec<_> = wire.lines().filter_map(decode_entry).collect();
        assert_eq!(back, items);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_entry("1 0 zz").is_none());
        assert!(decode_entry("1 0 abc").is_none());
        assert!(decode_entry("x 0 61").is_none());
        assert!(Action::decode("copy nope").is_none());
        assert!(Action::decode("rm 1").is_none());
    }

    #[test]
    fn actions_roundtrip() {
        for a in [
            Action::Copy(1),
            Action::Pin(2, true),
            Action::Pin(3, false),
            Action::Delete(4),
        ] {
            assert_eq!(Action::decode(&a.encode()), Some(a));
        }
    }
}

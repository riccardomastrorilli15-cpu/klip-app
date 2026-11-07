//! Wire format between the daemon and the picker process. Plain lines over pipes,
//! payloads are hex-encoded so any clipboard content survives. No serde, no sockets.
//!
//! daemon -> picker (stdin), then EOF:
//!   `# hotkey <hex(label)>` or `# nohotkey`
//!   `<id> <0|1 pinned> t <hex(utf8 text)>`
//!   `<id> <0|1 pinned> i <thumb_w> <thumb_h> <w> <h> <png_bytes> <hex(thumb rgba)>`
//! picker -> daemon (stdout): `copy <id>` | `pin <id>` | `unpin <id>` | `del <id>`
//!
//! Full-size images never cross the pipe: the picker only gets the thumbnail.

use crate::history::{Content, Entry};
use crate::image::Thumb;

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

#[derive(Debug, PartialEq)]
pub enum WireKind {
    Text(String),
    Image {
        thumb: Thumb,
        width: u32,
        height: u32,
        bytes: usize,
    },
}

#[derive(Debug, PartialEq)]
pub struct WireEntry {
    pub id: u64,
    pub pinned: bool,
    pub kind: WireKind,
}

#[derive(Debug, PartialEq)]
pub enum Line {
    Hotkey(Option<String>),
    Entry(WireEntry),
}

pub fn encode_header(hotkey: Option<&str>) -> String {
    match hotkey {
        Some(h) => format!("# hotkey {}\n", hex(h)),
        None => "# nohotkey\n".to_string(),
    }
}

pub fn encode_entries(items: &[Entry]) -> String {
    let mut out = String::new();
    for e in items {
        let pinned = u8::from(e.pinned);
        match &e.content {
            Content::Text(t) => out.push_str(&format!("{} {pinned} t {}\n", e.id, hex(t))),
            Content::Image(p) => {
                let t = &p.thumb;
                let line = format!(
                    "{} {pinned} i {} {} {} {} {} {}\n",
                    e.id,
                    t.w,
                    t.h,
                    p.width,
                    p.height,
                    p.png.len(),
                    hex_bytes(&t.rgba)
                );
                out.push_str(&line);
            }
        }
    }
    out
}

pub fn decode_line(line: &str) -> Option<Line> {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("# ") {
        return match rest.split_once(' ') {
            Some(("hotkey", h)) => Some(Line::Hotkey(Some(unhex(h)?))),
            _ if rest == "nohotkey" => Some(Line::Hotkey(None)),
            _ => None,
        };
    }
    let mut it = line.splitn(4, ' ');
    let id = it.next()?.parse().ok()?;
    let pinned = it.next()? == "1";
    let kind = match it.next()? {
        "t" => WireKind::Text(unhex(it.next()?)?),
        "i" => {
            let mut f = it.next()?.splitn(6, ' ');
            let (tw, th): (u32, u32) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
            let (width, height): (u32, u32) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
            let bytes: usize = f.next()?.parse().ok()?;
            let rgba = unhex_bytes(f.next()?)?;
            if tw == 0 || th == 0 || rgba.len() as u64 != u64::from(tw) * u64::from(th) * 4 {
                return None;
            }
            WireKind::Image {
                thumb: Thumb { w: tw, h: th, rgba },
                width,
                height,
                bytes,
            }
        }
        _ => return None,
    };
    Some(Line::Entry(WireEntry { id, pinned, kind }))
}

fn hex(s: &str) -> String {
    hex_bytes(s.as_bytes())
}

fn hex_bytes(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for &x in b {
        out.push(D[(x >> 4) as usize] as char);
        out.push(D[(x & 15) as usize] as char);
    }
    out
}

fn unhex(s: &str) -> Option<String> {
    String::from_utf8(unhex_bytes(s)?).ok()
}

fn unhex_bytes(s: &str) -> Option<Vec<u8>> {
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
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Picture;

    fn entry(id: u64, pinned: bool, content: Content) -> Entry {
        Entry {
            id,
            pinned,
            content,
        }
    }

    #[test]
    fn text_roundtrip_including_newlines_and_unicode() {
        let items = vec![
            entry(7, true, Content::Text("line1\nline2\t\"quoted\"".into())),
            entry(9, false, Content::Text("日本語 🙂 ñ".into())),
        ];
        let wire = encode_entries(&items);
        let back: Vec<_> = wire.lines().filter_map(decode_line).collect();
        assert_eq!(
            back,
            vec![
                Line::Entry(WireEntry {
                    id: 7,
                    pinned: true,
                    kind: WireKind::Text("line1\nline2\t\"quoted\"".into())
                }),
                Line::Entry(WireEntry {
                    id: 9,
                    pinned: false,
                    kind: WireKind::Text("日本語 🙂 ñ".into())
                }),
            ]
        );
    }

    #[test]
    fn image_roundtrip_sends_only_the_thumbnail() {
        let thumb = Thumb {
            w: 2,
            h: 1,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let pic = Picture {
            png: vec![0; 1234],
            width: 640,
            height: 480,
            key: 1,
            thumb: thumb.clone(),
        };
        let wire = encode_entries(&[entry(3, false, Content::Image(pic))]);
        assert!(wire.len() < 200, "full image must not be on the wire");
        match decode_line(wire.trim_end()) {
            Some(Line::Entry(WireEntry {
                id: 3,
                pinned: false,
                kind:
                    WireKind::Image {
                        thumb: t,
                        width: 640,
                        height: 480,
                        bytes: 1234,
                    },
            })) => assert_eq!(t, thumb),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn header_roundtrip() {
        assert_eq!(
            decode_line(&encode_header(Some("Alt+Shift+V"))),
            Some(Line::Hotkey(Some("Alt+Shift+V".into())))
        );
        assert_eq!(decode_line(&encode_header(None)), Some(Line::Hotkey(None)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_line("1 0 t zz").is_none());
        assert!(decode_line("1 0 t abc").is_none());
        assert!(decode_line("x 0 t 61").is_none());
        assert!(decode_line("1 0 i 2 1 5 5 9 00").is_none()); // thumbnail length mismatch
        assert!(decode_line("1 0 i 0 0 5 5 9 ").is_none());
        assert!(decode_line("1 0 q 61").is_none());
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

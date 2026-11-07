//! The picker window. It runs as its own short-lived process, so all UI and GL memory
//! goes back to the OS the moment it closes. The daemon never links a window into its loop.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions,
    FontFamily, FontId, Frame, Key, Margin, Modifiers, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextEdit, Vec2,
};

use crate::{
    history::Entry,
    proto::{self, Action},
};

const ACCENT: Color32 = Color32::from_rgb(59, 130, 246);
const ROW_H: f32 = 46.0;
const NUM_KEYS: [Key; 9] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

#[cfg(target_os = "macos")]
const MOD: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
const MOD: &str = "Ctrl";

struct Row {
    entry: Entry,
    preview: String,
    lower: String,
}

enum Pending {
    Copy(usize),
    Pin(usize),
    Delete(usize),
}

struct Picker {
    rows: Vec<Row>,
    query: String,
    last_query: String,
    sel: usize,
    scroll_to_sel: bool,
    reselect: Option<u64>,
    had_focus: bool,
}

pub fn run() -> ! {
    let rows = read_rows();
    let fallback = rows.iter().any(|r| needs_fallback_font(&r.preview));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("klip")
            .with_inner_size([460.0, 540.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(false),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let result = eframe::run_native(
        "klip",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx, fallback);
            Ok(Box::new(Picker {
                rows,
                query: String::new(),
                last_query: String::new(),
                sel: 0,
                scroll_to_sel: false,
                reselect: None,
                had_focus: false,
            }))
        }),
    );
    if let Err(e) = result {
        eprintln!("klip picker: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn read_rows() -> Vec<Row> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Vec::new();
    }
    let mut rows: Vec<Row> = stdin
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| proto::decode_entry(&l))
        .map(|entry| Row {
            preview: preview(&entry.text),
            lower: entry.text.to_lowercase(),
            entry,
        })
        .collect();
    rows.sort_by_key(|r| !r.entry.pinned); // stable: pinned first, recency kept inside each group
    rows
}

fn preview(text: &str) -> String {
    let head: String = text.chars().take(600).collect();
    let flat = head.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(220).collect()
}

// Scripts the bundled font cannot draw (CJK, Arabic, Hebrew, Thai, Indic...).
fn needs_fallback_font(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x0590..=0x1DFF | 0x2E80..=0xFFEF | 0x20000..=0x2FFFF))
}

fn load_fallback_font() -> Option<FontData> {
    const PATHS: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\malgun.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    ];
    PATHS
        .iter()
        .find_map(|p| std::fs::read(p).ok())
        .map(FontData::from_owned)
}

fn setup(ctx: &egui::Context, fallback: bool) {
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(0.0, 4.0);
        s.visuals.selection.bg_fill = ACCENT;
    });
    if fallback {
        if let Some(data) = load_fallback_font() {
            let mut fonts = FontDefinitions::default();
            fonts.font_data.insert("fallback".into(), Arc::new(data));
            for fam in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts
                    .families
                    .entry(fam)
                    .or_default()
                    .push("fallback".into());
            }
            ctx.set_fonts(fonts);
        }
    }
}

fn emit(a: &Action) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{}", a.encode());
    let _ = out.flush();
}

impl Picker {
    fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        (0..self.rows.len())
            .filter(|&i| q.is_empty() || self.rows[i].lower.contains(&q))
            .collect()
    }

    fn apply(&mut self, ctx: &egui::Context, p: Pending) {
        match p {
            Pending::Copy(i) => {
                emit(&Action::Copy(self.rows[i].entry.id));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Pending::Pin(i) => {
                let id = self.rows[i].entry.id;
                let pinned = !self.rows[i].entry.pinned;
                self.rows[i].entry.pinned = pinned;
                emit(&Action::Pin(id, pinned));
                self.rows.sort_by_key(|r| !r.entry.pinned);
                self.reselect = Some(id);
                self.scroll_to_sel = true;
            }
            Pending::Delete(i) => {
                emit(&Action::Delete(self.rows[i].entry.id));
                self.rows.remove(i);
            }
        }
        ctx.request_repaint();
    }
}

impl eframe::App for Picker {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close when the user clicks away, but only after we have been focused once:
        // some window managers deliver focus late.
        match ctx.input(|i| i.viewport().focused) {
            Some(true) => self.had_focus = true,
            Some(false) if self.had_focus => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            _ => {}
        }
        if !self.had_focus {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // Consume keys before the text field sees them.
        let (esc, down, up, enter, pin, del, quick) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::COMMAND, Key::P),
                i.consume_key(Modifiers::COMMAND, Key::D),
                NUM_KEYS
                    .iter()
                    .position(|k| i.consume_key(Modifiers::COMMAND, *k)),
            )
        });
        if esc {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let visuals = ctx.style().visuals.clone();
        let panel = |top: i8, bottom: i8| {
            Frame::new().fill(visuals.panel_fill).inner_margin(Margin {
                left: 12,
                right: 12,
                top,
                bottom,
            })
        };

        egui::TopBottomPanel::top("search")
            .frame(panel(12, 6))
            .show_separator_line(false)
            .show(ctx, |ui| {
                let r = ui.add(
                    TextEdit::singleline(&mut self.query)
                        .hint_text("Search clipboard")
                        .font(FontId::proportional(16.0))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(12, 10)),
                );
                r.request_focus();
            });

        if self.query != self.last_query {
            self.last_query = self.query.clone();
            self.sel = 0;
            self.scroll_to_sel = true;
        }
        let visible = self.visible();
        if let Some(id) = self.reselect.take() {
            if let Some(p) = visible.iter().position(|&i| self.rows[i].entry.id == id) {
                self.sel = p;
            }
        }
        self.sel = self.sel.min(visible.len().saturating_sub(1));
        if down && self.sel + 1 < visible.len() {
            self.sel += 1;
            self.scroll_to_sel = true;
        }
        if up && self.sel > 0 {
            self.sel -= 1;
            self.scroll_to_sel = true;
        }

        let mut pending = None;
        if enter {
            pending = visible.get(self.sel).map(|&i| Pending::Copy(i));
        }
        if let Some(n) = quick {
            pending = visible.get(n).map(|&i| Pending::Copy(i));
        }
        if pin {
            pending = visible.get(self.sel).map(|&i| Pending::Pin(i));
        }
        if del {
            pending = visible.get(self.sel).map(|&i| Pending::Delete(i));
        }

        egui::TopBottomPanel::bottom("hints")
            .frame(panel(6, 10))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Enter copy  ·  {MOD}+1-9 quick  ·  {MOD}+P pin  ·  {MOD}+D remove  ·  Esc close"
                    ))
                    .size(12.0)
                    .weak(),
                );
            });

        egui::CentralPanel::default()
            .frame(panel(0, 0))
            .show(ctx, |ui| {
                if visible.is_empty() {
                    ui.add_space(40.0);
                    let msg = if self.rows.is_empty() {
                        "Nothing copied yet"
                    } else {
                        "No matches"
                    };
                    ui.vertical_centered(|ui| ui.label(RichText::new(msg).weak()));
                    return;
                }
                let moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (pos, &i) in visible.iter().enumerate() {
                            if let Some(p) = self.row(ui, pos, i, moved) {
                                pending = Some(p);
                            }
                        }
                    });
                self.scroll_to_sel = false;
            });

        // 1px border: the window has no native frame.
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("border"),
        ))
        .rect_stroke(
            ctx.screen_rect(),
            CornerRadius::ZERO,
            Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
            StrokeKind::Inside,
        );

        if let Some(p) = pending {
            self.apply(ctx, p);
        }
    }
}

impl Picker {
    fn row(&mut self, ui: &mut egui::Ui, pos: usize, i: usize, moved: bool) -> Option<Pending> {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
        if resp.hovered() && moved {
            self.sel = pos;
        }
        let selected = pos == self.sel;
        if selected && self.scroll_to_sel {
            resp.scroll_to_me(Some(Align::Center));
        }
        let hovered = ui.rect_contains_pointer(rect);
        let row = &self.rows[i];
        let weak = ui.visuals().weak_text_color();
        let strong = ui.visuals().strong_text_color();
        let p = ui.painter();

        if selected {
            p.rect_filled(rect, CornerRadius::same(8), ACCENT.gamma_multiply(0.18));
        }
        if pos < 9 {
            p.text(
                Pos2::new(rect.left() + 16.0, rect.center().y),
                Align2::CENTER_CENTER,
                (pos + 1).to_string(),
                FontId::monospace(12.0),
                weak,
            );
        }
        let text_left = rect.left() + 34.0;
        let text_w = (rect.right() - 62.0 - text_left).max(40.0);
        let mut job = LayoutJob::simple(
            row.preview.clone(),
            FontId::proportional(14.0),
            if selected {
                strong
            } else {
                ui.visuals().text_color()
            },
            text_w,
        );
        job.wrap.max_rows = 2;
        // Break inside words only when a token is too long to fit (URLs, hashes).
        job.wrap.break_anywhere = row.preview.split(' ').any(|w| w.chars().count() > 36);
        let galley = p.layout_job(job);
        p.galley(
            Pos2::new(text_left, rect.center().y - galley.size().y / 2.0),
            galley,
            Color32::PLACEHOLDER,
        );

        let pin_c = Pos2::new(rect.right() - 46.0, rect.center().y);
        let del_c = Pos2::new(rect.right() - 18.0, rect.center().y);
        let pinned = row.entry.pinned;
        let id = row.entry.id;
        let mut out = None;

        if selected || hovered {
            let pin_r = ui.interact(
                Rect::from_center_size(pin_c, Vec2::splat(26.0)),
                ui.id().with(("pin", id)),
                Sense::click(),
            );
            let del_r = ui.interact(
                Rect::from_center_size(del_c, Vec2::splat(26.0)),
                ui.id().with(("del", id)),
                Sense::click(),
            );
            let p = ui.painter();
            let pc = if pinned || pin_r.hovered() {
                ACCENT
            } else {
                weak
            };
            p.circle(
                pin_c,
                5.0,
                if pinned { ACCENT } else { Color32::TRANSPARENT },
                Stroke::new(1.5_f32, pc),
            );
            let dc = if del_r.hovered() {
                Color32::from_rgb(239, 68, 68)
            } else {
                weak
            };
            let s = Stroke::new(1.5_f32, dc);
            p.line_segment(
                [del_c + Vec2::new(-4.5, -4.5), del_c + Vec2::new(4.5, 4.5)],
                s,
            );
            p.line_segment(
                [del_c + Vec2::new(-4.5, 4.5), del_c + Vec2::new(4.5, -4.5)],
                s,
            );
            if pin_r.clicked() {
                out = Some(Pending::Pin(i));
            } else if del_r.clicked() {
                out = Some(Pending::Delete(i));
            }
        } else if pinned {
            p.circle_filled(pin_c, 4.0, ACCENT);
        }

        if out.is_none() && resp.clicked() {
            out = Some(Pending::Copy(i));
        }
        out
    }
}

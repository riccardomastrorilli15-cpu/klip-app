//! The window: clipboard history and a file/app finder. It runs as its own short-lived
//! process, so all UI and GL memory goes back to the OS the moment it closes.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread,
};

use eframe::egui::{
    self, load::SizedTexture, text::LayoutJob, Align, Align2, Color32, ColorImage, CornerRadius,
    FontData, FontDefinitions, FontFamily, FontId, Frame, Key, Margin, Modifiers, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TextEdit, TextureHandle, TextureOptions, Vec2,
};

use crate::{
    files::{self, Hit, Root},
    image::Thumb,
    proto::{self, Action, Line, WireKind},
};

const INTER: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");
const INTER_SEMI: &[u8] = include_bytes!("../assets/Inter-SemiBold.ttf");
const ROW_H: f32 = 64.0;
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

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

// ---- palette ----------------------------------------------------------------------

struct Pal {
    bg: Color32,
    border: Color32,
    field: Color32,
    text: Color32,
    weak: Color32,
    accent: Color32,
    accent_soft: Color32,
    hover: Color32,
    chip: Color32,
    danger: Color32,
}

fn pal(dark: bool) -> Pal {
    let rgb = Color32::from_rgb;
    if dark {
        let accent = rgb(105, 146, 255);
        Pal {
            bg: rgb(24, 24, 30),
            border: rgb(52, 52, 64),
            field: rgb(33, 33, 42),
            text: rgb(236, 236, 243),
            weak: rgb(142, 142, 160),
            accent,
            accent_soft: accent.gamma_multiply(0.16),
            hover: rgb(32, 32, 41),
            chip: rgb(42, 42, 54),
            danger: rgb(255, 99, 99),
        }
    } else {
        let accent = rgb(52, 104, 240);
        Pal {
            bg: rgb(252, 252, 254),
            border: rgb(222, 222, 232),
            field: rgb(240, 240, 246),
            text: rgb(26, 26, 36),
            weak: rgb(118, 118, 136),
            accent,
            accent_soft: accent.gamma_multiply(0.11),
            hover: rgb(244, 244, 249),
            chip: rgb(231, 231, 240),
            danger: rgb(214, 48, 48),
        }
    }
}

// ---- data -------------------------------------------------------------------------

enum Kind {
    Text {
        preview: String,
        link: bool,
    },
    Image {
        thumb: Thumb,
        tex: Option<TextureHandle>,
        w: u32,
        h: u32,
        bytes: usize,
    },
}

struct Row {
    id: u64,
    pinned: bool,
    kind: Kind,
    hay: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Clipboard,
    Files,
}

enum Pending {
    Copy(usize),
    Pin(usize),
    Delete(usize),
    Open(usize),
    Reveal(usize),
}

struct FileSearch {
    roots: Arc<Vec<Root>>,
    hits: Vec<Hit>,
    rx: Option<Receiver<Hit>>,
    gen: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}

struct Picker {
    rows: Vec<Row>,
    hotkey: Option<String>,
    mode: Mode,
    query: String,
    last_query: String,
    sel: usize,
    scroll_to_sel: bool,
    reselect: Option<u64>,
    had_focus: bool,
    textures_ready: bool,
    files: FileSearch,
}

fn preview(text: &str) -> String {
    let head: String = text.chars().take(600).collect();
    let flat = head.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(220).collect()
}

fn size_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

fn read_input() -> (Vec<Row>, Option<String>) {
    let stdin = io::stdin();
    let mut rows = Vec::new();
    let mut hotkey = None;
    if stdin.is_terminal() {
        return (rows, hotkey);
    }
    for line in stdin.lock().lines().map_while(Result::ok) {
        match proto::decode_line(&line) {
            Some(Line::Hotkey(h)) => hotkey = h,
            Some(Line::Entry(e)) => {
                let (kind, hay) = match e.kind {
                    WireKind::Text(t) => {
                        let p = preview(&t);
                        let link = ["http://", "https://", "www."]
                            .iter()
                            .any(|s| p.starts_with(s));
                        (Kind::Text { preview: p, link }, t.to_lowercase())
                    }
                    WireKind::Image {
                        thumb,
                        width,
                        height,
                        bytes,
                    } => (
                        Kind::Image {
                            thumb,
                            tex: None,
                            w: width,
                            h: height,
                            bytes,
                        },
                        format!("image picture png screenshot {width}x{height}"),
                    ),
                };
                rows.push(Row {
                    id: e.id,
                    pinned: e.pinned,
                    kind,
                    hay,
                });
            }
            None => {}
        }
    }
    rows.sort_by_key(|r| !r.pinned); // stable: pinned first, recency kept inside each group
    (rows, hotkey)
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
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("inter".into(), Arc::new(FontData::from_static(INTER)));
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(INTER_SEMI)),
    );
    let mut prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    prop.insert(0, "inter".into());
    let mut semi = prop.clone();
    semi.insert(0, "inter-semibold".into());
    if fallback {
        if let Some(data) = load_fallback_font() {
            fonts.font_data.insert("fallback".into(), Arc::new(data));
            prop.push("fallback".into());
            semi.push("fallback".into());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("fallback".into());
        }
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(semibold(), semi);
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(0.0, 4.0);
        s.visuals.selection.bg_fill = Color32::from_rgb(70, 120, 255);
    });
}

pub fn run() -> ! {
    let (rows, hotkey) = read_input();
    let fallback = rows
        .iter()
        .any(|r| matches!(&r.kind, Kind::Text { preview, .. } if needs_fallback_font(preview)));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("klip")
            .with_inner_size([580.0, 620.0])
            .with_decorations(false)
            .with_transparent(true)
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
                hotkey,
                mode: Mode::Clipboard,
                query: String::new(),
                last_query: String::new(),
                sel: 0,
                scroll_to_sel: false,
                reselect: None,
                had_focus: false,
                textures_ready: false,
                files: FileSearch {
                    roots: Arc::new(files::roots()),
                    hits: Vec::new(),
                    rx: None,
                    gen: Arc::new(AtomicU64::new(0)),
                    done: Arc::new(AtomicBool::new(true)),
                },
            }))
        }),
    );
    if let Err(e) = result {
        eprintln!("klip picker: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn emit(a: &Action) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{}", a.encode());
    let _ = out.flush();
}

// ---- small widgets ----------------------------------------------------------------

fn keycap(ui: &mut egui::Ui, p: &Pal, label: &str) {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), FontId::new(11.0, semibold()), p.weak);
    let (rect, _) = ui.allocate_exact_size(galley.size() + Vec2::new(12.0, 7.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(6), p.chip);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, p.weak);
}

fn hint(ui: &mut egui::Ui, p: &Pal, key: &str, what: &str) {
    keycap(ui, p, key);
    ui.label(RichText::new(what).size(11.5).color(p.weak));
    ui.add_space(8.0);
}

fn tab(ui: &mut egui::Ui, p: &Pal, label: &str, active: bool) -> egui::Response {
    let color = if active { p.accent } else { p.weak };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), FontId::new(13.0, semibold()), color);
    let (rect, resp) =
        ui.allocate_exact_size(galley.size() + Vec2::new(26.0, 14.0), Sense::click());
    if active {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), p.accent_soft);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), p.hover);
    }
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

fn magnifier(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center() - Vec2::new(1.5, 1.5);
    let s = Stroke::new(1.8_f32, color);
    painter.circle_stroke(c, 6.0, s);
    painter.line_segment([c + Vec2::new(4.5, 4.5), c + Vec2::new(9.0, 9.0)], s);
}

fn empty_state(ui: &mut egui::Ui, p: &Pal, title: &str, sub: &str) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(title)
                .font(FontId::new(17.0, semibold()))
                .color(p.text),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(sub).size(13.0).color(p.weak));
    });
}

fn row_background(ui: &egui::Ui, p: &Pal, rect: Rect, selected: bool, hovered: bool) {
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(14), p.accent_soft);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(14),
            Stroke::new(1.0_f32, p.accent.gamma_multiply(0.45)),
            StrokeKind::Inside,
        );
    } else if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(14), p.hover);
    }
}

fn number_chip(ui: &egui::Ui, p: &Pal, rect: Rect, pos: usize) {
    if pos < 9 {
        let chip = Rect::from_center_size(
            Pos2::new(rect.right() - 26.0, rect.center().y),
            Vec2::new(24.0, 20.0),
        );
        ui.painter()
            .rect_filled(chip, CornerRadius::same(6), p.chip);
        ui.painter().text(
            chip.center(),
            Align2::CENTER_CENTER,
            (pos + 1).to_string(),
            FontId::new(11.0, semibold()),
            p.weak,
        );
    }
}

fn two_lines(
    ui: &egui::Ui,
    text: String,
    size: f32,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple(text, FontId::proportional(size), color, width);
    job.wrap.max_rows = 2;
    // Break inside words only when a token is too long to fit (URLs, hashes).
    job.wrap.break_anywhere = job.text.split(' ').any(|w| w.chars().count() > 34);
    ui.painter().layout_job(job)
}

// ---- app --------------------------------------------------------------------------

impl Picker {
    fn start_search(&mut self, ctx: &egui::Context) {
        let my = self.files.gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.files.hits.clear();
        self.files.rx = None;
        self.files.done = Arc::new(AtomicBool::new(true));
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        self.files.rx = Some(rx);
        self.files.done = done.clone();
        let (gen, roots, ctx) = (
            self.files.gen.clone(),
            self.files.roots.clone(),
            ctx.clone(),
        );
        thread::spawn(move || {
            files::search(
                &roots,
                &q,
                || gen.load(Ordering::SeqCst) != my,
                |h| {
                    let ok = tx.send(h).is_ok();
                    ctx.request_repaint();
                    ok
                },
            );
            done.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    fn switch_mode(&mut self, ctx: &egui::Context, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.query.clear();
        self.last_query.clear();
        self.sel = 0;
        self.scroll_to_sel = true;
        self.start_search(ctx); // empty query: just cancels any running search
    }

    fn visible(&self) -> Vec<usize> {
        match self.mode {
            Mode::Files => (0..self.files.hits.len()).collect(),
            Mode::Clipboard => {
                let q = self.query.trim().to_lowercase();
                (0..self.rows.len())
                    .filter(|&i| q.is_empty() || self.rows[i].hay.contains(&q))
                    .collect()
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, p: Pending) {
        match p {
            Pending::Copy(i) => {
                emit(&Action::Copy(self.rows[i].id));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Pending::Pin(i) => {
                let id = self.rows[i].id;
                let pinned = !self.rows[i].pinned;
                self.rows[i].pinned = pinned;
                emit(&Action::Pin(id, pinned));
                self.rows.sort_by_key(|r| !r.pinned);
                self.reselect = Some(id);
                self.scroll_to_sel = true;
            }
            Pending::Delete(i) => {
                emit(&Action::Delete(self.rows[i].id));
                self.rows.remove(i);
            }
            Pending::Open(i) => {
                if let Some(h) = self.files.hits.get(i) {
                    files::open(&h.path);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Pending::Reveal(i) => {
                if let Some(h) = self.files.hits.get(i) {
                    files::reveal(&h.path);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        ctx.request_repaint();
    }

    fn ensure_textures(&mut self, ctx: &egui::Context) {
        if self.textures_ready {
            return;
        }
        self.textures_ready = true;
        for row in &mut self.rows {
            if let Kind::Image { thumb, tex, .. } = &mut row.kind {
                let img = ColorImage::from_rgba_unmultiplied(
                    [thumb.w as usize, thumb.h as usize],
                    &thumb.rgba,
                );
                *tex =
                    Some(ctx.load_texture(format!("thumb{}", row.id), img, TextureOptions::LINEAR));
            }
        }
    }

    fn clip_row(
        &mut self,
        ui: &mut egui::Ui,
        p: &Pal,
        pos: usize,
        i: usize,
        moved: bool,
    ) -> Option<Pending> {
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
        row_background(ui, p, rect, selected, hovered);
        let row = &self.rows[i];
        let cy = rect.center().y;

        let tile = Rect::from_min_size(
            Pos2::new(rect.left() + 10.0, cy - 22.0),
            Vec2::new(56.0, 44.0),
        );
        ui.painter()
            .rect_filled(tile, CornerRadius::same(10), p.chip);
        let text_left = rect.left() + 78.0;
        let text_w = (rect.width() - 78.0 - 112.0).max(60.0);
        match &row.kind {
            Kind::Text { preview, link } => {
                ui.painter().text(
                    tile.center(),
                    Align2::CENTER_CENTER,
                    if *link { "://" } else { "Aa" },
                    FontId::new(15.0, semibold()),
                    p.accent,
                );
                let g = two_lines(ui, preview.clone(), 14.0, p.text, text_w);
                ui.painter()
                    .galley(Pos2::new(text_left, cy - g.size().y / 2.0), g, p.text);
            }
            Kind::Image {
                thumb,
                tex,
                w,
                h,
                bytes,
            } => {
                if let Some(t) = tex {
                    let s = (56.0 / thumb.w as f32).min(44.0 / thumb.h as f32);
                    let size = Vec2::new(thumb.w as f32 * s, thumb.h as f32 * s);
                    egui::Image::new(SizedTexture::new(t.id(), size))
                        .corner_radius(8.0)
                        .paint_at(ui, Rect::from_center_size(tile.center(), size));
                }
                let title = ui.painter().layout_no_wrap(
                    "Image".into(),
                    FontId::new(14.5, semibold()),
                    p.text,
                );
                let sub = ui.painter().layout_no_wrap(
                    format!("{w} × {h}  ·  PNG {}", size_label(*bytes)),
                    FontId::proportional(12.0),
                    p.weak,
                );
                let top = cy - (title.size().y + 3.0 + sub.size().y) / 2.0;
                let sub_y = top + title.size().y + 3.0;
                ui.painter()
                    .galley(Pos2::new(text_left, top), title, p.text);
                ui.painter()
                    .galley(Pos2::new(text_left, sub_y), sub, p.weak);
            }
        }

        number_chip(ui, p, rect, pos);
        let (pin_c, del_c) = (
            Pos2::new(rect.right() - 90.0, cy),
            Pos2::new(rect.right() - 62.0, cy),
        );
        let (pinned, id) = (row.pinned, row.id);
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
            let pc = if pinned || pin_r.hovered() {
                p.accent
            } else {
                p.weak
            };
            ui.painter().circle(
                pin_c,
                5.0,
                if pinned {
                    p.accent
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::new(1.5_f32, pc),
            );
            let s = Stroke::new(1.6_f32, if del_r.hovered() { p.danger } else { p.weak });
            ui.painter().line_segment(
                [del_c + Vec2::new(-4.5, -4.5), del_c + Vec2::new(4.5, 4.5)],
                s,
            );
            ui.painter().line_segment(
                [del_c + Vec2::new(-4.5, 4.5), del_c + Vec2::new(4.5, -4.5)],
                s,
            );
            if pin_r.clicked() {
                out = Some(Pending::Pin(i));
            } else if del_r.clicked() {
                out = Some(Pending::Delete(i));
            }
        } else if pinned {
            ui.painter().circle_filled(pin_c, 4.0, p.accent);
        }
        if out.is_none() && resp.clicked() {
            out = Some(Pending::Copy(i));
        }
        out
    }

    fn file_row(
        &mut self,
        ui: &mut egui::Ui,
        p: &Pal,
        pos: usize,
        hit: &Hit,
        moved: bool,
    ) -> Option<Pending> {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
        if resp.hovered() && moved {
            self.sel = pos;
        }
        let selected = pos == self.sel;
        if selected && self.scroll_to_sel {
            resp.scroll_to_me(Some(Align::Center));
        }
        row_background(ui, p, rect, selected, ui.rect_contains_pointer(rect));
        let cy = rect.center().y;

        let tile = Rect::from_min_size(
            Pos2::new(rect.left() + 10.0, cy - 22.0),
            Vec2::new(56.0, 44.0),
        );
        ui.painter()
            .rect_filled(tile, CornerRadius::same(10), p.chip);
        let label = if hit.dir {
            "DIR".to_string()
        } else if hit.app {
            "APP".to_string()
        } else {
            match hit.path.extension().and_then(|e| e.to_str()) {
                Some(e) if !e.is_empty() && e.chars().count() <= 4 => e.to_uppercase(),
                _ => "FILE".to_string(),
            }
        };
        ui.painter().text(
            tile.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(12.5, semibold()),
            p.accent,
        );

        let name = hit
            .name
            .strip_suffix(".lnk")
            .or_else(|| hit.name.strip_suffix(".app"))
            .unwrap_or(&hit.name);
        let max_w = rect.width() - 78.0 - 50.0;
        let clip = |s: String, font: FontId, col: Color32| {
            let mut job = LayoutJob::simple_singleline(s, font, col);
            job.wrap.max_width = max_w;
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            ui.painter().layout_job(job)
        };
        let title = clip(name.to_string(), FontId::new(14.5, semibold()), p.text);
        let sub = clip(
            files::parent_label(&hit.path),
            FontId::proportional(12.0),
            p.weak,
        );
        let top = cy - (title.size().y + 3.0 + sub.size().y) / 2.0;
        let sub_y = top + title.size().y + 3.0;
        ui.painter()
            .galley(Pos2::new(rect.left() + 78.0, top), title, p.text);
        ui.painter()
            .galley(Pos2::new(rect.left() + 78.0, sub_y), sub, p.weak);
        number_chip(ui, p, rect, pos);
        resp.clicked().then_some(Pending::Open(pos))
    }
}

impl eframe::App for Picker {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // transparent window, the rounded card is painted by us
    }

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
        self.ensure_textures(ctx);

        // Consume keys before the text field sees them.
        let (esc, down, up, enter, reveal, tab_key, pin, del, quick) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::COMMAND, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Tab),
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
        if tab_key {
            let next = if self.mode == Mode::Clipboard {
                Mode::Files
            } else {
                Mode::Clipboard
            };
            self.switch_mode(ctx, next);
        }

        if let Some(rx) = &self.files.rx {
            while let Ok(h) = rx.try_recv() {
                self.files.hits.push(h);
            }
        }

        let p = pal(ctx.style().visuals.dark_mode);
        let card = ctx.screen_rect().shrink(1.0);
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            card,
            CornerRadius::same(18),
            p.bg,
        );
        let transparent = |l, t, r, b| {
            Frame::NONE.inner_margin(Margin {
                left: l,
                top: t,
                right: r,
                bottom: b,
            })
        };
        let mode = self.mode;

        // ---- top: search + tabs
        egui::TopBottomPanel::top("top")
            .frame(transparent(16, 16, 16, 8))
            .show_separator_line(false)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(p.field)
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::symmetric(14, 11))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                            magnifier(ui.painter(), r, p.weak);
                            let hint_text = if mode == Mode::Clipboard {
                                "Search clipboard"
                            } else {
                                "Search files and apps"
                            };
                            let edit = TextEdit::singleline(&mut self.query)
                                .frame(false)
                                .hint_text(RichText::new(hint_text).color(p.weak))
                                .font(FontId::proportional(16.0))
                                .text_color(p.text)
                                .desired_width(f32::INFINITY);
                            ui.add(edit).request_focus();
                        });
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if tab(ui, &p, "Clipboard", mode == Mode::Clipboard).clicked() {
                        self.switch_mode(ctx, Mode::Clipboard);
                    }
                    if tab(ui, &p, "Files", mode == Mode::Files).clicked() {
                        self.switch_mode(ctx, Mode::Files);
                    }
                });
            });

        if self.query != self.last_query {
            self.last_query = self.query.clone();
            self.sel = 0;
            self.scroll_to_sel = true;
            if self.mode == Mode::Files {
                self.start_search(ctx);
            }
        }
        let visible = self.visible();
        if let Some(id) = self.reselect.take() {
            if let Some(pos) = visible
                .iter()
                .position(|&i| self.rows.get(i).is_some_and(|r| r.id == id))
            {
                self.sel = pos;
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
        let current = visible.get(self.sel).copied();
        match self.mode {
            Mode::Clipboard => {
                if enter {
                    pending = current.map(Pending::Copy);
                }
                if let Some(n) = quick {
                    pending = visible.get(n).copied().map(Pending::Copy);
                }
                if pin {
                    pending = current.map(Pending::Pin);
                }
                if del {
                    pending = current.map(Pending::Delete);
                }
            }
            Mode::Files => {
                if enter {
                    pending = current.map(Pending::Open);
                }
                if reveal {
                    pending = current.map(Pending::Reveal);
                }
                if let Some(n) = quick {
                    pending = visible.get(n).copied().map(Pending::Open);
                }
            }
        }

        // ---- bottom: key hints
        egui::TopBottomPanel::bottom("hints")
            .frame(transparent(16, 8, 16, 14))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match mode {
                        Mode::Clipboard => {
                            hint(ui, &p, "Enter", "copy");
                            hint(ui, &p, &format!("{MOD}+P"), "pin");
                            hint(ui, &p, &format!("{MOD}+D"), "remove");
                            hint(ui, &p, "Tab", "files");
                        }
                        Mode::Files => {
                            hint(ui, &p, "Enter", "open");
                            hint(ui, &p, &format!("{MOD}+Enter"), "show in folder");
                            hint(ui, &p, "Tab", "clipboard");
                        }
                    }
                    hint(ui, &p, "Esc", "close");
                });
            });

        // ---- list
        egui::CentralPanel::default()
            .frame(transparent(10, 0, 10, 0))
            .show(ctx, |ui| {
                if visible.is_empty() {
                    match mode {
                        Mode::Clipboard if self.rows.is_empty() => {
                            let sub = match &self.hotkey {
                                Some(h) => {
                                    format!("klip stays in your tray. Open this window with {h}.")
                                }
                                None => {
                                    "klip stays in your tray. Click its icon to open this window."
                                        .to_string()
                                }
                            };
                            empty_state(ui, &p, "Nothing copied yet", &sub);
                        }
                        Mode::Clipboard => {
                            empty_state(ui, &p, "No matches", "Try a different word.")
                        }
                        Mode::Files if self.query.trim().is_empty() => empty_state(
                            ui,
                            &p,
                            "Find files and apps",
                            "Searches Desktop, Documents, Downloads, Pictures, Music, Videos.",
                        ),
                        Mode::Files if !self.files.done.load(Ordering::SeqCst) => {
                            empty_state(ui, &p, "Searching…", "")
                        }
                        Mode::Files => empty_state(
                            ui,
                            &p,
                            "No matches",
                            "Names only; hidden folders are skipped.",
                        ),
                    }
                    return;
                }
                let moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO);
                let hits = self.files.hits.clone();
                let any_pinned =
                    mode == Mode::Clipboard && visible.iter().any(|&i| self.rows[i].pinned);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut group: Option<bool> = None;
                        for (pos, &i) in visible.iter().enumerate() {
                            let out = match mode {
                                Mode::Clipboard => {
                                    let pinned = self.rows[i].pinned;
                                    if any_pinned && group != Some(pinned) {
                                        ui.add_space(if group.is_some() { 10.0 } else { 2.0 });
                                        ui.horizontal(|ui| {
                                            ui.add_space(10.0);
                                            let t = if pinned { "PINNED" } else { "RECENT" };
                                            ui.label(
                                                RichText::new(t)
                                                    .font(FontId::new(11.0, semibold()))
                                                    .color(p.weak),
                                            );
                                        });
                                        ui.add_space(2.0);
                                    }
                                    group = Some(pinned);
                                    self.clip_row(ui, &p, pos, i, moved)
                                }
                                Mode::Files => self.file_row(ui, &p, pos, &hits[i], moved),
                            };
                            if out.is_some() {
                                pending = out;
                            }
                        }
                    });
                self.scroll_to_sel = false;
            });

        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("border"),
        ))
        .rect_stroke(
            card,
            CornerRadius::same(18),
            Stroke::new(1.0_f32, p.border),
            StrokeKind::Inside,
        );

        if let Some(pd) = pending {
            self.apply(ctx, pd);
        }
    }
}

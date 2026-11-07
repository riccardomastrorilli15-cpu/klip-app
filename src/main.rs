#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![forbid(unsafe_code)]

mod files;
mod history;
mod image;
mod picker;
mod proto;

use std::{
    borrow::Cow,
    collections::hash_map::DefaultHasher,
    fs::{File, OpenOptions, TryLockError},
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use arboard::Clipboard;
use clipboard_master::{CallbackResult, ClipboardHandler, Master};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
};
use tray_icon::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use history::{Content, History, Picture};
use proto::Action;

const POLL: Duration = Duration::from_millis(500);

#[cfg(target_os = "macos")]
const DEFAULT_HOTKEY_LABEL: &str = "Option+Shift+V";
#[cfg(not(target_os = "macos"))]
const DEFAULT_HOTKEY_LABEL: &str = "Alt+Shift+V";

enum Ev {
    Clip(Content),
    Menu(MenuEvent),
    Toggle,
    Picker(Action),
}

struct Ui {
    _tray: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    pause: CheckMenuItem,
    /// Label of the hotkey if it was registered successfully.
    hotkey: Option<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--picker") {
        picker::run();
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("klip {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    daemon(args.iter().any(|a| a == "--background"));
}

fn hash<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

// One daemon per user. Err(()) = another one is running. A lock we cannot create
// is not a reason to refuse to start.
fn acquire_lock() -> Result<Option<File>, ()> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let Ok(f) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("klip-{user}.lock")))
    else {
        return Ok(None);
    };
    match f.try_lock() {
        Err(TryLockError::WouldBlock) => Err(()),
        _ => Ok(Some(f)),
    }
}

// ---- clipboard watcher ------------------------------------------------------------
// Event-driven where the OS allows it (Windows, X11/Wayland); macOS compares the
// pasteboard change counter. Only if that fails do we fall back to a 500 ms poll.
// Text wins over images: copying cells from a spreadsheet gives both, the text is
// what you want.

struct Watch {
    proxy: EventLoopProxy<Ev>,
    paused: Arc<AtomicBool>,
    cb: Clipboard,
    last: u64,
    was_paused: bool,
    polling: bool,
}

enum Raw {
    Text(String),
    Image(arboard::ImageData<'static>),
}

impl Watch {
    fn new(proxy: &EventLoopProxy<Ev>, paused: &Arc<AtomicBool>, polling: bool) -> Option<Self> {
        Some(Self {
            proxy: proxy.clone(),
            paused: paused.clone(),
            cb: Clipboard::new().ok()?,
            last: 0,
            was_paused: false,
            polling,
        })
    }

    fn read(&mut self) -> Option<(u64, Raw)> {
        match self.cb.get_text() {
            Ok(t) => Some((hash(&t), Raw::Text(t))),
            // Not text. Which error that is differs per OS (X11 reports "incorrect type"),
            // so any failure means: try an image.
            Err(_) => {
                let img = self.cb.get_image().ok()?;
                let key = hash(&(img.width, img.height, &*img.bytes));
                Some((key, Raw::Image(img)))
            }
        }
    }

    // false = the daemon is gone, stop watching.
    fn check(&mut self) -> bool {
        if self.paused.load(Ordering::Relaxed) {
            self.was_paused = true; // never read the clipboard while paused
            return true;
        }
        let Some((key, raw)) = self.read() else {
            return true;
        };
        if self.was_paused {
            self.was_paused = false;
            if self.polling {
                self.last = key; // what was copied during the pause stays unrecorded
                return true;
            }
        }
        if key == self.last {
            return true;
        }
        self.last = key;
        let content = match raw {
            Raw::Text(t) => Content::Text(t),
            Raw::Image(img) => match picture(img, key) {
                Some(p) => Content::Image(p),
                None => return true,
            },
        };
        self.proxy.send_event(Ev::Clip(content)).is_ok()
    }
}

fn picture(img: arboard::ImageData<'_>, key: u64) -> Option<Picture> {
    let (w, h) = (
        u32::try_from(img.width).ok()?,
        u32::try_from(img.height).ok()?,
    );
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > image::MAX_PIXELS {
        return None;
    }
    Some(Picture {
        png: image::encode_png(w, h, &img.bytes)?,
        width: w,
        height: h,
        key,
        thumb: image::thumbnail(w, h, &img.bytes),
    })
}

impl ClipboardHandler for Watch {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        if self.check() {
            CallbackResult::Next
        } else {
            CallbackResult::Stop
        }
    }
}

fn spawn_watcher(proxy: EventLoopProxy<Ev>, paused: Arc<AtomicBool>) {
    thread::spawn(move || {
        if let Some(w) = Watch::new(&proxy, &paused, false) {
            if let Ok(mut master) = Master::new(w) {
                if master.run().is_ok() {
                    return;
                }
            }
        }
        let Some(mut w) = Watch::new(&proxy, &paused, true) else {
            return;
        };
        loop {
            thread::sleep(POLL);
            if !w.check() {
                return;
            }
        }
    });
}

// ---- picker process ---------------------------------------------------------------

type Picker = Arc<Mutex<Child>>;

fn spawn_picker(
    history: &History,
    hotkey: Option<&str>,
    proxy: &EventLoopProxy<Ev>,
) -> Option<Picker> {
    let mut child = Command::new(std::env::current_exe().ok()?)
        .arg("--picker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let payload = proto::encode_header(hotkey) + &proto::encode_entries(history.items());
    thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes()); // dropping stdin sends EOF
    });
    let child = Arc::new(Mutex::new(child));
    let (proxy, reaper) = (proxy.clone(), child.clone());
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(a) = Action::decode(&line) {
                if proxy.send_event(Ev::Picker(a)).is_err() {
                    break;
                }
            }
        }
        // stdout closed: the picker is exiting. Reap it so no zombie lingers.
        for _ in 0..60 {
            let done = match reaper.lock() {
                Ok(mut c) => !matches!(c.try_wait(), Ok(None)),
                Err(_) => true,
            };
            if done {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    Some(child)
}

// Second press closes an open picker.
fn toggle_picker(
    picker: &mut Option<Picker>,
    history: &History,
    hotkey: Option<&str>,
    proxy: &EventLoopProxy<Ev>,
) {
    if let Some(c) = picker.take() {
        if let Ok(mut c) = c.lock() {
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
                let _ = c.wait();
                return;
            }
        }
    }
    *picker = spawn_picker(history, hotkey, proxy);
}

// ---- tray -------------------------------------------------------------------------

// 32x32 glyph drawn at startup, so there are no asset files. macOS wants a monochrome
// template image (alpha only); Windows and Linux get a blue badge that reads on any panel.
fn icon() -> Icon {
    const N: u32 = 32;
    let mut px = vec![0u8; (N * N * 4) as usize];
    let mut put = |x: u32, y: u32, c: [u8; 4]| {
        let i = ((y * N + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&c);
    };
    if cfg!(target_os = "macos") {
        for y in 5..28 {
            for x in 6..26 {
                let edge = !(8..24).contains(&x) || !(7..26).contains(&y);
                let line = (10..22).contains(&x) && matches!(y, 13 | 18 | 23);
                if edge || line {
                    put(x, y, [0, 0, 0, 255]);
                }
            }
        }
        for y in 2..8 {
            for x in 11..21 {
                put(x, y, [0, 0, 0, 255]);
            }
        }
    } else {
        let blue = [70, 120, 255, 255];
        let white = [255, 255, 255, 255];
        for y in 0..N {
            for x in 0..N {
                // rounded square: cut the four corners
                let (dx, dy) = (x.min(N - 1 - x), y.min(N - 1 - y));
                if dx + dy >= 4 || (dx >= 2 && dy >= 2) {
                    put(x, y, blue);
                }
            }
        }
        for y in 9..27 {
            for x in 9..23 {
                let edge = !(11..21).contains(&x) || !(11..25).contains(&y);
                let line = (13..19).contains(&x) && matches!(y, 15 | 19 | 23);
                if edge || line {
                    put(x, y, white);
                }
            }
        }
        for y in 6..11 {
            for x in 12..20 {
                put(x, y, white);
            }
        }
    }
    Icon::from_rgba(px, N, N).expect("icon")
}

fn hotkey() -> (HotKey, String) {
    if let Ok(s) = std::env::var("KLIP_HOTKEY") {
        if let Ok(k) = s.parse::<HotKey>() {
            return (k, s);
        }
    }
    (
        HotKey::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::KeyV),
        DEFAULT_HOTKEY_LABEL.to_string(),
    )
}

fn build_ui(hotkey: HotKey, label: &str) -> Option<Ui> {
    // Global hotkeys can be unavailable (Wayland, key already taken); the tray still works.
    let manager = GlobalHotKeyManager::new().ok();
    let registered = manager.as_ref().is_some_and(|m| m.register(hotkey).is_ok());
    let open_text = if registered {
        format!("open klip ({label})")
    } else {
        "open klip".into()
    };

    let open = MenuItem::with_id("open", open_text, true, None);
    let pause = CheckMenuItem::with_id("pause", "pause capture", true, false, None);
    let clear = MenuItem::with_id("clear", "clear unpinned", true, None);
    let quit = MenuItem::with_id("quit", "quit klip", true, None);
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    let items: [&dyn IsMenuItem; 6] = [&open, &sep1, &pause, &clear, &sep2, &quit];
    let menu = Menu::new();
    menu.append_items(&items).ok()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false) // left click opens the window, right click the menu
        .with_tooltip("klip")
        .with_icon(icon())
        .with_icon_as_template(true)
        .build()
        .ok()?;
    Some(Ui {
        _tray: tray,
        _hotkeys: manager,
        pause,
        hotkey: registered.then(|| label.to_string()),
    })
}

fn daemon(background: bool) {
    let Ok(_lock) = acquire_lock() else { return };

    // GTK only draws the tray menu here. Without this it initialises OpenGL at startup,
    // which on Mesa pulled ~50 MB (LLVM) into the resident set. No threads exist yet.
    #[cfg(target_os = "linux")]
    std::env::set_var("GDK_GL", "disable");

    #[allow(unused_mut)] // mutated on macOS only
    let mut event_loop = EventLoopBuilder::<Ev>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory); // no dock icon
    }

    let proxy = event_loop.create_proxy();
    let paused = Arc::new(AtomicBool::new(false));

    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(Ev::Menu(e));
        }));
    }
    {
        let p = proxy.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state() == HotKeyState::Pressed {
                let _ = p.send_event(Ev::Toggle);
            }
        }));
    }
    {
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                let _ = p.send_event(Ev::Toggle);
            }
        }));
    }
    spawn_watcher(proxy.clone(), paused.clone());

    // Kept alive for the whole run: on X11 the selection owner must outlive the copy.
    let mut cb = Clipboard::new().ok();
    let mut history = History::default();
    let mut picker: Option<Picker> = None;
    let mut ui: Option<Ui> = None;
    let (key, label) = hotkey();

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let hk = |ui: &Option<Ui>| ui.as_ref().and_then(|u| u.hotkey.clone());
        match event {
            // macOS needs tray and hotkeys created once the loop is running.
            Event::NewEvents(StartCause::Init) => {
                ui = build_ui(key, &label);
                if ui.is_none() {
                    *flow = ControlFlow::Exit;
                } else if !background {
                    // Show the window once at launch so a first run is never "nothing happened".
                    picker = spawn_picker(&history, hk(&ui).as_deref(), &proxy);
                }
            }
            Event::UserEvent(Ev::Clip(c)) => {
                history.push(c);
            }
            Event::UserEvent(Ev::Toggle) => {
                toggle_picker(&mut picker, &history, hk(&ui).as_deref(), &proxy)
            }
            Event::UserEvent(Ev::Picker(a)) => match a {
                Action::Copy(id) => {
                    if let Some(cb) = cb.as_mut() {
                        match history.content(id) {
                            Some(Content::Text(t)) => {
                                let _ = cb.set_text(t.clone());
                            }
                            Some(Content::Image(p)) => {
                                if let Some((w, h, bytes)) = image::decode_png(&p.png) {
                                    let _ = cb.set_image(arboard::ImageData {
                                        width: w as usize,
                                        height: h as usize,
                                        bytes: Cow::Owned(bytes),
                                    });
                                }
                            }
                            None => {}
                        }
                    }
                }
                Action::Pin(id, p) => history.set_pinned(id, p),
                Action::Delete(id) => history.delete(id),
            },
            Event::UserEvent(Ev::Menu(e)) => match e.id.0.as_str() {
                "open" => toggle_picker(&mut picker, &history, hk(&ui).as_deref(), &proxy),
                "pause" => {
                    if let Some(ui) = &ui {
                        paused.store(ui.pause.is_checked(), Ordering::Relaxed);
                    }
                }
                "clear" => history.clear_unpinned(),
                "quit" => {
                    if let Some(c) = picker.take() {
                        if let Ok(mut c) = c.lock() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                    }
                    *flow = ControlFlow::Exit;
                }
                _ => {}
            },
            _ => {}
        }
    });
}

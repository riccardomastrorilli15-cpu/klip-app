#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![forbid(unsafe_code)]

mod history;
mod picker;
mod proto;

use std::{
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

use history::{History, MAX_ENTRY_BYTES};
use proto::Action;

const POLL: Duration = Duration::from_millis(500);

#[cfg(target_os = "macos")]
const DEFAULT_HOTKEY_LABEL: &str = "Option+Shift+V";
#[cfg(not(target_os = "macos"))]
const DEFAULT_HOTKEY_LABEL: &str = "Alt+Shift+V";

enum Ev {
    Clip(String),
    Menu(MenuEvent),
    Toggle,
    Picker(Action),
}

struct Ui {
    _tray: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    pause: CheckMenuItem,
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--picker") => picker::run(),
        Some("--version") | Some("-V") => println!("klip {}", env!("CARGO_PKG_VERSION")),
        _ => daemon(),
    }
}

fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
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

struct Watch {
    proxy: EventLoopProxy<Ev>,
    paused: Arc<AtomicBool>,
    cb: Clipboard,
    last: u64,
    was_paused: bool,
    polling: bool,
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

    // false = the daemon is gone, stop watching.
    fn check(&mut self) -> bool {
        if self.paused.load(Ordering::Relaxed) {
            self.was_paused = true; // never read the clipboard while paused
            return true;
        }
        let Ok(t) = self.cb.get_text() else {
            return true;
        };
        let h = hash(&t);
        if self.was_paused {
            self.was_paused = false;
            if self.polling {
                self.last = h; // what was copied during the pause stays unrecorded
                return true;
            }
        }
        if h == self.last {
            return true;
        }
        self.last = h;
        if t.trim().is_empty() || t.len() > MAX_ENTRY_BYTES {
            return true;
        }
        self.proxy.send_event(Ev::Clip(t)).is_ok()
    }
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

fn spawn_picker(history: &History, proxy: &EventLoopProxy<Ev>) -> Option<Picker> {
    let mut child = Command::new(std::env::current_exe().ok()?)
        .arg("--picker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let payload = proto::encode_entries(history.items());
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
fn toggle_picker(picker: &mut Option<Picker>, history: &History, proxy: &EventLoopProxy<Ev>) {
    if let Some(c) = picker.take() {
        if let Ok(mut c) = c.lock() {
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
                let _ = c.wait();
                return;
            }
        }
    }
    *picker = spawn_picker(history, proxy);
}

// ---- tray -------------------------------------------------------------------------

// 32x32 clipboard glyph drawn at startup, so there are no asset files.
// macOS uses it as a template image (alpha only); elsewhere it is mid-grey.
fn icon() -> Icon {
    const N: u32 = 32;
    let mut px = vec![0u8; (N * N * 4) as usize];
    let mut put = |x: u32, y: u32| {
        let i = ((y * N + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&[140, 140, 140, 255]);
    };
    for y in 5..28 {
        for x in 6..26 {
            let edge = !(8..24).contains(&x) || !(7..26).contains(&y);
            let line = (10..22).contains(&x) && matches!(y, 13 | 18 | 23);
            if edge || line {
                put(x, y);
            }
        }
    }
    for y in 2..8 {
        for x in 11..21 {
            put(x, y);
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
        format!("open history ({label})")
    } else {
        "open history".into()
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
        .with_menu_on_left_click(false) // left click opens the picker, right click the menu
        .with_tooltip("klip")
        .with_icon(icon())
        .with_icon_as_template(true)
        .build()
        .ok()?;
    Some(Ui {
        _tray: tray,
        _hotkeys: manager,
        pause,
    })
}

fn daemon() {
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
        match event {
            // macOS needs tray and hotkeys created once the loop is running.
            Event::NewEvents(StartCause::Init) => {
                ui = build_ui(key, &label);
                if ui.is_none() {
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(Ev::Clip(t)) => {
                history.push(t);
            }
            Event::UserEvent(Ev::Toggle) => toggle_picker(&mut picker, &history, &proxy),
            Event::UserEvent(Ev::Picker(a)) => match a {
                Action::Copy(id) => {
                    if let (Some(t), Some(cb)) = (history.text(id), cb.as_mut()) {
                        let _ = cb.set_text(t.to_owned());
                    }
                }
                Action::Pin(id, p) => history.set_pinned(id, p),
                Action::Delete(id) => history.delete(id),
            },
            Event::UserEvent(Ev::Menu(e)) => match e.id.0.as_str() {
                "open" => toggle_picker(&mut picker, &history, &proxy),
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

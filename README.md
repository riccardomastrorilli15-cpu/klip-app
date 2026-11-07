# klip

One keyboard-first window for your clipboard history (text, links, emoji, images) and for finding files and apps.
macOS, Windows, Linux. One native binary, no webview.

Press **Alt+Shift+V** (Option+Shift+V on macOS), type, **Enter**. That is the whole idea.

## Using it

klip starts in the tray / menu bar and opens its window once at launch, so a first run is never "nothing happened".
After that, open it with the hotkey or a left click on the tray icon.

**Clipboard tab**

| Key | Action |
|---|---|
| type | filter history |
| Up / Down, Enter | move, copy and close |
| Ctrl/Cmd + 1-9 | copy the nth row |
| Ctrl/Cmd + P | pin / unpin (pins stay on top and survive eviction, max 20) |
| Ctrl/Cmd + D | remove entry |
| Tab | switch to Files |
| Esc, click away, hotkey again | close |

Text, links, emoji and images are recorded. Pressing Enter puts the item back on the clipboard; paste it yourself.

**Files tab**: type part of a name to find files, folders and apps under Desktop, Documents, Downloads, Pictures, Music,
Videos (plus the Start Menu on Windows and `/Applications` on macOS). Enter opens it with your system default,
Ctrl/Cmd + Enter shows it in its folder. Names only, hidden folders are skipped, nothing is indexed or stored:
the search runs while the window is open and stops when it closes.

Tray right click: pause capture, clear unpinned, quit.
Start at login: add `klip --background` to your login items so it starts without opening the window.
Change the hotkey with `KLIP_HOTKEY`, for example `KLIP_HOTKEY="control+alt+KeyK"`.

## How it keeps resource use low

klip is two processes built from the same binary.

- **daemon** (always running): tray icon, hotkey, clipboard watcher, history. No window, no GPU context.
  It is event-driven where the OS allows it (Windows, X11/Wayland clipboard events); macOS compares the pasteboard
  change counter every 500 ms. A 500 ms poll is used only if the OS event API is unavailable.
- **window** (exists while you look at it): spawned on the hotkey, exits when you close it. All window, font and
  OpenGL memory goes back to the OS at that moment. It repaints only on input.

Images are stored as PNG, never as raw pixels, and the window only receives a 160x100 thumbnail.
Limits: 50 entries, text up to 64 KB, at most 8 images (8 MB each, 32 MB total, up to 12 megapixels).

Measured on Linux (Ubuntu 24.04, X11, GTK tray), release build: the daemon uses about 29 MB RSS (25 MB PSS) with text
only and about 38 MB after copying one 1600x900 image (the allocator keeps some freed memory). The binary is 7.2 MB.
macOS and Windows are not measured yet.

## What it does not do

- No network access of any kind, no telemetry, no update checker.
- No disk writes (apart from an empty lock file so only one daemon runs per user). History lives in RAM only.
- No keystroke injection, no auto-paste, no accessibility or screen-recording permission, no autostart by itself.
- The Files tab only lists directory names. The only program klip ever launches is your system opener
  (`explorer`, `open` or `xdg-open`), on the one file you pick.
- `#![forbid(unsafe_code)]` in this crate. Dependencies are pinned by `Cargo.lock` and checked weekly with `cargo audit`.

Unsigned binaries can still trigger SmartScreen / Gatekeeper / heuristic AV warnings. That is about the missing
signature, not behaviour. Every release has `SHA256SUMS.txt` and a GitHub build attestation:

```sh
gh attestation verify klip-linux-x64.tar.gz --repo <owner>/klip-app
```

## Install

Download the archive for your OS from [Releases](../../releases).

- **macOS**: unzip, move `klip.app` to `/Applications`. Ad-hoc signed, not notarized: first launch via right click, Open.
- **Windows**: unzip, run `klip.exe`. On Windows 11 new tray icons can hide behind the `^` arrow near the clock.
- **Linux**: `tar xzf klip-linux-x64.tar.gz && ./klip`.
  Debian/Ubuntu runtime packages: `libgtk-3-0 libxdo3 libayatana-appindicator3-1 libgl1`.

## Build

```sh
cargo build --release
cargo test
```

Linux build packages: `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev pkg-config`. Rust 1.89 or newer.
Releases are built by GitHub Actions when a `v*` tag is pushed.

## Known limits

- **Password managers.** klip cannot see "concealed clipboard" flags, so it records whatever a manager puts on the
  clipboard until something replaces it. Use *pause capture* when handling secrets.
- **Wayland.** Global hotkeys do not exist there; use the tray icon. GNOME blocks background clipboard reads, so
  capture will not work on GNOME/Wayland. X11, sway and Hyprland are fine. GNOME also needs the AppIndicator extension.
- **Emoji** are recorded and shown, but drawn as monochrome glyphs. **Fonts:** the window bundles Inter and loads a system
  CJK font only when an entry needs it. Arabic and Indic scripts are drawn without shaping.
- **Graphics.** The window uses OpenGL 2+ and a transparent surface. It will not start on a bare VM or RDP session without
  GL, and on Linux without a compositor the rounded corners show as black. If it never appears, run
  `klip --picker < /dev/null` in a terminal to see why.
- Text wins over images: copying spreadsheet cells records the text. Files copied in the file manager are not recorded.
- The Files tab does not use an index, so very large folders may show results gradually or not reach deep files.

## Licenses

klip is MIT. The window embeds the Inter font (SIL Open Font License 1.1, see `assets/Inter-OFL.txt`).

# klip

Clipboard history with a keyboard-first picker. macOS, Windows, Linux. One native binary.

- Press **Alt+Shift+V** (Option+Shift+V on macOS) to open the picker, type to search, **Enter** to copy.
- Text only, kept in memory, never written to disk, no network code.

## Using it

| Key | Action |
|---|---|
| type | filter history |
| Up / Down, Enter | move, copy and close |
| Ctrl/Cmd + 1-9 | copy the nth row |
| Ctrl/Cmd + P | pin / unpin (pins stay on top and survive eviction, max 20) |
| Ctrl/Cmd + D | remove entry |
| Esc, click away, hotkey again | close |

The tray / menu bar icon: left click opens the picker, right click gives the menu (pause capture, clear unpinned, quit).
History holds 50 entries; anything over 64 KB is ignored. Pressing Enter puts the text back on the clipboard; paste it yourself.
Change the hotkey with `KLIP_HOTKEY`, for example `KLIP_HOTKEY="control+alt+KeyK"`.

## How it keeps resource use low

klip is two processes built from the same binary.

- **daemon** (always running): tray icon, hotkey, clipboard watcher, history. No window, no GPU context.
  Idle cost is zero wakeups on Windows and X11 (clipboard change events), and one cheap change-counter check
  every 500 ms on macOS. A 500 ms poll is used only if the OS event API is unavailable.
- **picker** (exists while you look at it): spawned on the hotkey, exits when you close it. All window,
  font and OpenGL memory goes back to the OS at that moment. The picker repaints only on input.

Measured on Linux (Ubuntu 24.04, X11, GTK tray) with the release build: the daemon sits at about 29 MB RSS
(25 MB PSS, 5 MB private; most of it is shared GTK libraries) and uses roughly 0.1% CPU while idle. The binary is
6.2 MB. During that test the picker used about 110 MB while open, but that was software OpenGL in a virtual
display, so treat it as an upper bound. macOS and Windows are not measured yet.

## What it does not do

- No network access of any kind, no telemetry, no update checker.
- No disk writes (apart from an empty lock file so only one daemon runs per user).
- No keystroke injection, no auto-paste, no accessibility or screen-recording permission.
- No autostart. Add it yourself: login items on macOS, Startup folder on Windows, your DE's autostart on Linux.
- `#![forbid(unsafe_code)]` in this crate. Dependencies are pinned by `Cargo.lock` and checked weekly with `cargo audit`.

Unsigned binaries can still trigger SmartScreen / Gatekeeper / heuristic AV warnings. That is about the missing
signature, not behaviour. Every release has `SHA256SUMS.txt` and a GitHub build attestation:

```sh
gh attestation verify klip-linux-x64.tar.gz --repo <owner>/klip
```

## Install

Download the archive for your OS from [Releases](../../releases).

- **macOS**: unzip, move `klip.app` to `/Applications`. Ad-hoc signed, not notarized: first launch via right click, Open.
- **Windows**: unzip, run `klip.exe`.
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
  clipboard until something replaces it. Use *pause capture* when handling secrets, or clear after.
- **Wayland.** Global hotkeys do not exist there; use the tray menu. GNOME blocks background clipboard reads, so
  capture will not work on GNOME/Wayland. X11, sway and Hyprland are fine. GNOME also needs the AppIndicator extension for the tray.
- **Fonts.** The picker bundles a Latin/Cyrillic/Greek font and loads a system CJK font only when an entry needs it.
  Arabic and Indic scripts are drawn without shaping.
- **Graphics.** The picker uses OpenGL 2+. It will not start on a bare VM or RDP session without GL.
  If the picker never appears, run `klip --picker < /dev/null` in a terminal to see why.
- Text only. Images and files are not recorded.

## License

MIT

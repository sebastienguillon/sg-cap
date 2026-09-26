# SgCap

macOS-style screen capture for Windows 10 and 11, written in Rust with no runtime
dependencies.

Press the hotkey, drag a rectangle, and the capture is saved as PNG right away. A
thumbnail slides into the bottom-right corner of the screen; you can drag it into
any application that accepts files or images (browser upload forms, mail, chat,
Explorer, Office), click it to open the file, or right-click it for more options.
After a few seconds it fades out on its own.

## Usage

| Action | Default |
|---|---|
| Capture a region | `Ctrl+Shift+4` |
| Capture the screen under the cursor | `Ctrl+Shift+3` |
| Cancel a region capture | `Esc` or right-click |
| Move the selection while dragging | hold `Space` |
| Thumbnail: drag into another app | left-drag |
| Thumbnail: open the file | click |
| Thumbnail: copy image, show in folder, delete, dismiss | right-click |
| Thumbnail: keep it on screen | hover it |

SgCap lives in the notification area. Its icon menu offers the two captures,
the settings window, the save folder and Quit. Left-clicking the icon starts a
region capture.

Files are named `Screenshot 2026-09-26 at 14.32.11.png` and saved to the Desktop
by default.

## Settings

Open from the tray menu. Both hotkeys, the thumbnail delay, the save folder,
an optional clipboard copy and "start with Windows" can be changed. Settings are
stored in `%APPDATA%\SgCap\settings.json`; a log is written next to it as
`sgcap.log`.

If a hotkey is already taken by another program, SgCap shows a notification at
startup and opens the settings window so you can pick another one. On Windows 11
the PrintScreen key is bound to Snipping Tool by default; disable that in
Settings > Accessibility > Keyboard if you want to use it here.

## Building

Requires the Rust toolchain (stable, MSVC target) and the Visual Studio Build
Tools C++ workload.

```
cargo build --release
```

The result is a single file, `target\release\sgcap.exe`. Copy it anywhere and
run it; enable "Start with Windows" in the settings to keep it resident.

## How it works

- A resident process owns the global hotkeys, so the selection overlay appears
  within a few tens of milliseconds.
- The screen is grabbed with DXGI Desktop Duplication (GDI fallback), shown frozen
  and dimmed on a full-desktop overlay; the selection is cropped from the bright copy.
- PNG encoding and saving happen on a worker thread while the thumbnail is shown.
- The thumbnail is a layered window; dragging it hands the receiving application a
  shell data object with the file, a bitmap and a PNG stream.

## Known limitations (MVP)

- HDR monitors: colours may look washed out (SDR readback).
- Exclusive-fullscreen games cannot be overlaid.
- Drops into applications running as administrator are blocked by Windows.
- Window-picking capture mode is not implemented yet.

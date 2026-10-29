# WinOrbit

WinOrbit is a fast, Windows-focused app switcher that sharpens both `Alt+\`` and `Alt+Tab`.

It keeps the lightweight same-app switching flow, adds an optional card-based `Alt+Tab` overlay, and supports inline cycling through windows of the selected app before you commit on `Alt` release.

## Features

- `Alt+\`` cycles windows for the current app.
- Optional `Alt+Tab` app switcher with live `preview` cards by default and `icon_only` fallback mode.
- Inline same-app cycling inside the `Alt+Tab` overlay with deferred activation.
- Adaptive multi-row layout with scalable cards.
- Dot indicators for multi-window apps.
- Configurable backdrop opacity, backdrop color, representative window policy, and icon overrides.
- Works in standard-user mode and can optionally handle elevated windows when run as administrator.

## Installation

1. Download the latest release archive from this repository's Releases page.
2. Extract the archive to a folder and keep `winorbit.exe`, `winorbit.ini`, and `LICENSE` together.
3. Edit `winorbit.ini` if you want to change the default hotkeys or overlay behavior.
4. Launch `winorbit.exe`.

An optional PowerShell installer script is included as [install.ps1](install.ps1). It defaults to `dfk789/winorbit`, and you can override that with `-Repo owner/name` if you fork it. The installer now lays down the bundled default `winorbit.ini` on first install and preserves an existing config on upgrades.

## Configuration

WinOrbit reads `winorbit.ini` from the same directory as `winorbit.exe`.

```ini
# Whether to show trayicon, yes/no
trayicon = yes

[switch-windows]

# Hotkey to switch windows
hotkey = alt+`

# List of hotkey conflict apps
# e.g. game1.exe,game2.exe
blacklist =

# Ignore minimal windows
ignore_minimal = no

# Switch to windows from only the current virtual desktops instead of all desktops.
# Defaults to match the Alt-Tab behavior of Windows:
# Settings > System > Multitasking > Virtual Desktops
only_current_desktop = auto

[switch-apps]

# Whether to enable switching apps
enable = no

# Hotkey to switch apps
hotkey = alt+tab

# Ignore minimal windows
ignore_minimal = no

# How to render app-switch entries.
# preview = default; show live preview cards when DWM thumbnails are available;
#   entries that cannot provide a preview fall back to icons
# icon_only = fallback mode when preview cards are incompatible with an app
#   or when you want the lightest overlay
render_mode = preview

# Whether to show per-window dot indicators for apps with more than one window.
show_window_count = no

# Scale the overlay card size as a percentage (50-200, default 100).
# Higher values produce larger cards and previews.
overlay_scale = 100

# Overall overlay background opacity as a percentage (0-100, default 100).
# Lower values make only the container more transparent.
backdrop_opacity = 100

# Optional hex color for the overlay background (e.g. #2d2d2d).
# When not set the overlay follows the current Windows light/dark theme.
backdrop_color =

# Which app window to use as the representative target.
# legacy_minimized_fallback = if the first grouped window is minimized,
#   fall back to the last window in that app group.
# first_window = always use the first window in the existing app-group order.
representative_window = legacy_minimized_fallback

# List of override icons, syntax: app1.exe=icon1.ico,app2.exe=icon2.png.
# The icon path can be a full path or a relative path to the app's directory.
# The icon format can be ico or png.
override_icons =

# Switch to apps from only the current virtual desktops instead of all desktops.
# Defaults to match the Alt-Tab behavior of Windows:
# Settings > System > Multitasking > Virtual Desktops
only_current_desktop = auto

[overview]

# Experimental, off by default. When enabled, WinOrbit owns a bare tap of the
# Windows key: Start does not open and the overview action runs (this build
# only logs it; the overview itself is not built yet). Every Windows-key
# shortcut (Win+E, Win+L, Win+Shift+S, ...) stays native, and Ctrl+Esc still
# opens Start. Restart WinOrbit after changing this section. A malformed value
# disables the feature and keeps the native Windows key.
enable = no

# A second bare tap starting within this many milliseconds of the first is
# reported to the overview as a second tap (Start stays closed for both).
# 0 disables second taps; otherwise 100-1000, default 250.
double_tap_ms = 250

[log]

# Log level can be one of off,error,warn,info,debug,trace.
level = info

# Log file path.
# e.g.
#   winorbit.log (located in the same directory as winorbit.exe)
#   C:\Users\you\AppData\Local\Temp\winorbit.log
path =
```

## Usage Notes

- Hold `Alt` and tap `Tab` to cycle apps in the overlay.
- While the `Alt+Tab` overlay is open, press `Alt+\`` to cycle windows inside the selected app without foregrounding each candidate window immediately.
- Release `Alt` to activate the currently selected app window.
- Press `Esc` while the overlay is open to cancel. `Alt+Esc` without an open overlay is left to Windows.
- WinOrbit ignores key events that Windows marks as injected, and passes them on to Windows unchanged. Software input sent through mechanisms such as `SendInput` is reported this way, so for example an `Alt+Tab` produced by a key remapper such as PowerToys Keyboard Manager opens the native Windows switcher rather than WinOrbit. Other tools (macro tools, on-screen keyboards, remote-control or KVM software, tablet drivers) are affected only if the keys they produce carry that mark. All other key events are treated as ordinary keyboard input.
- Bare Windows key (`[overview] enable = yes`, experimental, off by default):
  - A clean tap of either Windows key (pressed and released with no other key involved) no longer opens Start. It is reserved for the upcoming overview; this build only logs it.
  - A quick second tap (within `double_tap_ms`) is owned too and reported to the overview as a second tap; Start stays closed. `Ctrl+Esc` always opens Start.
  - While Start (or Search) is visibly open, a tap is left to Windows, so it closes Start as usual. A hidden or cloaked Start host in the foreground (Windows does this briefly during `Win+Left`) does not count.
  - Anything else involving the Windows key is left to Windows unchanged: `Win+E`, `Win+L`, `Win+Shift+S`, `Win+number`, `Win+arrows`, holding `Shift` and then tapping Win, pressing both Windows keys together, and so on. WinOrbit never swallows a Windows-key event; on a clean tap it only injects the unassigned key `vkE8` just before the release, which is what stops Start from opening.
  - If that injection fails, Start opens as usual. If the overview action cannot be delivered, WinOrbit reopens Start itself.
  - While an elevated window is focused and WinOrbit is not elevated, Windows does not send WinOrbit's hook any keys, so the Windows key keeps its native behavior there.
  - When WinOrbit starts, and after the lock screen, a UAC prompt, Ctrl+Alt+Del or focus moving to an elevated window, WinOrbit cannot know whether a key is still held, so it owns no tap until it has seen a key released with nothing else held (normal typing does this). Until then the Windows key is native and Start opens; often that is just the first tap after unlocking.
  - To turn it off, set `enable = no` and restart, or exit WinOrbit from the tray. Changes to `[overview]` need a restart; a malformed value disables the feature and is logged.
- `preview` is the shipped default. `icon_only` remains the safest fallback mode when previews are incompatible with a specific app.

## Credits

WinOrbit is a substantial MIT-licensed continuation of the original
[`sigoden/window-switcher`](https://github.com/sigoden/window-switcher)
project by [sigoden](https://github.com/sigoden). This codebase keeps that
upstream lineage visible while carrying its own Windows-first UI, switching,
and overlay changes forward under the WinOrbit name.

## Building

Build on Windows with the MSVC toolchain:

```bash
cargo build --locked --release --target x86_64-pc-windows-msvc
```

Cross-compile from Linux:

```bash
cargo build --locked --release --target x86_64-pc-windows-gnu
```

The resulting executable is:

```text
target/<target-triple>/release/winorbit.exe
```

## License

WinOrbit is released under the MIT License. See [LICENSE](LICENSE).

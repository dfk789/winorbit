# Window Switcher

Window-Switcher offers hotkeys for quickly switching windows on Windows OS:

1. ```Alt+`(Backtick)```: switch between windows of the same app.

![switch-windows](https://github.com/sigoden/window-switcher/assets/4012553/06d387ce-31fd-450b-adf3-01bfcfc4bce3)

2. ```Alt+Tab```: switch between apps. (disabled by default)

![switch-apps](https://github.com/sigoden/window-switcher/assets/4012553/0c74a7ca-3a48-4458-8d2d-b40dc041f067)

**💡 Hold down the `Alt` key and tap the ``` `(Backtick)/Tab ``` key to cycle through windows/apps, Press ```Alt + `(Backtick)/Tab``` and release both keys to switch to the last active window/app.**

**💡 While the `Alt+Tab` overlay is open, press `` Alt+` `` to cycle through windows of the selected app without dismissing the overlay. Release `Alt` to activate the chosen window.**

## Installation

1. **Download:** Visit the [Github Release](https://github.com/sigoden/windows-switcher/releases) and download the `windows-switcher.zip` file.
2. **Extract:** Unzip the downloaded file and extract the `window-switcher.exe` to your preferred location.
3. **Launch:** `window-switcher.exe` is a standalone executable, no installation is required, just double-click the file to run it.

For the tech-savvy, here's a one-liner to automate the installation:
```ps1
iwr -useb https://raw.githubusercontent.com/sigoden/window-switcher/main/install.ps1 | iex
```

## Configuration

Window-Switcher offers various customization options to tailor its behavior to your preferences. You can define custom keyboard shortcuts, enable or disable specific features, and fine-tune settings through a configuration file.

To personalize Window-Switcher, you'll need a configuration file named `window-switcher.ini`. This file should be placed in the same directory as the `window-switcher.exe` file. Once you've made changes to the configuration, make sure to restart Window-Switcher so your new settings can take effect.

Here is the default configuration:

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

# Only switch within the current virtual desktops: yes/no/auto
only_current_desktop = auto

[switch-apps]

# Whether to enable switching apps
enable = no 

# Hotkey to switch apps
hotkey = alt+tab

# Ignore minimal windows
ignore_minimal = no

# How to render app-switch entries.
# icon_only = current compact icon strip behavior; recommended when preview cards
#   are incompatible with an app or when you want the lightest overlay.
# preview = show live preview cards when DWM thumbnails are available;
#   entries that cannot provide a preview fall back to icons
render_mode = icon_only

# Whether to show per-window dot indicators for apps with more than one window.
# When enabled, each dot represents one window; the active dot highlights the
# currently selected window during Alt+` same-app cycling.
show_window_count = no

# Scale the overlay card size as a percentage (50-200, default 100).
# Higher values produce larger cards and previews.
overlay_scale = 100

# Overall overlay opacity as a percentage (0-100, default 100).
# Lower values make the overlay more transparent.
backdrop_opacity = 100

# Optional hex color for the overlay background (e.g. #2d2d2d).
# When not set the overlay follows the current Windows light/dark theme.
backdrop_color =

# Which app window to use as the representative target.
# legacy_minimized_fallback = use the original upstream behavior:
#   if the first grouped window is minimized, fall back to the last window in that app group.
# first_window = always use the first window in the existing app-group order.
representative_window = legacy_minimized_fallback

# List of override icons, syntax: app1.exe=icon1.ico,app2.exe=icon2.png.
# The icon path can be a full path or a relative path to the app's directory.
# The icon format can be ico or png.
override_icons =

# Only switch apps within the current virtual desktops: yes/no/auto
only_current_desktop = auto
```

`icon_only` remains the default and the safest fallback mode. Prefer it when preview cards are incompatible with a specific app, when you want the lightest possible overlay for rapid switching, or while validating preview behavior on a new Windows setup.

The overlay automatically adapts to an adaptive multi-row grid when there are more apps than fit in a single row. Use `overlay_scale` to make cards larger or smaller, `backdrop_opacity` to control transparency, and `backdrop_color` to override the theme-derived background.

## Running as Administrator (Optional)

The window-switcher works in standard user mode. But only the window-switcher running in administrator mode can manage applications running in administrator mode.

**Important:** If you enable the startup option while running in standard user mode, it will launch in standard mode upon system reboot. To ensure startup with admin privileges, launch the window-switcher as administrator first before enabling startup.

## License

Copyright (c) 2023-2026 window-switcher developers.

window-switcher is made available under the terms of the MIT License, at your option.

See the LICENSE files for license details.

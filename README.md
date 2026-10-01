A simple desktop/game streaming tool. Currently experimental - has only been confirmed to work on MacOS. Supports mouse (position-based) and keyboard, plus gamepads on Windows and Linux hosts (new, not yet confirmed on real hardware; see below).

Currently only supports one client per stream (if that matters).

## Gamepads

Controllers connected to the browser (standard mapping, up to four) are forwarded while the stream is fullscreen and appear on the host as Xbox 360 controllers. Rumble from games is sent back to the controller in browsers that support gamepad haptics (Chromium-based browsers).

Beam's settings page (tray menu, "Open Beam Settings") shows whether gamepads are ready on the host and walks you through any one-time setup. It updates by itself once you're done.

- **Windows:** needs the [ViGEmBus](https://github.com/nefarius/ViGEmBus) driver. Release builds include its installer inside `beam.exe` and start it on first run; if you skipped it, click "Install driver" in settings.
- **Linux:** the user running Beam needs write access to `/dev/uinput`. Settings shows the command to run.
- **macOS:** for now, macOS hosts cannot receive gamepad input (virtual controllers need an Apple-restricted entitlement). Mouse and keyboard still work.

ViGEmBus is BSD-3-Clause licensed; see [web/public/third-party-notices.txt](web/public/third-party-notices.txt) (also linked from the settings page).

To build the Windows installer into `beam.exe` yourself, put it at `drivers/vigembus.exe` before `cargo build`; without it the build still works and settings links to the download.

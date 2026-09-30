A simple desktop/game streaming tool. Currently experimental - has only been confirmed to work on MacOS, with only mouse (position-based) & keyboard support.

## Gamepads

Controllers connected to the browser (standard mapping, up to four) are forwarded while the stream is fullscreen and appear on the host as Xbox 360 controllers. Rumble from games is sent back to the controller in browsers that support gamepad haptics (Chromium-based browsers).

Beam's settings page (tray menu, "Open Beam Settings") shows whether gamepads are ready on the host and walks you through any one-time setup. It updates by itself once you're done.

- **Windows:** needs the [ViGEmBus](https://github.com/nefarius/ViGEmBus) driver. Click "Install driver" in settings.
- **Linux:** the user running Beam needs write access to `/dev/uinput`. Settings shows the command to run.
- **macOS:** for now, macOS hosts cannot receive gamepad input (virtual controllers need an Apple-restricted entitlement). Mouse and keyboard still work.

Maintainers: to bundle the Windows driver installer in CI builds, set the repository variables `VIGEMBUS_URL` (download link of the ViGEmBus installer) and `VIGEMBUS_SHA256` (its checksum). Without them the settings page links to the download instead.

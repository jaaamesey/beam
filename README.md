A simple desktop/game streaming tool. Currently experimental - has only been confirmed to work on MacOS, with only mouse (position-based) & keyboard support.

## Gamepads

Controllers connected to the browser (standard mapping, up to four) are forwarded while the stream is fullscreen and appear on the host as Xbox 360 controllers. Rumble from games is sent back to the controller in browsers that support gamepad haptics (Chromium-based browsers).

- **Windows:** install the [ViGEmBus](https://github.com/nefarius/ViGEmBus) driver.
- **Linux:** the user running Beam needs write access to `/dev/uinput`.
- **macOS:** for now, macOS hosts cannot receive gamepad input (virtual controllers need a signed driver extension). Mouse and keyboard still work.

use display_info::DisplayInfo;
use enigo::{Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;
use std::cell::RefCell;

thread_local! {
    static SCROLL_REMAINDER: RefCell<(f64, f64)> = const { RefCell::new((0.0, 0.0)) };
}

pub fn check_permissions() {
    #[cfg(target_os = "macos")]
    if !macos_accessibility_client::accessibility::application_is_trusted_with_prompt() {
        tracing::warn!(
            "Beam is not trusted for Accessibility; enable it in System Settings > Privacy & Security > Accessibility"
        );
    }

    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tracing::info!(
            "Wayland detected; Beam will use the compositor/libei input backend if supported"
        );
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Message {
    #[serde(rename = "mouseMove")]
    MouseMove { x: f64, y: f64 },
    #[serde(rename = "wheel")]
    Wheel {
        #[serde(rename = "deltaX")]
        delta_x: f64,
        #[serde(rename = "deltaY")]
        delta_y: f64,
        #[serde(rename = "deltaMode")]
        delta_mode: u32,
    },
    #[serde(rename = "keyDown")]
    KeyDown { code: String, key: String },
    #[serde(rename = "keyUp")]
    KeyUp { code: String, key: String },
    #[serde(rename = "mouseButton")]
    MouseButton { button: u16, down: bool },
}

pub fn new() -> anyhow::Result<Enigo> {
    Ok(Enigo::new(&Settings::default())?)
}

pub fn handle(enigo: &mut Enigo, message: &[u8]) {
    let Ok(message) = serde_json::from_slice::<Message>(message) else {
        return;
    };
    match message {
        Message::MouseMove { x, y } => move_mouse(enigo, x, y),
        Message::Wheel {
            delta_x,
            delta_y,
            delta_mode,
        } => scroll(enigo, delta_x, delta_y, delta_mode),
        Message::KeyDown { code, key } => key_event(enigo, &code, &key, Direction::Press),
        Message::KeyUp { code, key } => key_event(enigo, &code, &key, Direction::Release),
        Message::MouseButton { button, down } => mouse_button(enigo, button, down),
    }
}

fn mouse_button(enigo: &mut Enigo, button: u16, down: bool) {
    let Some(button) = (match button {
        0 => Some(enigo::Button::Left),
        1 => Some(enigo::Button::Middle),
        2 => Some(enigo::Button::Right),
        _ => None,
    }) else {
        return;
    };
    let direction = if down { Direction::Press } else { Direction::Release };
    if let Err(error) = enigo.button(button, direction) {
        tracing::warn!(%error, "could not send native mouse button");
    }
}

fn key_event(enigo: &mut Enigo, code: &str, key: &str, direction: Direction) {
    let Some(key) = map_key(code, key) else { return };
    if let Err(error) = enigo.key(key, direction) {
        tracing::warn!(%error, ?direction, code, "could not send native key");
    }
}

fn map_key(code: &str, key: &str) -> Option<Key> {
    Some(match code {
        "Escape" => Key::Escape,
        "Enter" => Key::Return,
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "Space" => Key::Space,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "ShiftLeft" | "ShiftRight" => Key::Shift,
        "ControlLeft" | "ControlRight" => Key::Control,
        "AltLeft" | "AltRight" => Key::Alt,
        "MetaLeft" | "MetaRight" => Key::Meta,
        "CapsLock" => Key::CapsLock,
        "Delete" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        code if code.starts_with("Key") && code.len() == 4 => {
            Key::Unicode((code.as_bytes()[3] as char).to_ascii_lowercase())
        }
        code if code.starts_with("Digit") && code.len() == 6 => {
            Key::Unicode(code.as_bytes()[5] as char)
        }
        _ => {
            let mut chars = key.chars();
            let value = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Unicode(value)
        }
    })
}

fn move_mouse(enigo: &mut Enigo, x: f64, y: f64) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let displays = match DisplayInfo::all() {
        Ok(displays) => displays,
        Err(error) => {
            tracing::warn!(%error, "could not enumerate displays for mouse input");
            return;
        }
    };
    let Some(display) = displays.iter().find(|display| display.is_primary).or(displays.first()) else {
        tracing::warn!("no display available for mouse input");
        return;
    };
    let x = display.x + (x.clamp(0.0, 1.0) * (display.width.saturating_sub(1) as f64)) as i32;
    let y = display.y + (y.clamp(0.0, 1.0) * (display.height.saturating_sub(1) as f64)) as i32;
    if let Err(error) = enigo.move_mouse(x, y, Coordinate::Abs) {
        tracing::warn!(%error, "could not move native mouse");
    }
}

fn scroll(enigo: &mut Enigo, delta_x: f64, delta_y: f64, delta_mode: u32) {
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return;
    }
    let scale = match delta_mode {
        0 => 1.0 / 40.0,
        1 => 1.0,
        2 => 24.0,
        _ => return,
    };
    let (horizontal, vertical) = SCROLL_REMAINDER.with(|remainder| {
        let mut remainder = remainder.borrow_mut();
        remainder.0 += delta_x * scale;
        remainder.1 += delta_y * scale;
        let horizontal = remainder.0 as i32;
        let vertical = remainder.1 as i32;
        remainder.0 -= horizontal as f64;
        remainder.1 -= vertical as f64;
        (horizontal, vertical)
    });
    if horizontal != 0 {
        if let Err(error) = enigo.scroll(horizontal, enigo::Axis::Horizontal) {
            tracing::warn!(%error, "could not scroll horizontally");
        }
    }
    if vertical != 0 {
        if let Err(error) = enigo.scroll(vertical, enigo::Axis::Vertical) {
            tracing::warn!(%error, "could not scroll vertically");
        }
    }
}

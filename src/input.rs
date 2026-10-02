use crate::{gamepad::{self, Gamepads}, held::Held};
use display_info::DisplayInfo;
use enigo::{Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;
use std::{
    cell::RefCell,
    sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}, mpsc::Receiver},
    time::Duration,
};
use tokio::sync::broadcast;

thread_local! {
    static SCROLL_REMAINDER: RefCell<(f64, f64)> = const { RefCell::new((0.0, 0.0)) };
    static MOVE_REMAINDER: RefCell<(f64, f64)> = const { RefCell::new((0.0, 0.0)) };
}

#[derive(Clone, Copy)]
struct DisplayBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

static PRIMARY_DISPLAY: OnceLock<Option<DisplayBounds>> = OnceLock::new();

pub struct Session {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn(
    receiver: Receiver<Vec<u8>>,
    initial: Option<Enigo>,
    shutdown: Arc<AtomicBool>,
    rumble: broadcast::Sender<gamepad::Rumble>,
) -> Session {
    let thread_shutdown = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name("beam-input".into())
        .spawn(move || run(receiver, initial, thread_shutdown, rumble))
        .expect("input thread");
    Session { shutdown, thread: Some(thread) }
}

fn run(
    receiver: Receiver<Vec<u8>>,
    mut enigo: Option<Enigo>,
    shutdown: Arc<AtomicBool>,
    rumble: broadcast::Sender<gamepad::Rumble>,
) {
    let mut gamepads = Gamepads::new(rumble);
    let mut held = Held::default();
    while !shutdown.load(Ordering::Relaxed) {
        let first = match receiver.recv_timeout(Duration::from_millis(2)) {
            Ok(message) => message,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        process_batch(std::iter::once(first).chain(receiver.try_iter()), |message| {
            handle_with_session(&mut enigo, &mut gamepads, &mut held, &message);
        });
    }
}

fn process_batch(
    messages: impl IntoIterator<Item = Vec<u8>>,
    mut handle: impl FnMut(Vec<u8>),
) {
    let mut latest_mouse_move = None;
    for message in messages {
        if is_mouse_move(&message) {
            latest_mouse_move = Some(message);
        } else {
            if let Some(message) = latest_mouse_move.take() {
                handle(message);
            }
            handle(message);
        }
    }
    if let Some(message) = latest_mouse_move {
        handle(message);
    }
}

fn handle_with_session(
    enigo: &mut Option<Enigo>,
    gamepads: &mut Gamepads,
    held: &mut Held,
    message: &[u8],
) {
    match serde_json::from_slice::<Message>(message) {
        Ok(Message::Gamepad { index, state }) => gamepads.update(index, &state),
        Ok(Message::GamepadDisconnected { index }) => gamepads.disconnect(index),
        Ok(Message::ReleaseAll) => {
            gamepads.release_all();
            if let Some(enigo) = enigo.as_mut() {
                release_held(enigo, held);
            }
        }
        Ok(message) => {
            if enigo.is_none() {
                *enigo = new().map_err(|error| tracing::error!(%error, "could not initialise native input")).ok();
            }
            if let Some(enigo) = enigo.as_mut() {
                handle(enigo, held, message);
            }
        }
        Err(_) => {}
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Message {
    #[serde(rename = "mouseMove")]
    MouseMove { x: f64, y: f64 },
    #[serde(rename = "mouseMoveRelative")]
    MouseMoveRelative { dx: f64, dy: f64 },
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
    #[serde(rename = "gamepad")]
    Gamepad {
        index: u8,
        #[serde(flatten)]
        state: gamepad::State,
    },
    #[serde(rename = "gamepadDisconnected")]
    GamepadDisconnected { index: u8 },
    #[serde(rename = "releaseAll")]
    ReleaseAll,
}

pub fn is_mouse_move(message: &[u8]) -> bool {
    matches!(serde_json::from_slice::<Message>(message), Ok(Message::MouseMove { .. }))
}

pub fn new() -> anyhow::Result<Enigo> {
    Ok(Enigo::new(&Settings::default())?)
}

fn release_held(enigo: &mut Enigo, held: &mut Held) {
    let (keys, buttons) = held.take();
    for (code, key) in keys {
        key_event(enigo, &code, &key, Direction::Release);
    }
    for button in buttons {
        mouse_button(enigo, button, false);
    }
}

fn handle(enigo: &mut Enigo, held: &mut Held, message: Message) {
    match message {
        Message::MouseMove { x, y } => move_mouse(enigo, x, y),
        Message::MouseMoveRelative { dx, dy } => move_mouse_relative(enigo, dx, dy),
        Message::Wheel {
            delta_x,
            delta_y,
            delta_mode,
        } => scroll(enigo, delta_x, delta_y, delta_mode),
        Message::KeyDown { code, key } => {
            let key = held.press_key(&code, &key);
            key_event(enigo, &code, &key, Direction::Press);
        }
        Message::KeyUp { code, key } => {
            let key = held.release_key(&code).unwrap_or(key);
            key_event(enigo, &code, &key, Direction::Release);
        }
        Message::MouseButton { button, down } => {
            held.button(button, down);
            mouse_button(enigo, button, down);
        }
        Message::Gamepad { .. } | Message::GamepadDisconnected { .. } | Message::ReleaseAll => {}
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
    let Some(display) = PRIMARY_DISPLAY.get_or_init(|| {
        let displays = match DisplayInfo::all() {
            Ok(displays) => displays,
            Err(error) => {
                tracing::warn!(%error, "could not enumerate displays for mouse input");
                return None;
            }
        };
        let display = displays.iter().find(|display| display.is_primary).or(displays.first())?;
        Some(DisplayBounds {
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        })
    }) else {
        tracing::warn!("no display available for mouse input");
        return;
    };
    let (x, y) = map_position(*display, x, y, cfg!(target_os = "linux"));
    if let Err(error) = enigo.move_mouse(x, y, Coordinate::Abs) {
        tracing::warn!(%error, "could not move native mouse");
    }
}

fn map_position(display: DisplayBounds, x: f64, y: f64, local_origin: bool) -> (i32, i32) {
    let offset_x = if local_origin { 0 } else { display.x };
    let offset_y = if local_origin { 0 } else { display.y };
    (
        offset_x + (x.clamp(0.0, 1.0) * display.width.saturating_sub(1) as f64) as i32,
        offset_y + (y.clamp(0.0, 1.0) * display.height.saturating_sub(1) as f64) as i32,
    )
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
    let (horizontal, vertical) = SCROLL_REMAINDER
        .with(|remainder| whole_steps(&mut remainder.borrow_mut(), delta_x * scale, delta_y * scale));
    if horizontal != 0
        && let Err(error) = enigo.scroll(horizontal, enigo::Axis::Horizontal)
    {
        tracing::warn!(%error, "could not scroll horizontally");
    }
    if vertical != 0
        && let Err(error) = enigo.scroll(vertical, enigo::Axis::Vertical)
    {
        tracing::warn!(%error, "could not scroll vertically");
    }
}

fn move_mouse_relative(enigo: &mut Enigo, dx: f64, dy: f64) {
    if !dx.is_finite() || !dy.is_finite() {
        return;
    }
    let (x, y) = MOVE_REMAINDER.with(|remainder| whole_steps(&mut remainder.borrow_mut(), dx, dy));
    if (x, y) != (0, 0)
        && let Err(error) = enigo.move_mouse(x, y, Coordinate::Rel)
    {
        tracing::warn!(%error, "could not move native mouse");
    }
}

/// Accumulates fractional deltas and returns the whole steps.
fn whole_steps(remainder: &mut (f64, f64), dx: f64, dy: f64) -> (i32, i32) {
    remainder.0 += dx;
    remainder.1 += dy;
    let steps = (remainder.0 as i32, remainder.1 as i32);
    remainder.0 -= f64::from(steps.0);
    remainder.1 -= f64::from(steps.1);
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_moves_are_parsed_and_never_coalesced() {
        let message = br#"{"type":"mouseMoveRelative","dx":3.5,"dy":-2}"#;
        assert!(matches!(
            serde_json::from_slice::<Message>(message),
            Ok(Message::MouseMoveRelative { dx: 3.5, dy: -2.0 })
        ));
        assert!(!is_mouse_move(message));
    }

    #[test]
    fn fractional_deltas_accumulate_into_whole_steps() {
        let mut remainder = (0.0, 0.0);
        assert_eq!(whole_steps(&mut remainder, 0.4, -0.6), (0, 0));
        assert_eq!(whole_steps(&mut remainder, 0.4, -0.6), (0, -1));
        assert_eq!(whole_steps(&mut remainder, 0.4, 0.0), (1, 0));
        assert!((remainder.0 - 0.2).abs() < 1e-9 && (remainder.1 + 0.2).abs() < 1e-9);
    }

    #[test]
    fn coalesces_only_consecutive_mouse_moves() {
        let move_one = br#"{"type":"mouseMove","x":0.1,"y":0.1}"#.to_vec();
        let move_two = br#"{"type":"mouseMove","x":0.2,"y":0.2}"#.to_vec();
        let click = br#"{"type":"mouseButton","button":0,"down":true}"#.to_vec();
        let move_three = br#"{"type":"mouseMove","x":0.3,"y":0.3}"#.to_vec();
        let mut output = Vec::new();

        process_batch(
            vec![move_one, move_two.clone(), click.clone(), move_three.clone()],
            |message| output.push(message),
        );

        assert_eq!(output, vec![move_two, click, move_three]);
    }

    #[test]
    fn linux_pointer_coordinates_are_local_to_the_capture_region() {
        let display = DisplayBounds { x: 1920, y: 200, width: 1920, height: 1080 };
        assert_eq!(map_position(display, 0.0, 0.0, true), (0, 0));
        assert_eq!(map_position(display, 1.0, 1.0, true), (1919, 1079));
        assert_eq!(map_position(display, 0.0, 0.0, false), (1920, 200));
    }
}

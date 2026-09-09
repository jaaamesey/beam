use display_info::DisplayInfo;
use enigo::{Coordinate, Enigo, Mouse, Settings};
use serde::Deserialize;
use std::sync::mpsc::Receiver;

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
        delta_x: f64,
        delta_y: f64,
        delta_mode: u32,
    },
}

pub fn run(messages: Receiver<Vec<u8>>) {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(enigo) => enigo,
        Err(error) => {
            tracing::error!(%error, "could not initialise native mouse input");
            return;
        }
    };
    for message in messages {
        handle(&mut enigo, &message);
    }
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
    }
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
        0 => 1.0,
        1 => 1.0,
        2 => 24.0,
        _ => return,
    };
    let horizontal = (delta_x * scale).round() as i32;
    let vertical = (delta_y * scale).round() as i32;
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

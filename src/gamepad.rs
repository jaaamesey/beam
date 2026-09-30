use serde::Deserialize;
use std::collections::HashMap;

/// A browser gamepad snapshot using the W3C "standard" mapping.
#[derive(Deserialize)]
pub struct State {
    pub buttons: Vec<f64>,
    pub axes: Vec<f64>,
}

/// Xbox 360 button bits, as used by XInput.
mod button {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Standard-mapping button index -> Xbox 360 button bit. Indices 6 and 7 are
/// the analog triggers and are reported separately.
const BUTTONS: [(usize, u16); 15] = [
    (0, button::A),
    (1, button::B),
    (2, button::X),
    (3, button::Y),
    (4, button::LEFT_SHOULDER),
    (5, button::RIGHT_SHOULDER),
    (8, button::BACK),
    (9, button::START),
    (10, button::LEFT_THUMB),
    (11, button::RIGHT_THUMB),
    (12, button::DPAD_UP),
    (13, button::DPAD_DOWN),
    (14, button::DPAD_LEFT),
    (15, button::DPAD_RIGHT),
    (16, button::GUIDE),
];

/// An Xbox 360 controller report. Stick Y axes are positive when pushed up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Report {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

impl From<&State> for Report {
    fn from(state: &State) -> Self {
        let value = |index: usize| state.buttons.get(index).copied().unwrap_or(0.0);
        let axis = |index: usize| state.axes.get(index).copied().unwrap_or(0.0);
        let stick = |value: f64| (value.clamp(-1.0, 1.0) * f64::from(i16::MAX)) as i16;
        let trigger = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self {
            buttons: BUTTONS
                .iter()
                .filter(|(index, _)| value(*index) > 0.5)
                .fold(0, |bits, (_, bit)| bits | bit),
            left_trigger: trigger(value(6)),
            right_trigger: trigger(value(7)),
            left_x: stick(axis(0)),
            left_y: stick(-axis(1)),
            right_x: stick(axis(2)),
            right_y: stick(-axis(3)),
        }
    }
}

/// Virtual controllers on the host, one per connected browser gamepad.
#[derive(Default)]
pub struct Gamepads {
    devices: HashMap<u8, platform::Device>,
    unavailable: bool,
}

impl Gamepads {
    pub fn update(&mut self, index: u8, state: &State) {
        if self.unavailable {
            return;
        }
        // Browsers can expose only a handful of pads; ignore anything wilder.
        if index >= 4 {
            return;
        }
        let report = Report::from(state);
        if !self.devices.contains_key(&index) {
            match platform::Device::new(index) {
                Ok(device) => {
                    self.devices.insert(index, device);
                }
                Err(error) => {
                    tracing::warn!(%error, "gamepad input is unavailable on this host");
                    self.unavailable = true;
                    return;
                }
            }
        }
        if let Some(device) = self.devices.get_mut(&index)
            && let Err(error) = device.send(&report)
        {
            tracing::warn!(%error, "could not send gamepad state");
        }
    }

    pub fn disconnect(&mut self, index: u8) {
        self.devices.remove(&index);
    }
}

#[cfg(windows)]
mod platform {
    use super::Report;
    use std::sync::Arc;
    use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

    /// Requires the ViGEmBus driver: https://github.com/nefarius/ViGEmBus
    pub struct Device(Xbox360Wired<Arc<Client>>);

    impl Device {
        pub fn new(_index: u8) -> anyhow::Result<Self> {
            let client = Arc::new(Client::connect().map_err(|error| {
                anyhow::anyhow!("ViGEmBus driver is not installed or not running ({error})")
            })?);
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            target.plugin()?;
            target.wait_ready()?;
            Ok(Self(target))
        }

        pub fn send(&mut self, report: &Report) -> anyhow::Result<()> {
            self.0.update(&XGamepad {
                buttons: XButtons { raw: report.buttons },
                left_trigger: report.left_trigger,
                right_trigger: report.right_trigger,
                thumb_lx: report.left_x,
                thumb_ly: report.left_y,
                thumb_rx: report.right_x,
                thumb_ry: report.right_y,
            })?;
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Report, button};
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
        UinputAbsSetup, uinput::VirtualDevice,
    };

    /// Requires write access to /dev/uinput.
    pub struct Device {
        device: VirtualDevice,
        previous: Report,
    }

    const KEYS: [(u16, KeyCode); 11] = [
        (button::A, KeyCode::BTN_SOUTH),
        (button::B, KeyCode::BTN_EAST),
        (button::X, KeyCode::BTN_WEST),
        (button::Y, KeyCode::BTN_NORTH),
        (button::LEFT_SHOULDER, KeyCode::BTN_TL),
        (button::RIGHT_SHOULDER, KeyCode::BTN_TR),
        (button::BACK, KeyCode::BTN_SELECT),
        (button::START, KeyCode::BTN_START),
        (button::GUIDE, KeyCode::BTN_MODE),
        (button::LEFT_THUMB, KeyCode::BTN_THUMBL),
        (button::RIGHT_THUMB, KeyCode::BTN_THUMBR),
    ];

    fn stick(axis: AbsoluteAxisCode) -> UinputAbsSetup {
        UinputAbsSetup::new(axis, AbsInfo::new(0, -32768, 32767, 16, 128, 0))
    }

    fn trigger(axis: AbsoluteAxisCode) -> UinputAbsSetup {
        UinputAbsSetup::new(axis, AbsInfo::new(0, 0, 255, 0, 0, 0))
    }

    fn hat(axis: AbsoluteAxisCode) -> UinputAbsSetup {
        UinputAbsSetup::new(axis, AbsInfo::new(0, -1, 1, 0, 0, 0))
    }

    impl Device {
        pub fn new(_index: u8) -> anyhow::Result<Self> {
            let keys = KEYS.iter().map(|(_, key)| *key).collect::<AttributeSet<_>>();
            let device = VirtualDevice::builder()?
                .name("Beam Virtual Gamepad")
                // Microsoft Xbox 360 controller, so SDL and games apply their usual mapping.
                .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
                .with_keys(&keys)?
                .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_X))?
                .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_Y))?
                .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RX))?
                .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RY))?
                .with_absolute_axis(&trigger(AbsoluteAxisCode::ABS_Z))?
                .with_absolute_axis(&trigger(AbsoluteAxisCode::ABS_RZ))?
                .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0X))?
                .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0Y))?
                .build()
                .map_err(|error| {
                    anyhow::anyhow!("could not create a uinput device; check /dev/uinput permissions ({error})")
                })?;
            Ok(Self {
                device,
                previous: Report::default(),
            })
        }

        pub fn send(&mut self, report: &Report) -> anyhow::Result<()> {
            let (old, new) = (self.previous, *report);
            let key = |bit: u16, code: KeyCode| {
                ((old.buttons ^ new.buttons) & bit != 0).then(|| {
                    InputEvent::new(EventType::KEY.0, code.0, i32::from(new.buttons & bit != 0))
                })
            };
            let axis = |changed: bool, code: AbsoluteAxisCode, value: i32| {
                changed.then(|| InputEvent::new(EventType::ABSOLUTE.0, code.0, value))
            };
            let hat = |negative: u16, positive: u16| {
                i32::from(new.buttons & positive != 0) - i32::from(new.buttons & negative != 0)
            };
            let hat_changed = |negative: u16, positive: u16| {
                (old.buttons ^ new.buttons) & (negative | positive) != 0
            };
            let events = KEYS
                .iter()
                .filter_map(|(bit, code)| key(*bit, *code))
                .chain([
                    axis(old.left_x != new.left_x, AbsoluteAxisCode::ABS_X, new.left_x.into()),
                    // evdev's Y axes are positive downwards.
                    axis(old.left_y != new.left_y, AbsoluteAxisCode::ABS_Y, -i32::from(new.left_y)),
                    axis(old.right_x != new.right_x, AbsoluteAxisCode::ABS_RX, new.right_x.into()),
                    axis(old.right_y != new.right_y, AbsoluteAxisCode::ABS_RY, -i32::from(new.right_y)),
                    axis(old.left_trigger != new.left_trigger, AbsoluteAxisCode::ABS_Z, new.left_trigger.into()),
                    axis(old.right_trigger != new.right_trigger, AbsoluteAxisCode::ABS_RZ, new.right_trigger.into()),
                    axis(
                        hat_changed(button::DPAD_LEFT, button::DPAD_RIGHT),
                        AbsoluteAxisCode::ABS_HAT0X,
                        hat(button::DPAD_LEFT, button::DPAD_RIGHT),
                    ),
                    axis(
                        hat_changed(button::DPAD_UP, button::DPAD_DOWN),
                        AbsoluteAxisCode::ABS_HAT0Y,
                        hat(button::DPAD_UP, button::DPAD_DOWN),
                    ),
                ]
                .into_iter()
                .flatten())
                .collect::<Vec<_>>();
            // `emit` appends the SYN_REPORT event itself.
            self.device.emit(&events)?;
            self.previous = new;
            Ok(())
        }
    }
}

/// macOS has no supported way to create a virtual game controller without a
/// signed driver extension, so gamepads are reported as unavailable there.
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::Report;

    pub struct Device;

    impl Device {
        pub fn new(_index: u8) -> anyhow::Result<Self> {
            anyhow::bail!("virtual gamepads are not supported on this platform")
        }

        pub fn send(&mut self, _report: &Report) -> anyhow::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pressed: &[usize], axes: [f64; 4]) -> State {
        let mut buttons = vec![0.0; 17];
        for index in pressed {
            buttons[*index] = 1.0;
        }
        State {
            buttons,
            axes: axes.to_vec(),
        }
    }

    #[test]
    fn maps_buttons_and_dpad() {
        let report = Report::from(&state(&[0, 4, 9, 12], [0.0; 4]));
        assert_eq!(
            report.buttons,
            button::A | button::LEFT_SHOULDER | button::START | button::DPAD_UP
        );
    }

    #[test]
    fn triggers_are_analog_and_not_buttons() {
        let mut state = state(&[], [0.0; 4]);
        state.buttons[6] = 1.0;
        state.buttons[7] = 0.5;
        let report = Report::from(&state);
        assert_eq!((report.left_trigger, report.right_trigger), (255, 128));
        assert_eq!(report.buttons, 0);
    }

    #[test]
    fn sticks_scale_clamp_and_flip_y() {
        let report = Report::from(&state(&[], [1.0, -1.0, -0.5, 2.0]));
        assert_eq!(report.left_x, i16::MAX);
        assert_eq!(report.left_y, i16::MAX); // browser up (-1) is XInput up (+)
        assert_eq!(report.right_x, -16383);
        assert_eq!(report.right_y, -i16::MAX);
    }

    #[test]
    fn short_or_hostile_input_is_safe() {
        let report = Report::from(&State {
            buttons: vec![],
            axes: vec![f64::NAN],
        });
        assert_eq!(report, Report::default());
    }
}

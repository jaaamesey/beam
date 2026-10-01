use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

/// How long to wait before trying to create a virtual controller again, e.g. while the driver is being installed.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// A browser gamepad snapshot (W3C standard mapping).
#[derive(Deserialize)]
pub struct State {
    pub buttons: Vec<f64>,
    pub axes: Vec<f64>,
}

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

/// Standard-mapping button index and its XInput bit. The triggers (6, 7) are analog.
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

/// Xbox 360 controller state. Stick Y is positive when pushed up.
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rumble {
    pub index: u8,
    pub strong: u8,
    pub weak: u8,
}

#[cfg(any(target_os = "linux", test))]
fn combine(effects: impl Iterator<Item = (u16, u16)>, gain: u16) -> (u8, u8) {
    let (strong, weak) = effects.fold((0u64, 0u64), |(strong, weak), (s, w)| {
        (strong + u64::from(s), weak + u64::from(w))
    });
    let scale = |total: u64| ((total * u64::from(gain) / u64::from(u16::MAX)).min(0xffff) >> 8) as u8;
    (scale(strong), scale(weak))
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Ready,
    /// ViGEmBus on Windows, the uinput module on Linux.
    DriverMissing,
    NoPermission,
    Unsupported,
    Failed { message: String },
}

#[derive(Serialize)]
pub struct Health {
    #[serde(flatten)]
    pub status: Status,
    pub platform: &'static str,
    pub installer_available: bool,
}

pub fn health() -> Health {
    Health {
        status: platform::probe(),
        platform: std::env::consts::OS,
        installer_available: platform::installer_available(),
    }
}

pub fn install_driver() -> anyhow::Result<()> {
    platform::install()
}

pub fn install_driver_if_missing() {
    if matches!(platform::probe(), Status::DriverMissing)
        && platform::installer_available()
        && let Err(error) = install_driver()
    {
        tracing::warn!(%error, "could not start the gamepad driver installer");
    }
}

pub struct Gamepads {
    devices: HashMap<u8, platform::Device>,
    retry_after: Option<Instant>,
    rumble: broadcast::Sender<Rumble>,
}

impl Gamepads {
    pub fn new(rumble: broadcast::Sender<Rumble>) -> Self {
        Self {
            devices: HashMap::new(),
            retry_after: None,
            rumble,
        }
    }

    pub fn update(&mut self, index: u8, state: &State) {
        if index >= 4 {
            return;
        }
        let report = Report::from(state);
        if !self.devices.contains_key(&index) {
            if self.retry_after.is_some_and(|time| Instant::now() < time) {
                return;
            }
            let rumble = self.rumble.clone();
            let notify = move |strong, weak| {
                let _ = rumble.send(Rumble { index, strong, weak });
            };
            match platform::Device::new(notify) {
                Ok(device) => {
                    self.retry_after = None;
                    self.devices.insert(index, device);
                }
                Err(error) => {
                    if self.retry_after.is_none() {
                        tracing::warn!(%error, "gamepad input is unavailable on this host");
                    }
                    self.retry_after = Some(Instant::now() + RETRY_INTERVAL);
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

    pub fn release_all(&mut self) {
        for device in self.devices.values_mut() {
            if let Err(error) = device.send(&Report::default()) {
                tracing::warn!(%error, "could not reset gamepad state");
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::Report;
    use super::Status;
    use anyhow::Context;
    use std::sync::Arc;
    use vigem_client::{Client, Error, TargetId, XButtons, XGamepad, Xbox360Wired};

    // Empty unless build.rs found drivers/vigembus.exe.
    const INSTALLER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vigembus.exe"));

    pub fn probe() -> Status {
        match Client::connect() {
            Ok(_) => Status::Ready,
            Err(Error::BusNotFound) => Status::DriverMissing,
            Err(error) => Status::Failed { message: error.to_string() },
        }
    }

    pub fn installer_available() -> bool {
        !INSTALLER.is_empty()
    }

    pub fn install() -> anyhow::Result<()> {
        anyhow::ensure!(installer_available(), "this build doesn't include the driver installer");
        let folder = dirs::data_local_dir().context("no local data directory")?.join("beam");
        std::fs::create_dir_all(&folder)?;
        let path = folder.join("ViGEmBus-setup.exe");
        std::fs::write(&path, INSTALLER)?;
        open::that(path)?;
        Ok(())
    }

    pub struct Device(Xbox360Wired<Arc<Client>>);

    impl Device {
        pub fn new(notify: impl Fn(u8, u8) + Send + 'static) -> anyhow::Result<Self> {
            let client = Arc::new(Client::connect().map_err(|error| {
                anyhow::anyhow!("ViGEmBus driver is not installed or not running ({error})")
            })?);
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            target.plugin()?;
            target.wait_ready()?;
            target
                .request_notification()?
                .spawn_thread(move |_, rumble| notify(rumble.large_motor, rumble.small_motor));
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
    use super::{Report, Status, button};
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode,
        FFEffectKind, InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup,
        uinput::VirtualDevice,
    };
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::{
        collections::HashMap,
        io::ErrorKind,
        os::fd::AsRawFd,
        sync::{Arc, Mutex, Weak},
        time::{Duration, Instant},
    };

    pub struct Device {
        device: Arc<Mutex<VirtualDevice>>,
        previous: Report,
    }

    const MAX_EFFECTS: i16 = 16;

    pub fn probe() -> Status {
        match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
            Ok(_) => Status::Ready,
            Err(error) if error.kind() == ErrorKind::NotFound => Status::DriverMissing,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => Status::NoPermission,
            Err(error) => Status::Failed { message: error.to_string() },
        }
    }

    pub fn installer_available() -> bool {
        false
    }

    pub fn install() -> anyhow::Result<()> {
        anyhow::bail!("this build doesn't include the driver installer")
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
        pub fn new(notify: impl Fn(u8, u8) + Send + 'static) -> anyhow::Result<Self> {
            let keys = KEYS.iter().map(|(_, key)| *key).collect::<AttributeSet<_>>();
            let device = VirtualDevice::builder()?
                .name("Beam Virtual Gamepad")
                // Xbox 360 ids, so SDL and games pick the usual mapping.
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
                .with_ff(&AttributeSet::from_iter([FFEffectCode::FF_RUMBLE, FFEffectCode::FF_GAIN]))?
                .with_ff_effects_max(MAX_EFFECTS as u32)
                .build()
                .map_err(|error| {
                    anyhow::anyhow!("could not create a uinput device; check /dev/uinput permissions ({error})")
                })?;
            // Rumble requests arrive on this same fd; don't let the reader block `send`.
            set_nonblocking(&device)?;
            let device = Arc::new(Mutex::new(device));
            let weak = Arc::downgrade(&device);
            std::thread::spawn(move || serve_force_feedback(weak, notify));
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
                    // evdev Y is positive downwards.
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
            self.device.lock().unwrap().emit(&events)?;
            self.previous = new;
            Ok(())
        }
    }

    fn set_nonblocking(device: &VirtualDevice) -> std::io::Result<()> {
        let fd = device.as_raw_fd();
        let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
        fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(())
    }

    fn serve_force_feedback(device: Weak<Mutex<VirtualDevice>>, notify: impl Fn(u8, u8)) {
        // id -> (strong, weak, length in ms; 0 plays until stopped)
        let mut effects = HashMap::<i16, (u16, u16, u16)>::new();
        let mut playing = HashMap::<i16, Option<Instant>>::new();
        let mut gain = u16::MAX;
        let mut last = (0, 0);
        loop {
            std::thread::sleep(Duration::from_millis(8));
            let Some(device) = device.upgrade() else { return };
            let mut device = device.lock().unwrap();
            let events = match device.fetch_events() {
                Ok(events) => events.collect::<Vec<_>>(),
                Err(error) if error.kind() == ErrorKind::WouldBlock => Vec::new(),
                Err(error) => {
                    tracing::warn!(%error, "gamepad force feedback stopped");
                    return;
                }
            };
            for event in events {
                match event.destructure() {
                    EventSummary::UInput(event, UInputCode::UI_FF_UPLOAD, ..) => {
                        let Ok(mut upload) = device.process_ff_upload(event) else { continue };
                        let effect = upload.effect();
                        let id = upload.effect_id();
                        let known = effects.contains_key(&id);
                        let free = (0..MAX_EFFECTS).find(|id| !effects.contains_key(id));
                        match (effect.kind, if known && id >= 0 { Some(id) } else { free }) {
                            (FFEffectKind::Rumble { strong_magnitude, weak_magnitude }, Some(id)) => {
                                effects.insert(id, (strong_magnitude, weak_magnitude, effect.replay.length));
                                upload.set_effect_id(id);
                                upload.set_retval(0);
                            }
                            _ => upload.set_retval(-1),
                        }
                    }
                    EventSummary::UInput(event, UInputCode::UI_FF_ERASE, ..) => {
                        if let Ok(erase) = device.process_ff_erase(event) {
                            let id = erase.effect_id() as i16;
                            effects.remove(&id);
                            playing.remove(&id);
                        }
                    }
                    EventSummary::ForceFeedback(_, FFEffectCode::FF_GAIN, value) => {
                        gain = value.clamp(0, i32::from(u16::MAX)) as u16;
                    }
                    EventSummary::ForceFeedback(_, code, value) => {
                        let id = code.0 as i16;
                        match effects.get(&id) {
                            Some(&(.., length)) if value > 0 => {
                                let stop = (length > 0)
                                    .then(|| Instant::now() + Duration::from_millis(length.into()));
                                playing.insert(id, stop);
                            }
                            _ => {
                                playing.remove(&id);
                            }
                        }
                    }
                    _ => {}
                }
            }
            drop(device);
            let now = Instant::now();
            playing.retain(|_, stop| stop.is_none_or(|stop| stop > now));
            let current = super::combine(
                playing.keys().filter_map(|id| effects.get(id)).map(|&(strong, weak, _)| (strong, weak)),
                gain,
            );
            if current != last {
                last = current;
                notify(current.0, current.1);
            }
        }
    }
}

/// Virtual controllers need a signed driver extension here.
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{Report, Status};

    pub fn probe() -> Status {
        Status::Unsupported
    }

    pub fn installer_available() -> bool {
        false
    }

    pub fn install() -> anyhow::Result<()> {
        anyhow::bail!("this build doesn't include the driver installer")
    }

    pub struct Device;

    impl Device {
        pub fn new(_notify: impl Fn(u8, u8) + Send + 'static) -> anyhow::Result<Self> {
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
        assert_eq!(report.left_y, i16::MAX);
        assert_eq!(report.right_x, -16383);
        assert_eq!(report.right_y, -i16::MAX);
    }

    #[test]
    fn rumble_effects_add_up_scaled_by_gain() {
        assert_eq!(combine([].into_iter(), u16::MAX), (0, 0));
        assert_eq!(combine([(0xffff, 0x8000)].into_iter(), u16::MAX), (255, 128));
        assert_eq!(combine([(0x4000, 0), (0x4000, 0)].into_iter(), u16::MAX), (128, 0));
        assert_eq!(combine([(0xffff, 0xffff), (0xffff, 0)].into_iter(), u16::MAX), (255, 255));
        assert_eq!(combine([(0xffff, 0xffff)].into_iter(), 0x8000), (128, 128));
    }

    #[test]
    fn health_serializes_flat() {
        let health = Health {
            status: Status::DriverMissing,
            platform: "windows",
            installer_available: true,
        };
        assert_eq!(
            serde_json::to_string(&health).unwrap(),
            r#"{"state":"driver_missing","platform":"windows","installer_available":true}"#
        );
        let failed = serde_json::to_string(&Status::Failed { message: "x".into() }).unwrap();
        assert_eq!(failed, r#"{"state":"failed","message":"x"}"#);
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

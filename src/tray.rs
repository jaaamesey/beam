use anyhow::{Context, Result};
use resvg::{
    render,
    tiny_skia::{Pixmap, Transform},
    usvg::{Options, Tree},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
    sync::mpsc::Receiver,
};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoop},
};
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

pub fn run(
    settings_url: String,
    shutdown: Arc<AtomicBool>,
    input_rx: Receiver<Vec<u8>>,
) -> Result<()> {
    #[allow(unused_mut)] // mutability is only exercised on macOS below
    let mut event_loop = EventLoop::new();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let open = MenuItem::new("Open Beam Settings", true, None);
    let quit = MenuItem::new("Quit Beam", true, None);
    let separator = PredefinedMenuItem::separator();
    let menu = Menu::with_items(&[&open, &separator, &quit])?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    let menu_shutdown = shutdown.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            if let Err(error) = open::that(&settings_url) {
                tracing::warn!(%error, "could not open settings in the browser");
            }
        } else if event.id == quit_id {
            menu_shutdown.store(true, Ordering::Relaxed);
        }
    }));

    let icon = icon()?;
    let mut tray = None;
    let mut enigo = crate::input::new()?;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(2));
        for message in input_rx.try_iter() {
            crate::input::handle(&mut enigo, &message);
        }
        if shutdown.load(Ordering::Relaxed) {
            *control_flow = ControlFlow::Exit;
            return;
        }
        if tray.is_none() && matches!(event, Event::NewEvents(_)) {
            tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip("Beam")
                    .with_icon(icon.clone())
                    .build()
                    .expect("create tray icon"),
            );
        }
    })
}

fn icon() -> Result<Icon> {
    let tree = Tree::from_data(include_bytes!("../assets/tray.svg"), &Options::default())?;
    let size = tree.size().to_int_size();
    let mut image = Pixmap::new(size.width(), size.height()).context("invalid tray icon size")?;
    render(&tree, Transform::default(), &mut image.as_mut());
    Icon::from_rgba(image.take(), size.width(), size.height()).context("invalid tray icon")
}

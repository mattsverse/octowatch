use std::sync::Arc;

use octowatcher_core::{engine::Engine, health::ReviewsStatus, instance, login};
use rune::{AppSpec, Chrome};

mod app;
mod jobs;
#[cfg(test)]
mod tests;
mod theme;
mod tray;
mod ui;

use app::Octowatcher;
use tray::Tray;

/// Rune's headless tooling flags: these runs render the view and exit, so they take no
/// instance lock, tray, disk state or network.
fn headless() -> bool {
    std::env::args().skip(1).any(|arg| {
        matches!(
            arg.as_str(),
            "--snap"
                | "--tree"
                | "--script"
                | "--list-scenes"
                | "--trace"
                | "--video"
                | "--out"
                | "--frames"
        )
    })
}

/// The interface face: macOS's own system font (SF), read from the system at launch and never
/// bundled, so the app sets text exactly as native apps do. Elsewhere, or if it can't be read,
/// Rune's built-in Inter.
fn ui_fonts() -> (&'static [&'static [u8]], &'static str) {
    #[cfg(target_os = "macos")]
    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/SFNS.ttf") {
        let face: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        return (Box::leak(vec![face].into_boxed_slice()), ".SF NS");
    }
    (&[], "Inter")
}

fn spec() -> AppSpec<Octowatcher> {
    let (fonts, family) = ui_fonts();
    AppSpec {
        min_size: Some((460.0, 520.0)),
        on_key: Some(ui::on_key),
        scenes: SCENES,
        fonts,
        // The system monospace face for commands.
        families: (family, "ui-monospace"),
        ..AppSpec::new("Octowatcher", (560.0, 680.0), initial, ui::view)
    }
}

/// The state a headless run starts from (scenes replace it).
fn initial() -> Octowatcher {
    #[cfg(feature = "demo")]
    return scenes::demo();
    #[cfg(not(feature = "demo"))]
    Octowatcher::new(Engine::load())
}

#[cfg(feature = "demo")]
mod scenes;
#[cfg(feature = "demo")]
const SCENES: &[Scene] = scenes::ALL;
#[cfg(not(feature = "demo"))]
const SCENES: &[Scene] = &[];

/// A named state for headless snapshots.
type Scene = (&'static str, fn(&mut Octowatcher));

fn chrome() -> Chrome {
    Chrome {
        full_size_content: true,
        traffic_lights: Some(ui::TRAFFIC_LIGHTS),
        drag_regions: true,
    }
}

fn main() {
    if headless() {
        rune::App::new(spec()).chrome(chrome()).run();
        return;
    }
    rune::prewarm_gpu();
    let instance = match instance::Instance::acquire(login::is_background_launch()) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(err) => {
            eprintln!("could not start Octowatcher: {err:#}");
            std::process::exit(1);
        }
    };
    let engine = Engine::load();
    // Start with no window: the first update decides whether this launch shows one.
    let app = rune::App::new(spec())
        .chrome(chrome())
        .windowless()
        .standard_menus()
        // Writes unsaved state on every close, including the OS quitting the app.
        .on_close(|s: &mut Octowatcher, _| s.engine.flush())
        .on_reopen(|s: &mut Octowatcher, _, _| {
            s.show_window();
            true
        });
    let handle = app.handle();
    let state_handle = handle.clone();
    let app = app.init(move || {
        let mut s = Octowatcher::new(engine);
        s.handle = Some(state_handle);
        s
    });
    tray::listen(handle.clone());
    let pump = handle.clone();
    handle.update(move |s: &mut Octowatcher, tx| {
        // The tray needs the running event loop (NSApplication on macOS), so it starts here.
        // Without one, a quiet launch at login shows the window instead.
        match Tray::new(
            &s.engine.tray_reviews(),
            ReviewsStatus::Loading.tray_message(),
            s.engine.store().notifications_muted,
        ) {
            Ok(tray) => s.tray = Some(tray),
            Err(err) => s.tray_error = Some(format!("could not create tray icon: {err:#}")),
        }
        // Finished jobs wake the UI thread, whether or not a window shows.
        let wake = pump.clone();
        s.jobs.connect(Arc::new(move || {
            wake.update_if(|s: &mut Octowatcher, tx| s.pump(tx))
        }));
        s.start(tx);
        if login::should_show_window(login::is_background_launch(), s.tray.is_some()) {
            s.show_window();
        }
    });
    let reopen = instance.reopen.clone();
    let opener = handle.clone();
    std::thread::Builder::new()
        .name("octowatcher-reopen".into())
        .spawn(move || {
            while reopen.recv_blocking().is_ok() {
                opener.update(|s: &mut Octowatcher, _| s.show_window());
            }
        })
        .expect("could not start the reopen listener");
    app.run();
    // Keep the descriptor and IPC worker alive through the entire app loop, including window
    // closure and the updater's wait-for-exit restart handoff.
    drop(instance);
}

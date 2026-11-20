#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod airplay;
mod drop;
#[cfg(windows)]
mod remote;
#[cfg(not(windows))]
#[path = "remote_linux.rs"]
mod remote;
mod settings;
mod ui_socket;
mod updates;

use std::path::Path;
use std::sync::{Arc, Mutex};

use airplay::audio::Output;
use airplay::hub::Hub;
use serde_json::{Value, json};
use settings::Appearance;
use tauri::{LogicalSize, Manager, PhysicalPosition, PhysicalSize};

/// Space kept around the mirrored screen while the window is shrunk to the phone,
/// so its shadow has room. Matches PHONE_MARGIN in ui/app.js. Windows gets none:
/// whatever it draws around a transparent window (its frame, or a dark backing)
/// shows up as a box around the phone, so there the window is just the phone.
const PHONE_MARGIN: f64 = if cfg!(windows) { 0.0 } else { 18.0 };
/// The window's title while it's shrunk to the phone.
const PHONE_TITLE: &str = "RustPlay Mirroring";
/// The normal window's minimum size (tauri.conf.json).
const MIN_SIZE: (f64, f64) = (520.0, 420.0);

/// Where the window was before it shrank to the phone, to go back to afterwards.
struct Restore {
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
}

struct App {
    hub: Arc<Hub>,
    output: Arc<Output>,
    receiver: airplay::Running,
    files: Arc<drop::FileDrop>,
    remote: Arc<remote::Remote>,
    updates: Arc<updates::Updates>,
    settings: settings::Store,
    window: Option<tauri::WebviewWindow>,
    /// False for a second copy of the app (RUSTPLAY_NAME), which keeps the name it was given.
    persist_name: bool,
    /// Set while the window is shrunk to the phone's shape.
    phone: Mutex<Option<Restore>>,
}

impl App {
    /// Commands from the window, arriving over the UI socket.
    fn command(&self, request: &Value) -> Value {
        match request["type"].as_str() {
            Some(kind) if kind.starts_with("drop-") => self.files.command(request),
            Some(kind) if kind.starts_with("remote-") => self.remote.command(request),
            Some(kind) if kind.starts_with("update-") => self.updates.command(request),
            Some("rename") => match self.rename(request["name"].as_str().unwrap_or_default()) {
                Ok(name) => json!({ "ok": true, "name": name }),
                Err(error) => json!({ "ok": false, "error": error }),
            },
            Some("volume") => {
                let volume = request["volume"].as_f64().unwrap_or(0.8).clamp(0.0, 1.0) as f32;
                self.output.set_volume(volume);
                self.hub.set_volume(volume);
                if request["save"].as_bool().unwrap_or(false) {
                    self.settings.update(|s| s.volume = volume);
                }
                json!({ "ok": true })
            }
            Some("phone-frame") => {
                let on = request["on"].as_bool().unwrap_or(false);
                let width = request["width"].as_f64().unwrap_or(1080.0).max(1.0);
                let height = request["height"].as_f64().unwrap_or(1920.0).max(1.0);
                match self.phone_frame(on, width / height) {
                    Ok(()) => json!({ "ok": true }),
                    Err(err) => json!({ "ok": false, "error": err.to_string() }),
                }
            }
            Some("phone-shape") => {
                if let Some(window) = &self.window {
                    let rect = ["x", "y", "w", "h", "r"].map(|key| request[key].as_f64());
                    let shape = match rect {
                        [Some(x), Some(y), Some(w), Some(h), Some(r)] => Some(Shape { x, y, w, h, r }),
                        _ => None,
                    };
                    clip_window(window, shape);
                }
                json!({ "ok": true })
            }
            Some("stop") => {
                self.hub.stop_mirroring();
                json!({ "ok": true })
            }
            Some("settings") => {
                let settings = self.settings.get();
                json!({ "ok": true, "appearance": settings.appearance, "requireCode": settings.require_code })
            }
            Some("set-appearance") => match serde_json::from_value(request["appearance"].clone()) {
                Ok(appearance) => {
                    self.settings.update(|s| s.appearance = appearance);
                    if let Some(window) = &self.window {
                        apply_appearance(window, appearance);
                    }
                    json!({ "ok": true })
                }
                Err(_) => json!({ "ok": false, "error": "unknown appearance" }),
            },
            Some("set-require-code") => {
                let required = request["on"].as_bool().unwrap_or(false);
                match self.receiver.require_code(required) {
                    Ok(()) => {
                        self.settings.update(|s| s.require_code = required);
                        json!({ "ok": true })
                    }
                    Err(err) => json!({ "ok": false, "error": format!("Couldn't change this: {err}") }),
                }
            }
            Some("code-cancel") => {
                self.receiver.cancel_code();
                json!({ "ok": true })
            }
            _ => json!({ "ok": false, "error": "unknown command" }),
        }
    }

    /// Renames the receiver as the iPhone sees it; returns the cleaned-up name.
    fn rename(&self, name: &str) -> Result<String, String> {
        let name = settings::clean_name(name).map_err(str::to_string)?;
        self.receiver.rename(&name).map_err(|err| format!("Couldn't rename: {err}"))?;
        if self.persist_name {
            self.settings.update(|s| s.name = name.clone());
        }
        self.hub.set_receiver(name.clone());
        // LocalSend devices see the same name.
        let files = self.files.clone();
        let alias = name.clone();
        tauri::async_runtime::spawn(async move { files.set_alias(alias).await });
        Ok(name)
    }
}

impl App {
    /// While mirroring, the window turns into just the phone: transparent, no
    /// shadow or edge, and sized to the mirrored screen. Off puts it back.
    fn phone_frame(&self, on: bool, aspect: f64) -> tauri::Result<()> {
        let Some(window) = &self.window else { return Ok(()) };
        let mut phone = self.phone.lock().unwrap_or_else(|e| e.into_inner());
        if !on {
            if let Some(restore) = phone.take() {
                if window.is_fullscreen()? {
                    window.set_fullscreen(false)?;
                }
                window.set_min_size(Some(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1)))?;
                window.set_size(restore.size)?;
                window.set_position(restore.position)?;
                clip_window(window, None);
                set_window_border(window, true);
                if cfg!(windows) {
                    window.set_resizable(true)?;
                }
                window.set_shadow(true)?;
                window.set_title("RustPlay")?;
                apply_appearance(window, self.settings.get().appearance);
            }
            return Ok(());
        }

        window.set_shadow(false)?;
        window.set_background_color(Some(tauri::window::Color(0, 0, 0, 0)))?;
        set_window_border(window, false);
        // A stretched edge would leave window showing beside the phone.
        if cfg!(windows) {
            window.set_resizable(false)?;
        }
        // Window managers can match this title to drop their own edge (e.g. a niri focus ring).
        window.set_title(PHONE_TITLE)?;
        // Full screen shows the phone on black; it reshapes once full screen ends.
        if window.is_fullscreen()? {
            return Ok(());
        }
        if window.is_maximized()? {
            window.unmaximize()?;
        }
        let Some(monitor) = window.current_monitor()?.or(window.primary_monitor()?) else { return Ok(()) };
        let area = monitor.work_area();
        let (area_x, area_y) = (area.position.x as f64, area.position.y as f64);
        let (area_w, area_h) = (area.size.width as f64, area.size.height as f64);
        let margin = PHONE_MARGIN * window.scale_factor()?;
        let position = window.outer_position()?;
        let size = window.outer_size()?;
        let center = (position.x as f64 + size.width as f64 / 2.0, position.y as f64 + size.height as f64 / 2.0);

        // The screen's long side: most of the display's height when it first
        // connects, and whatever it already is when the phone turns.
        let long = if phone.is_some() {
            size.width.max(size.height) as f64 - margin * 2.0
        } else {
            area_h * 0.84 - margin * 2.0
        };
        let (mut w, mut h) = if aspect < 1.0 { (long * aspect, long) } else { (long, long / aspect) };
        let fit = ((area_w * 0.95 - margin * 2.0) / w).min((area_h * 0.95 - margin * 2.0) / h).min(1.0);
        w = (w * fit).max(80.0);
        h = (h * fit).max(80.0);
        let (outer_w, outer_h) = (w + margin * 2.0, h + margin * 2.0);
        let x = (center.0 - outer_w / 2.0).clamp(area_x, (area_x + area_w - outer_w).max(area_x));
        let y = (center.1 - outer_h / 2.0).clamp(area_y, (area_y + area_h - outer_h).max(area_y));

        if phone.is_none() {
            *phone = Some(Restore { position, size: window.inner_size()? });
        }
        window.set_min_size(Some(LogicalSize::new(120.0, 120.0)))?;
        window.set_size(PhysicalSize::new(outer_w.round() as u32, outer_h.round() as u32))?;
        window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))?;
        Ok(())
    }
}

/// The mirrored screen's rounded rectangle, in CSS pixels from the window's top left.
#[cfg_attr(not(windows), allow(dead_code))]
struct Shape {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
}

/// Cuts the window down to the phone's screen (None: the whole window again).
///
/// Windows keeps a native frame behind borderless windows, and once the page is
/// transparent its frosted glass and minimize/maximize/close buttons show through.
/// A window region removes everything outside the phone, frame included.
/// Other platforms draw nothing behind a transparent window, so there's nothing to cut.
#[cfg(windows)]
fn clip_window(window: &tauri::WebviewWindow, shape: Option<Shape>) {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{ClientToScreen, CreateRoundRectRgn, SetWindowRgn};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let Ok(hwnd) = window.hwnd() else { return };
    let scale = window.scale_factor().unwrap_or(1.0);
    // HWND isn't Send; the region is set on the window's own thread.
    let raw = hwnd.0 as isize;
    let _ = window.run_on_main_thread(move || unsafe {
        let hwnd = HWND(raw as *mut _);
        let Some(shape) = shape else {
            SetWindowRgn(hwnd, None, true);
            return;
        };
        // Regions are relative to the window's outer corner, the page to its client area.
        let mut outer = RECT::default();
        let mut origin = POINT::default();
        if GetWindowRect(hwnd, &mut outer).is_err() || !ClientToScreen(hwnd, &mut origin).as_bool() {
            return;
        }
        let (dx, dy) = ((origin.x - outer.left) as f64, (origin.y - outer.top) as f64);
        let px = |v: f64| (v * scale).round() as i32;
        let (left, top) = (px(shape.x) + dx as i32, px(shape.y) + dy as i32);
        let (right, bottom) = (left + px(shape.w), top + px(shape.h));
        let corner = px(shape.r * 2.0);
        // The region's right and bottom edges are exclusive.
        let region = CreateRoundRectRgn(left, top, right + 1, bottom + 1, corner, corner);
        // On success the window owns the region and frees it.
        SetWindowRgn(hwnd, Some(region), true);
    });
}

#[cfg(not(windows))]
fn clip_window(_window: &tauri::WebviewWindow, _shape: Option<Shape>) {}

/// Windows 11 outlines every window with a thin border; around the phone it
/// would read as a frame. Older Windows has no such border and ignores this.
#[cfg(windows)]
fn set_window_border(window: &tauri::WebviewWindow, on: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_COLOR_NONE, DwmSetWindowAttribute,
    };

    let Ok(hwnd) = window.hwnd() else { return };
    let raw = hwnd.0 as isize;
    let _ = window.run_on_main_thread(move || unsafe {
        let color: u32 = if on { DWMWA_COLOR_DEFAULT } else { DWMWA_COLOR_NONE };
        let _ = DwmSetWindowAttribute(
            HWND(raw as *mut _),
            DWMWA_BORDER_COLOR,
            (&color as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    });
}

#[cfg(not(windows))]
fn set_window_border(_window: &tauri::WebviewWindow, _on: bool) {}

struct Endpoint(String);

#[tauri::command]
fn stream_endpoint(endpoint: tauri::State<'_, Endpoint>) -> String {
    endpoint.0.clone()
}

/// The window's theme decides the page's first paint (through prefers-color-scheme),
/// so a chosen appearance shows from the first frame, and the window's own
/// background keeps resizes and page loads from flashing the other color.
fn apply_appearance(window: &tauri::WebviewWindow, appearance: Appearance) {
    let theme = match appearance {
        Appearance::Auto => None,
        Appearance::Light => Some(tauri::Theme::Light),
        Appearance::Dark => Some(tauri::Theme::Dark),
    };
    let _ = window.set_theme(theme);
    let color = match window.theme() {
        Ok(tauri::Theme::Light) => tauri::window::Color(0xf5, 0xf5, 0xf7, 0xff),
        _ => tauri::window::Color(0x1e, 0x1e, 0x1e, 0xff),
    };
    let _ = window.set_background_color(Some(color));
}

/// Release builds have no console, so they log to a file beside the settings.
fn init_logging(config_dir: &Path, second_copy: bool) {
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if cfg!(not(debug_assertions)) {
        let name = if second_copy { "rustplay-dev.log" } else { "rustplay.log" };
        let file = std::fs::create_dir_all(config_dir).and_then(|()| std::fs::File::create(config_dir.join(name)));
        if let Ok(file) = file {
            logger.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    logger.init();
}

fn main() {
    // RUSTPLAY_NAME runs a separate copy beside the installed one.
    let second_copy = std::env::var("RUSTPLAY_NAME").ok();
    let mut builder = tauri::Builder::default();
    if second_copy.is_none() {
        // Two copies would both pose as a Bluetooth mouse and knock each other's
        // iPad off, so launching again just brings the open window forward.
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let file = if second_copy.is_some() { "settings-dev.json" } else { "settings.json" };
            let config_dir = app.path().app_config_dir()?;
            init_logging(&config_dir, second_copy.is_some());
            let settings = settings::Store::load(config_dir.join(file));
            let saved = settings.get();
            let name = second_copy.clone().unwrap_or(saved.name);
            let instance = second_copy.as_deref().unwrap_or(settings::DEFAULT_NAME);

            let address = airplay::primary_ipv4();
            let output = Output::start(saved.volume);
            let hub = Arc::new(Hub::new(name.clone(), address.map(|ip| ip.to_string()), saved.volume));
            let receiver = tauri::async_runtime::block_on(airplay::start(
                name.clone(),
                instance,
                address,
                hub.clone(),
                output.clone(),
                saved.require_code,
            ))?;

            // Received files go straight to Downloads.
            let downloads = app.path().download_dir()?;
            let identity_file =
                config_dir.join(if second_copy.is_some() { "localsend-dev.pem" } else { "localsend.pem" });
            let files =
                tauri::async_runtime::block_on(drop::FileDrop::start(name, downloads, &identity_file, hub.clone()))?;

            let remote = remote::Remote::start(hub.clone());
            app.manage(remote.clone());
            let updates = updates::Updates::start(app.handle().clone(), hub.clone());

            let window = app.get_webview_window("main");
            if let Some(window) = &window {
                apply_appearance(window, saved.appearance);
            }

            let persist_name = second_copy.is_none();
            let state = Arc::new(App {
                hub: hub.clone(),
                output,
                receiver,
                files,
                remote,
                updates,
                settings,
                window: window.clone(),
                persist_name,
                phone: Mutex::new(None),
            });
            let commands: ui_socket::Commands = Arc::new(move |request| state.command(request));
            let endpoint = tauri::async_runtime::block_on(ui_socket::start(hub, commands))?;
            app.manage(Endpoint(endpoint));

            // The window starts hidden (tauri.conf.json); RUSTPLAY_HIDE keeps it that
            // way, for running the backend in automated tests.
            if std::env::var_os("RUSTPLAY_HIDE").is_none() {
                if let Some(window) = window {
                    window.show()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![stream_endpoint])
        .build(tauri::generate_context!())
        .expect("error while starting RustPlay")
        .run(|app, event| {
            // Otherwise the iPad stays linked to the PC after RustPlay closes.
            if let tauri::RunEvent::Exit = event {
                app.state::<Arc<remote::Remote>>().shutdown();
            }
        });
}

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod airplay;
mod drop;
mod remote;
mod settings;
mod ui_socket;

use std::path::Path;
use std::sync::Arc;

use airplay::audio::Output;
use airplay::hub::Hub;
use serde_json::{Value, json};
use settings::Appearance;
use tauri::Manager;

struct App {
    hub: Arc<Hub>,
    output: Arc<Output>,
    receiver: airplay::Running,
    files: Arc<drop::FileDrop>,
    remote: Arc<remote::Remote>,
    settings: settings::Store,
    window: Option<tauri::WebviewWindow>,
    /// False for a second copy of the app (RUSTPLAY_NAME), which keeps the name it was given.
    persist_name: bool,
}

impl App {
    /// Commands from the window, arriving over the UI socket.
    fn command(&self, request: &Value) -> Value {
        match request["type"].as_str() {
            Some(kind) if kind.starts_with("drop-") => self.files.command(request),
            Some(kind) if kind.starts_with("remote-") => self.remote.command(request),
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
                settings,
                window: window.clone(),
                persist_name,
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

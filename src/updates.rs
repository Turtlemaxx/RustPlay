//! Looks for a newer RustPlay on GitHub and, when the user says so, installs it
//! and restarts. The window shows the prompt; updates arrive as signed installers
//! (tauri.conf.json holds the public key), so a tampered download is refused.
//!
//! The window hears about it as `{"type": "update", "state": ...}`:
//! `none`, `available` (with `version` and `notes`), `downloading` (with
//! `progress` from 0 to 1, or null while the size is unknown), `installing`,
//! and `failed` (with `error`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tauri::AppHandle;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::airplay::hub::Hub;

/// First look shortly after launch, then a few times a day for PCs left running.
const FIRST_CHECK: Duration = Duration::from_secs(8);
const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

pub struct Updates {
    app: AppHandle,
    hub: Arc<Hub>,
    found: Mutex<Option<Update>>,
    /// The last thing the window was told, for a window that (re)connects.
    status: Mutex<Value>,
}

impl Updates {
    pub fn start(app: AppHandle, hub: Arc<Hub>) -> Arc<Updates> {
        let updates = Arc::new(Updates {
            app,
            hub,
            found: Mutex::new(None),
            status: Mutex::new(json!({ "type": "update", "state": "none" })),
        });
        if supported() {
            let looper = updates.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(FIRST_CHECK).await;
                loop {
                    looper.check().await;
                    tokio::time::sleep(RECHECK).await;
                }
            });
        } else {
            log::info!("not looking for updates: this copy wasn't installed from a release");
        }
        updates
    }

    pub fn command(self: &Arc<Self>, request: &Value) -> Value {
        match request["type"].as_str() {
            Some("update-status") => {
                let mut status = self.status.lock().unwrap().clone();
                status["ok"] = json!(true);
                status
            }
            Some("update-install") => {
                let Some(update) = self.found.lock().unwrap().clone() else {
                    return json!({ "ok": false, "error": "There's no update to install." });
                };
                let this = self.clone();
                tauri::async_runtime::spawn(async move { this.install(update).await });
                json!({ "ok": true })
            }
            Some("update-check") => {
                let this = self.clone();
                tauri::async_runtime::spawn(async move { this.check().await });
                json!({ "ok": true })
            }
            _ => json!({ "ok": false, "error": "unknown command" }),
        }
    }

    async fn check(&self) {
        if !supported() {
            return;
        }
        // Don't interrupt an install in progress with a fresh check.
        let state = self.status.lock().unwrap()["state"].as_str().unwrap_or_default().to_string();
        if state == "downloading" || state == "installing" {
            return;
        }
        let result = match self.app.updater() {
            Ok(updater) => updater.check().await,
            Err(err) => Err(err),
        };
        match result {
            Ok(Some(update)) => {
                log::info!("RustPlay {} is available", update.version);
                self.publish(json!({
                    "state": "available",
                    "version": update.version,
                    "notes": update.body,
                }));
                *self.found.lock().unwrap() = Some(update);
            }
            Ok(None) => log::debug!("RustPlay is up to date"),
            // Offline, GitHub unreachable, or no release for this kind of install yet.
            Err(err) => log::info!("couldn't check for updates: {err}"),
        }
    }

    async fn install(&self, update: Update) {
        let version = update.version.clone();
        self.publish(json!({ "state": "downloading", "version": version, "progress": 0.0 }));
        let mut received = 0usize;
        let mut last_sent = -1i64;
        let result = update
            .download_and_install(
                |chunk, total| {
                    received += chunk;
                    let progress = total.map(|total| received as f64 / total.max(1) as f64);
                    // A whole percent at a time is plenty for the window.
                    let percent = progress.map_or(-2, |p| (p * 100.0) as i64);
                    if percent != last_sent {
                        last_sent = percent;
                        self.publish(json!({ "state": "downloading", "version": version, "progress": progress }));
                    }
                },
                || self.publish(json!({ "state": "installing", "version": version })),
            )
            .await;
        match result {
            // On Windows the installer has taken over and RustPlay has already quit.
            Ok(()) => {
                log::info!("installed RustPlay {version}, restarting");
                self.app.restart();
            }
            Err(err) => {
                log::warn!("update to {version} failed: {err}");
                self.publish(json!({
                    "state": "failed",
                    "version": version,
                    "error": format!("Couldn't install the update: {err}"),
                }));
            }
        }
    }

    fn publish(&self, mut status: Value) {
        status["type"] = json!("update");
        *self.status.lock().unwrap() = status.clone();
        self.hub.notify(status);
    }
}

/// Releases can update themselves. A copy built from source on Linux can't: the
/// update would overwrite it with an AppImage. On Windows even the portable exe
/// is offered the installer.
fn supported() -> bool {
    cfg!(windows) || tauri::utils::platform::bundle_type().is_some()
}

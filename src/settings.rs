//! User settings (receiver name, volume), kept as JSON in the app's config folder.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const DEFAULT_NAME: &str = "RustPlay";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    pub volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { name: DEFAULT_NAME.into(), volume: 0.8 }
    }
}

pub struct Store {
    path: PathBuf,
    current: Mutex<Settings>,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let current =
            std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        Store { path, current: Mutex::new(current) }
    }

    pub fn get(&self) -> Settings {
        self.current.lock().unwrap().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut Settings)) {
        let mut current = self.current.lock().unwrap();
        change(&mut current);
        let saved = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&self.path, serde_json::to_string_pretty(&*current).unwrap()));
        if let Err(err) = saved {
            log::warn!("could not save settings to {}: {err}", self.path.display());
        }
    }
}

/// A receiver name the iPhone can show: trimmed, 1–40 characters, and free of
/// the characters that separate parts of Bonjour names.
pub fn clean_name(input: &str) -> Result<String, &'static str> {
    let name = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Enter a name.");
    }
    if name.chars().count() > 40 {
        return Err("Use 40 characters or fewer.");
    }
    if name.chars().any(|c| c == '.' || c == '@' || c.is_control()) {
        return Err("Names can't contain . or @");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_receiver_names() {
        assert_eq!(clean_name("  Living   Room TV ").unwrap(), "Living Room TV");
        assert_eq!(clean_name("Max's PC").unwrap(), "Max's PC");
        assert!(clean_name("   ").is_err());
        assert!(clean_name("a.b").is_err());
        assert!(clean_name(&"x".repeat(41)).is_err());
    }
}

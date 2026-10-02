//! Settings persisted in `~/.config/ochre/config.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::annot::model::Style;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Last used style per tool (keyed by `Tool::key`).
    pub styles: BTreeMap<String, Style>,
    /// Stroke stabilizer radius in screen pixels (at least 3).
    pub stabilizer: f32,
    pub recent: Vec<PathBuf>,
    /// Custom colors added to the palette.
    pub palette: Vec<[f32; 3]>,
}

impl Default for Config {
    fn default() -> Self {
        Self { styles: BTreeMap::new(), stabilizer: 3.0, recent: Vec::new(), palette: Vec::new() }
    }
}

#[cfg(test)]
fn path() -> Option<PathBuf> {
    None // tests never touch the user's config
}

#[cfg(not(test))]
fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ochre").map(|d| d.config_dir().join("config.toml"))
}

/// Settings from before the app was renamed (InkPDF), used when Ochre has none yet.
#[cfg(not(test))]
fn legacy_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "inkpdf").map(|d| d.config_dir().join("config.toml"))
}

#[cfg(test)]
fn legacy_path() -> Option<PathBuf> {
    None
}

impl Config {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .or_else(|| legacy_path().and_then(|p| std::fs::read_to_string(p).ok()))
            .and_then(|s| toml::from_str::<Config>(&s).ok())
            .map(|mut c| {
                // Older versions allowed weaker smoothing; 3 px is now the minimum.
                c.stabilizer = c.stabilizer.max(crate::ui::chrome::MIN_STABILIZER);
                c
            })
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }

    pub fn add_recent(&mut self, p: PathBuf) {
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(10);
    }
}

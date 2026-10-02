//! Settings persisted in `~/.config/ochre/config.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::annot::model::Style;
use crate::viewer::Fit;

/// Where a file was left: the page at the top of the window and how far down it
/// was (display points), plus the zoom.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadPos {
    pub path: PathBuf,
    pub page: usize,
    pub y: f32,
    pub zoom: f32,
    pub fit: Option<Fit>,
}

/// Which list the sidebar shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarTab {
    #[default]
    Contents,
    Annotations,
    Pages,
}

/// How many files' reading positions are kept.
const MAX_POSITIONS: usize = 200;

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
    /// Whether the sidebar is shown (the name predates its Annotations tab).
    pub show_outline: bool,
    pub sidebar_tab: SidebarTab,
    /// Reading position per file, most recent first.
    pub positions: Vec<ReadPos>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            styles: BTreeMap::new(),
            stabilizer: crate::ui::chrome::MIN_STABILIZER,
            recent: Vec::new(),
            palette: Vec::new(),
            show_outline: false,
            sidebar_tab: SidebarTab::Contents,
            positions: Vec::new(),
        }
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
                // Older versions allowed weaker smoothing; 9 px is now the minimum.
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

    pub fn position(&self, path: &std::path::Path) -> Option<&ReadPos> {
        self.positions.iter().find(|p| p.path == path)
    }

    pub fn set_position(&mut self, pos: ReadPos) {
        self.positions.retain(|p| p.path != pos.path);
        self.positions.insert(0, pos);
        self.positions.truncate(MAX_POSITIONS);
    }

    pub fn add_recent(&mut self, p: PathBuf) {
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(10);
    }
}

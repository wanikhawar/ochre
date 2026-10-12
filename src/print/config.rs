//! Print settings: per-printer defaults, presets, window preferences.
//!
//! Stored as JSON in `~/.config/ochre/print.json`, in Print Studio's format.
//! The first time, Print Studio's settings are copied over if it's installed, so
//! presets and two-sided calibration carry across.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::print::pipeline::JobOptions;

/// Never carried over from one document to the next.
const PER_DOCUMENT: [&str; 2] = ["page_ranges", "copies"];
/// Chosen for one document at a time, but worth keeping in a preset.
const PER_JOB: [&str; 1] = ["booklet"];
/// Describe how the printer handles paper, so presets leave them alone.
const PER_PRINTER: [&str; 2] = ["backs_reverse", "backs_rotate"];

pub type PpdValues = HashMap<String, String>;

#[cfg(not(test))]
fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ochre").map(|d| d.config_dir().join("print.json"))
}

#[cfg(test)]
fn config_path() -> Option<PathBuf> {
    None // tests never touch the user's settings
}

/// Print Studio's settings file.
#[cfg(not(test))]
fn print_studio_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "printstudio").map(|d| d.config_dir().join("config.json"))
}

#[cfg(test)]
fn print_studio_path() -> Option<PathBuf> {
    None
}

#[derive(Clone)]
pub struct Config {
    pub path: PathBuf,
    data: Map<String, Value>,
}

fn job_map(job: &JobOptions, skip: &[&str]) -> Value {
    let mut v = serde_json::to_value(job).unwrap_or_default();
    if let Some(map) = v.as_object_mut() {
        for k in skip {
            map.remove(*k);
        }
    }
    v
}

fn ppd_values(v: Option<&Value>) -> PpdValues {
    v.and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default()
}

/// Apply stored (possibly partial) job fields on top of `base`.
pub fn merge_job(base: &JobOptions, fields: &Map<String, Value>) -> JobOptions {
    let mut v = serde_json::to_value(base).unwrap_or_default();
    if let Some(map) = v.as_object_mut() {
        for (k, val) in fields {
            if map.contains_key(k) {
                map.insert(k.clone(), val.clone());
            }
        }
    }
    serde_json::from_value(v).unwrap_or_else(|_| base.clone())
}

impl Config {
    /// Ochre's print settings, or Print Studio's the first time. Without a
    /// settings folder (or in tests) nothing is saved.
    pub fn load() -> Config {
        let Some(path) = config_path() else { return Config::load_from(PathBuf::new()) };
        if !path.exists()
            && let Some(theirs) = print_studio_path().filter(|p| p.exists())
        {
            return Config { path, ..Config::load_from(theirs) };
        }
        Config::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Config {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let mut cfg = Config { path, data };
        for key in ["printers", "presets", "ui"] {
            if !cfg.data.get(key).is_some_and(Value::is_object) {
                cfg.data.insert(key.into(), json!({}));
            }
        }
        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let dir = self.path.parent().ok_or("bad config path")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(&self.data).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)).map_err(|e| e.to_string())
    }

    fn section(&mut self, key: &str) -> &mut Map<String, Value> {
        self.data.get_mut(key).and_then(Value::as_object_mut).expect("section exists")
    }

    fn printer_entry(&mut self, printer: &str) -> &mut Map<String, Value> {
        let printers = self.section("printers");
        if !printers.get(printer).is_some_and(Value::is_object) {
            printers.insert(printer.into(), json!({}));
        }
        printers.get_mut(printer).and_then(Value::as_object_mut).unwrap()
    }

    pub fn last_printer(&self) -> Option<String> {
        self.data.get("last_printer")?.as_str().map(str::to_string)
    }

    pub fn set_last_printer(&mut self, name: &str) {
        self.data.insert("last_printer".into(), json!(name));
    }

    // Per-printer defaults

    pub fn printer_settings(&self, printer: &str) -> (JobOptions, PpdValues) {
        let entry = self.data["printers"].get(printer);
        let job = entry
            .and_then(|e| e.get("job"))
            .and_then(Value::as_object)
            .map(|m| merge_job(&JobOptions::default(), m))
            .unwrap_or_default();
        (job, ppd_values(entry.and_then(|e| e.get("ppd"))))
    }

    pub fn set_printer_settings(&mut self, printer: &str, job: &JobOptions, ppd: &PpdValues) {
        let entry = self.printer_entry(printer);
        let skip: Vec<&str> = PER_DOCUMENT.iter().chain(PER_JOB.iter()).copied().collect();
        entry.insert("job".into(), job_map(job, &skip));
        entry.insert("ppd".into(), json!(ppd));
    }

    pub fn printer_note(&self, printer: &str) -> String {
        self.data["printers"]
            .get(printer)
            .and_then(|e| e.get("duplex_note"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .into()
    }

    pub fn set_printer_note(&mut self, printer: &str, note: &str) {
        self.printer_entry(printer).insert("duplex_note".into(), json!(note));
    }

    // Presets

    pub fn preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.data["presets"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// The stored (partial) job fields and PPD options.
    pub fn preset(&self, name: &str) -> (Map<String, Value>, PpdValues) {
        let entry = self.data["presets"].get(name);
        let job = entry.and_then(|e| e.get("job")).and_then(Value::as_object).cloned().unwrap_or_default();
        (job, ppd_values(entry.and_then(|e| e.get("ppd"))))
    }

    pub fn set_preset(&mut self, name: &str, job: &JobOptions, ppd: &PpdValues) {
        let skip: Vec<&str> = PER_DOCUMENT.iter().chain(PER_PRINTER.iter()).copied().collect();
        self.section("presets").insert(name.into(), json!({ "job": job_map(job, &skip), "ppd": ppd }));
    }

    pub fn delete_preset(&mut self, name: &str) {
        self.section("presets").remove(name);
    }

    // Window preferences

    pub fn ui(&self, key: &str) -> Option<&Value> {
        self.data["ui"].get(key)
    }

    pub fn set_ui(&mut self, key: &str, value: Value) {
        self.section("ui").insert(key.into(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("ochre-print-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Config::load_from(dir.join("config.json"))
    }

    #[test]
    fn printer_settings_round_trip_without_per_document_fields() {
        let mut cfg = temp_config("printer");
        let job = JobOptions {
            copies: 5,
            page_ranges: "1-2".into(),
            nup: 4,
            backs_rotate: true,
            booklet: true,
            ..Default::default()
        };
        let ppd = PpdValues::from([("BRResolution".to_string(), "PlainFast".to_string())]);
        cfg.set_printer_settings("P", &job, &ppd);
        cfg.save().unwrap();
        let (loaded, ppd2) = Config::load_from(cfg.path.clone()).printer_settings("P");
        assert_eq!((loaded.copies, loaded.page_ranges.as_str(), loaded.nup, loaded.backs_rotate), (1, "", 4, true));
        assert!(!loaded.booklet, "a booklet is a choice for one document, not a printer default");
        assert_eq!(ppd2["BRResolution"], "PlainFast");
    }

    #[test]
    fn presets_leave_printer_handling_alone() {
        let mut cfg = temp_config("preset");
        let job = JobOptions { nup: 2, backs_rotate: true, booklet: true, ..Default::default() };
        cfg.set_preset("Draft", &job, &PpdValues::new());
        let (fields, _) = cfg.preset("Draft");
        assert!(!fields.contains_key("backs_rotate") && !fields.contains_key("copies"));
        assert_eq!(fields["booklet"], true, "presets can make booklets");
        let base = JobOptions { backs_rotate: false, ..Default::default() };
        let merged = merge_job(&base, &fields);
        assert_eq!((merged.nup, merged.backs_rotate), (2, false));
    }
}

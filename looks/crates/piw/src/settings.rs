// OWNER: selectors (settings values and the file they live in)
//! piw's own settings, the subset of Pi's `settings.json` the wizard backend leaves to the
//! frontend (`docs/piw-spec.md` 14.3). Read from `<config dir>/settings.json`, written back when a
//! value changes in `/settings`. Unknown keys are kept.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub theme: Option<String>,
    pub hide_thinking_block: bool,
    pub output_padding: u16,
    pub editor_padding_x: u16,
    pub autocomplete_max_visible: usize,
    /// `false`, `true`, or `"header"`: how much of the startup header to show.
    pub quiet_startup: Quiet,
    pub collapse_changelog: bool,
    pub show_hardware_cursor: bool,
    /// Model ids the cycle keys visit; empty means all.
    pub enabled_models: Vec<String>,
    pub fullscreen_scrollbar: Scrollbar,
    pub fullscreen_copy_on_select: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    Off,
    Header,
    On,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrollbar {
    Auto,
    Always,
    Hidden,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: None,
            hide_thinking_block: false,
            output_padding: 1,
            editor_padding_x: 0,
            autocomplete_max_visible: 5,
            quiet_startup: Quiet::Off,
            collapse_changelog: false,
            show_hardware_cursor: false,
            enabled_models: Vec::new(),
            fullscreen_scrollbar: Scrollbar::Auto,
            fullscreen_copy_on_select: true,
        }
    }
}

pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(s).join("piw"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/piw")))
}

impl Settings {
    pub fn from_json(v: &Value) -> Settings {
        let mut s = Settings::default();
        let get = |k: &str| v.get(k);
        s.theme = get("theme").and_then(Value::as_str).map(String::from);
        if let Some(b) = get("hideThinkingBlock").and_then(Value::as_bool) {
            s.hide_thinking_block = b;
        }
        if let Some(n) = get("outputPadding").and_then(Value::as_u64) {
            s.output_padding = n.min(1) as u16;
        }
        if let Some(n) = get("editorPaddingX").and_then(Value::as_u64) {
            s.editor_padding_x = n.min(3) as u16;
        }
        if let Some(n) = get("autocompleteMaxVisible").and_then(Value::as_u64) {
            s.autocomplete_max_visible = n.clamp(3, 20) as usize;
        }
        s.quiet_startup = match get("quietStartup") {
            Some(Value::Bool(true)) => Quiet::On,
            Some(Value::String(h)) if h == "header" => Quiet::Header,
            _ => Quiet::Off,
        };
        if let Some(b) = get("collapseChangelog").and_then(Value::as_bool) {
            s.collapse_changelog = b;
        }
        if let Some(b) = get("showHardwareCursor").and_then(Value::as_bool) {
            s.show_hardware_cursor = b;
        }
        if let Some(a) = get("enabledModels").and_then(Value::as_array) {
            s.enabled_models = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
        }
        s.fullscreen_scrollbar = match get("fullscreenScrollbar").and_then(Value::as_str) {
            Some("always") => Scrollbar::Always,
            Some("hidden") => Scrollbar::Hidden,
            _ => Scrollbar::Auto,
        };
        if let Some(b) = get("fullscreenCopyOnSelect").and_then(Value::as_bool) {
            s.fullscreen_copy_on_select = b;
        }
        s
    }

    pub fn load(dir: &Path) -> Settings {
        std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| Settings::from_json(&v))
            .unwrap_or_default()
    }

    /// Write the values over whatever else the file holds.
    pub fn save(&self, dir: &Path) {
        let path = dir.join("settings.json");
        let old = std::fs::read(&path).ok();
        let mut m: Map<String, Value> = old
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();
        // A file that exists but does not parse (a hand edit with a trailing comma) is not ours
        // to overwrite with a file of defaults: keep it beside the new one.
        if let Some(b) = old.as_deref().filter(|b| {
            !b.iter().all(u8::is_ascii_whitespace)
                && serde_json::from_slice::<Map<String, Value>>(b).is_err()
        }) {
            let _ = tuikit::private::write_atomic(&dir.join("settings.json.bad"), b);
        }
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        match &self.theme {
            Some(t) => put("theme", Value::String(t.clone())),
            None => {
                m.remove("theme");
            }
        }
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        put("hideThinkingBlock", self.hide_thinking_block.into());
        put("outputPadding", self.output_padding.into());
        put("editorPaddingX", self.editor_padding_x.into());
        put(
            "autocompleteMaxVisible",
            self.autocomplete_max_visible.into(),
        );
        put(
            "quietStartup",
            match self.quiet_startup {
                Quiet::Off => Value::Bool(false),
                Quiet::On => Value::Bool(true),
                Quiet::Header => Value::String("header".into()),
            },
        );
        put("collapseChangelog", self.collapse_changelog.into());
        put("showHardwareCursor", self.show_hardware_cursor.into());
        put(
            "enabledModels",
            Value::Array(
                self.enabled_models
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        put(
            "fullscreenScrollbar",
            match self.fullscreen_scrollbar {
                Scrollbar::Auto => "auto",
                Scrollbar::Always => "always",
                Scrollbar::Hidden => "hidden",
            }
            .into(),
        );
        put(
            "fullscreenCopyOnSelect",
            self.fullscreen_copy_on_select.into(),
        );
        if let Ok(t) = serde_json::to_string_pretty(&Value::Object(m)) {
            let _ = tuikit::private::write_atomic(&path, (t + "\n").as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_roundtrip() {
        let d = std::env::temp_dir().join(format!("piw-settings-{}", std::process::id()));
        let mut s = Settings::default();
        assert_eq!(Settings::load(&d), s);
        s.hide_thinking_block = true;
        s.quiet_startup = Quiet::Header;
        s.enabled_models = vec!["a/b".into()];
        s.save(&d);
        assert_eq!(Settings::load(&d), s);
        let _ = std::fs::remove_dir_all(&d);
    }
}

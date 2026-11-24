//! Configuration loading and validation for wavepill.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Minimum allowed number of spectrum visualizer bars.
pub const MIN_BAR_COUNT: usize = 4;
/// Maximum allowed number of spectrum visualizer bars.
pub const MAX_BAR_COUNT: usize = 32;

const DEFAULT_MARGIN_X: i32 = 16;
const DEFAULT_MARGIN_Y: i32 = 48;
const DEFAULT_HIDE_DELAY_MS: u64 = 3000;
const DEFAULT_BAR_COUNT: usize = 12;
const DEFAULT_ACCENT_COLOR: &str = "#7aa2f7";

/// Screen corner where the floating wavepill window is anchored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

/// Application configuration loaded from `~/.config/wavepill/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub corner: Corner,
    pub margin_x: i32,
    pub margin_y: i32,
    pub hide_delay_ms: u64,
    pub bar_count: usize,
    pub accent_color: String,
    pub ignore_apps: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            corner: Corner::default(),
            margin_x: DEFAULT_MARGIN_X,
            margin_y: DEFAULT_MARGIN_Y,
            hide_delay_ms: DEFAULT_HIDE_DELAY_MS,
            bar_count: DEFAULT_BAR_COUNT,
            accent_color: DEFAULT_ACCENT_COLOR.to_owned(),
            ignore_apps: Vec::new(),
        }
    }
}

impl Config {
    /// Parses a TOML configuration string, filling omitted fields with defaults
    /// and clamping `bar_count` to `4..=32`.
    pub fn parse_toml(raw: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(raw)?;
        config.bar_count = config.bar_count.clamp(MIN_BAR_COUNT, MAX_BAR_COUNT);
        Ok(config)
    }

    /// Returns the default configuration file path (`~/.config/wavepill/config.toml`).
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("wavepill").join("config.toml"))
    }

    /// Loads configuration from `~/.config/wavepill/config.toml` if present,
    /// falling back to defaults (and logging a warning on read/parse errors).
    pub fn load_default() -> Self {
        match Self::default_path() {
            Some(path) => Self::load_from_path(&path),
            None => Self::default(),
        }
    }

    /// Loads configuration from an explicit path, falling back to defaults if
    /// missing or invalid.
    pub fn load_from_path(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match Self::parse_toml(&contents) {
                Ok(config) => config,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to parse wavepill config; falling back to defaults"
                    );
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to read wavepill config; falling back to defaults"
                );
                Self::default()
            }
        }
    }

    /// Returns the hide delay as a `Duration`.
    pub fn hide_delay(&self) -> Duration {
        Duration::from_millis(self.hide_delay_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("wavepill_test_{pid}_{id}_{name}"));
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn default_values_match_spec() {
        let cfg = Config::default();
        assert_eq!(cfg.corner, Corner::TopRight);
        assert_eq!(cfg.margin_x, 16);
        assert_eq!(cfg.margin_y, 48);
        assert_eq!(cfg.hide_delay_ms, 3000);
        assert_eq!(cfg.bar_count, 12);
        assert_eq!(cfg.accent_color, "#7aa2f7");
        assert!(cfg.ignore_apps.is_empty());
        assert_eq!(cfg.hide_delay(), Duration::from_millis(3000));
    }

    #[test]
    fn parse_full_toml_and_corners() {
        let raw = r##"
            corner = "bottom-left"
            margin_x = 24
            margin_y = 64
            hide_delay_ms = 1500
            bar_count = 20
            accent_color = "#bb9af7"
            ignore_apps = ["firefox", "chromium"]
        "##;

        let cfg = Config::parse_toml(raw).expect("valid config should parse");
        assert_eq!(cfg.corner, Corner::BottomLeft);
        assert_eq!(cfg.margin_x, 24);
        assert_eq!(cfg.margin_y, 64);
        assert_eq!(cfg.hide_delay_ms, 1500);
        assert_eq!(cfg.bar_count, 20);
        assert_eq!(cfg.accent_color, "#bb9af7");
        assert_eq!(cfg.ignore_apps, vec!["firefox", "chromium"]);

        for (toml_val, expected) in [
            ("top-right", Corner::TopRight),
            ("top-left", Corner::TopLeft),
            ("bottom-right", Corner::BottomRight),
            ("bottom-left", Corner::BottomLeft),
        ] {
            let parsed = Config::parse_toml(&format!("corner = \"{toml_val}\"")).unwrap();
            assert_eq!(parsed.corner, expected);
        }
    }

    #[test]
    fn parse_partial_toml_uses_defaults() {
        let empty = Config::parse_toml("").expect("empty config should parse");
        assert_eq!(empty, Config::default());

        let partial = Config::parse_toml("corner = \"top-left\"\nmargin_y = 12").unwrap();
        assert_eq!(partial.corner, Corner::TopLeft);
        assert_eq!(partial.margin_x, 16);
        assert_eq!(partial.margin_y, 12);
        assert_eq!(partial.hide_delay_ms, 3000);
        assert_eq!(partial.bar_count, 12);
        assert_eq!(partial.accent_color, "#7aa2f7");
    }

    #[test]
    fn parse_toml_clamps_bar_count() {
        assert_eq!(Config::parse_toml("bar_count = 0").unwrap().bar_count, 4);
        assert_eq!(Config::parse_toml("bar_count = 3").unwrap().bar_count, 4);
        assert_eq!(Config::parse_toml("bar_count = 4").unwrap().bar_count, 4);
        assert_eq!(Config::parse_toml("bar_count = 16").unwrap().bar_count, 16);
        assert_eq!(Config::parse_toml("bar_count = 32").unwrap().bar_count, 32);
        assert_eq!(Config::parse_toml("bar_count = 999").unwrap().bar_count, 32);
    }

    #[test]
    fn parse_toml_invalid_returns_error() {
        assert!(Config::parse_toml("corner = \"center\"").is_err());
        assert!(Config::parse_toml("bar_count = \"twelve\"").is_err());
        assert!(Config::parse_toml("not valid toml = [").is_err());
    }

    #[test]
    fn load_from_path_handles_missing_valid_and_invalid_files() {
        let missing = TempFile::new("missing.toml");
        assert_eq!(Config::load_from_path(&missing.path), Config::default());

        let valid = TempFile::new("valid.toml");
        std::fs::write(&valid.path, "corner = \"bottom-right\"\nbar_count = 64\n").unwrap();
        let loaded = Config::load_from_path(&valid.path);
        assert_eq!(loaded.corner, Corner::BottomRight);
        assert_eq!(loaded.bar_count, 32);
        assert_eq!(loaded.margin_x, 16);

        let invalid = TempFile::new("invalid.toml");
        std::fs::write(&invalid.path, "corner = [broken").unwrap();
        assert_eq!(Config::load_from_path(&invalid.path), Config::default());
    }
}

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_breaking_count")]
    pub breaking_count: usize,
    #[serde(default = "default_auto_refresh_secs")]
    pub auto_refresh_secs: u64,
    #[serde(default = "default_feeds")]
    pub feeds: Vec<FeedConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FeedConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "default_color")]
    pub color: String,
}

fn default_breaking_count() -> usize {
    3
}

fn default_auto_refresh_secs() -> u64 {
    300
}

fn default_color() -> String {
    "#888888".into()
}

fn default_feeds() -> Vec<FeedConfig> {
    vec![
        FeedConfig {
            name: "CNN".into(),
            url: "http://rss.cnn.com/rss/edition.rss".into(),
            color: "#8b2020".into(),
        },
        FeedConfig {
            name: "CNBC".into(),
            url: "https://search.cnbc.com/rs/search/combinedcms/view.xml?partnerId=wrss01&id=100003114".into(),
            color: "#1a6a8a".into(),
        },
        FeedConfig {
            name: "BBC".into(),
            url: "https://feeds.bbci.co.uk/news/rss.xml".into(),
            color: "#7a1515".into(),
        },
        FeedConfig {
            name: "Reuters".into(),
            url: "https://www.reutersagency.com/feed/".into(),
            color: "#a05a10".into(),
        },
        FeedConfig {
            name: "TechCrunch".into(),
            url: "https://techcrunch.com/feed/".into(),
            color: "#1a7a1a".into(),
        },
        FeedConfig {
            name: "AP News".into(),
            url: "https://rsshub.app/apnews/topics/apf-topnews".into(),
            color: "#8b2a2d".into(),
        },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            breaking_count: default_breaking_count(),
            auto_refresh_secs: default_auto_refresh_secs(),
            feeds: default_feeds(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(config) => return config,
                    Err(e) => eprintln!("Warning: failed to parse config: {e}, using defaults"),
                },
                Err(e) => eprintln!("Warning: failed to read config: {e}, using defaults"),
            }
        } else {
            // Write default config so user can edit it
            let config = Config::default();
            let _ = config.save();
            return config;
        }
        Config::default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| format!("Serialize error: {e}"))?;
        fs::write(&path, content).map_err(|e| format!("Write error: {e}"))?;
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("newsterm")
        .join("config.toml")
}

pub fn parse_hex_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        ) {
            return Color::Rgb(r, g, b);
        }
    }
    Color::Rgb(136, 136, 136)
}

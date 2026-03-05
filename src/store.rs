use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub read_links: HashSet<String>,
    pub bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub link: String,
    pub source: String,
    pub saved_at: String,
}

impl Store {
    pub fn load() -> Self {
        let path = store_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str(&content) {
                    return store;
                }
            }
        }
        Store::default()
    }

    pub fn save(&self) {
        let path = store_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&path, json);
        }
    }

    pub fn mark_read(&mut self, link: &str) {
        if self.read_links.insert(link.to_string()) {
            self.save();
        }
    }

    pub fn is_read(&self, link: &str) -> bool {
        self.read_links.contains(link)
    }

    pub fn toggle_bookmark(&mut self, title: &str, link: &str, source: &str) -> bool {
        if let Some(pos) = self.bookmarks.iter().position(|b| b.link == link) {
            self.bookmarks.remove(pos);
            self.save();
            false
        } else {
            self.bookmarks.push(Bookmark {
                title: title.to_string(),
                link: link.to_string(),
                source: source.to_string(),
                saved_at: chrono::Utc::now().to_rfc3339(),
            });
            self.save();
            true
        }
    }

    pub fn is_bookmarked(&self, link: &str) -> bool {
        self.bookmarks.iter().any(|b| b.link == link)
    }
}

fn store_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("newsterm")
        .join("store.json")
}

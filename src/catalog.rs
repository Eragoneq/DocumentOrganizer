use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Image,
    Document,
    Audio,
    Video,
    Project,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Document => "document",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Project => "project",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "image" => Self::Image,
            "document" => Self::Document,
            "audio" => Self::Audio,
            "video" => Self::Video,
            "project" => Self::Project,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub original_path: String,
    pub current_path: String,
    pub kind: Kind,
    pub project_id: Option<i64>,
    pub fingerprint: String,
    pub revision: i64,
    pub relevance: String,
    pub relevance_manual: Option<String>,
    pub category_id: i64,
    pub category_manual: bool,
    pub confidence: Option<f64>,
    pub status: String,
    pub missing: bool,
    pub block_reason: String,
    pub content: String,
    pub limitations: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub revision: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub revision: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemTag {
    pub tag_id: i64,
    pub name: String,
    pub probability: Option<f64>,
    pub manual: Option<bool>,
}
impl ItemTag {
    pub fn assigned(&self) -> bool {
        self.manual
            .unwrap_or(self.probability.unwrap_or(0.0) >= 0.5)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: i64,
    pub name: String,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exclusion {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub item_id: i64,
    pub revision: i64,
    pub kind: String,
    pub tags: Vec<Tag>,
    pub category_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub decision_url: String,
    #[serde(default = "default_decision_version")]
    pub decision_model_version: String,
    pub generative_url: String,
    pub generative_model: String,
    pub pdfium_library: String,
    pub destinations: BTreeMap<String, String>,
}
impl Default for Settings {
    fn default() -> Self {
        let mut destinations = BTreeMap::new();
        if let Some(dirs) = directories::UserDirs::new() {
            for (k, p) in [
                ("image", dirs.picture_dir()),
                ("document", dirs.document_dir()),
                ("audio", dirs.audio_dir()),
                ("video", dirs.video_dir()),
            ] {
                if let Some(p) = p {
                    destinations.insert(k.into(), p.to_string_lossy().into_owned());
                }
            }
            if let Some(p) = dirs.document_dir() {
                destinations.insert(
                    "project".into(),
                    p.join("Projects").to_string_lossy().into_owned(),
                );
                destinations.insert(
                    "other".into(),
                    p.join("Other").to_string_lossy().into_owned(),
                );
            }
        }
        Self {
            decision_url: "http://127.0.0.1:8080/v1/systemone".into(),
            decision_model_version: default_decision_version(),
            generative_url: "http://127.0.0.1:8081/v1/chat/completions".into(),
            generative_model: String::new(),
            pdfium_library: String::new(),
            destinations,
        }
    }
}
fn default_decision_version() -> String {
    "LiquidAI/d1-3B (server revision unreported)".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub relevance: Option<(String, f64)>,
    pub category: Option<(i64, f64)>,
    pub tags: Vec<(i64, i64, f64)>,
    pub raw: String,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub items: Vec<Item>,
    pub sources: Vec<Source>,
    pub tags: Vec<Tag>,
    pub categories: Vec<Category>,
    pub suggestions: Vec<Suggestion>,
    pub exclusions: Vec<Exclusion>,
    pub item_tags: BTreeMap<i64, Vec<ItemTag>>,
    pub moves: Vec<crate::moves::MoveOperation>,
    pub counts: BTreeMap<String, i64>,
    pub thumbnails: BTreeMap<i64, String>,
    pub settings: Settings,
}

//! Parsers for reMarkable `.metadata` and `.content` JSON sidecar files.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableMetadata {
    pub deleted: bool,
    #[serde(rename = "lastModified")]
    pub last_modified: String,
    #[serde(rename = "lastOpened")]
    pub last_opened: Option<String>,
    #[serde(rename = "lastOpenedPage")]
    pub last_opened_page: Option<u32>,
    pub metadatamodified: Option<bool>,
    pub modified: Option<bool>,
    pub parent: String,
    pub pinned: bool,
    pub synced: Option<bool>,
    #[serde(rename = "type")]
    pub doc_type: String,
    pub version: Option<u32>,
    #[serde(rename = "visibleName")]
    pub visible_name: String,
}

impl RemarkableMetadata {
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading metadata file {}", path.display()))?;
        let md = serde_json::from_str(&text)
            .with_context(|| format!("parsing metadata file {}", path.display()))?;
        Ok(md)
    }

    pub fn is_folder(&self) -> bool {
        self.doc_type == "CollectionType"
    }

    pub fn is_document(&self) -> bool {
        self.doc_type == "DocumentType"
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted || self.parent == "trash"
    }

    pub fn last_modified_ms(&self) -> Result<u64> {
        self.last_modified
            .parse::<u64>()
            .with_context(|| format!("parsing lastModified `{}` as u64", self.last_modified))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableContent {
    #[serde(rename = "fileType")]
    pub file_type: Option<String>,
    #[serde(rename = "formatVersion")]
    pub format_version: Option<u32>,
    pub orientation: Option<String>,
    #[serde(rename = "pageCount")]
    pub page_count: Option<u32>,
    pub pages: Option<Vec<String>>,
    #[serde(rename = "textScale")]
    pub text_scale: Option<f64>,
}

impl RemarkableContent {
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading content file {}", path.display()))?;
        let c = serde_json::from_str(&text)
            .with_context(|| format!("parsing content file {}", path.display()))?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const META_SAMPLE: &str = r#"{
        "deleted": false,
        "lastModified": "1712934567890",
        "lastOpened": "1712934567890",
        "lastOpenedPage": 3,
        "metadatamodified": false,
        "modified": false,
        "parent": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "pinned": false,
        "synced": true,
        "type": "DocumentType",
        "version": 1,
        "visibleName": "Meeting Notes"
    }"#;

    #[test]
    fn parses_valid_metadata() {
        let md: RemarkableMetadata = serde_json::from_str(META_SAMPLE).unwrap();
        assert_eq!(md.visible_name, "Meeting Notes");
        assert_eq!(md.doc_type, "DocumentType");
        assert_eq!(md.parent, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        assert_eq!(md.last_opened_page, Some(3));
        assert_eq!(md.synced, Some(true));
        assert_eq!(md.version, Some(1));
        assert!(md.is_document());
        assert!(!md.is_folder());
        assert!(!md.is_deleted());
        assert_eq!(md.last_modified_ms().unwrap(), 1_712_934_567_890);
    }

    #[test]
    fn detects_deleted_via_flag() {
        let json = r#"{
            "deleted": true, "lastModified": "1", "parent": "",
            "pinned": false, "type": "DocumentType", "visibleName": "Gone"
        }"#;
        let md: RemarkableMetadata = serde_json::from_str(json).unwrap();
        assert!(md.is_deleted());
    }

    #[test]
    fn detects_deleted_via_trash_parent() {
        let json = r#"{
            "deleted": false, "lastModified": "1", "parent": "trash",
            "pinned": false, "type": "DocumentType", "visibleName": "Gone"
        }"#;
        let md: RemarkableMetadata = serde_json::from_str(json).unwrap();
        assert!(md.is_deleted());
    }

    #[test]
    fn folder_type_detected() {
        let json = r#"{
            "deleted": false, "lastModified": "1", "parent": "",
            "pinned": false, "type": "CollectionType", "visibleName": "Folder"
        }"#;
        let md: RemarkableMetadata = serde_json::from_str(json).unwrap();
        assert!(md.is_folder());
        assert!(!md.is_document());
    }

    const CONTENT_SAMPLE: &str = r#"{
        "dpiRasterBackground": 226,
        "fileType": "notebook",
        "formatVersion": 2,
        "orientation": "portrait",
        "pageCount": 5,
        "pages": ["p1","p2","p3","p4","p5"],
        "textAlignment": "justify",
        "textScale": 1
    }"#;

    #[test]
    fn parses_valid_content() {
        let c: RemarkableContent = serde_json::from_str(CONTENT_SAMPLE).unwrap();
        assert_eq!(c.file_type.as_deref(), Some("notebook"));
        assert_eq!(c.format_version, Some(2));
        assert_eq!(c.orientation.as_deref(), Some("portrait"));
        assert_eq!(c.page_count, Some(5));
        assert_eq!(c.pages.as_ref().map(|p| p.len()), Some(5));
        assert_eq!(c.text_scale, Some(1.0));
    }
}

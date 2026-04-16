//! Parsers for reMarkable `.metadata` and `.content` JSON sidecar files.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableMetadata {
    /// Real firmware omits this on non-deleted documents; fall back to
    /// `false`. Trash detection still works via `parent == "trash"`.
    #[serde(default)]
    pub deleted: bool,
    #[serde(rename = "lastModified")]
    pub last_modified: String,
    #[serde(rename = "lastOpened")]
    pub last_opened: Option<String>,
    /// Signed because the device writes `-1` as "never opened" sentinel.
    #[serde(rename = "lastOpenedPage")]
    pub last_opened_page: Option<i32>,
    pub metadatamodified: Option<bool>,
    pub modified: Option<bool>,
    pub parent: String,
    #[serde(default)]
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
    /// Legacy format (firmware ≤ 2.x): flat array of page UUID strings.
    pub pages: Option<Vec<String>>,
    /// Modern format (firmware 3.x+): structured page objects.
    #[serde(rename = "cPages")]
    pub c_pages: Option<CPages>,
    #[serde(rename = "textScale")]
    pub text_scale: Option<f64>,
}

/// Structured page list used by modern reMarkable firmware.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CPages {
    pub pages: Option<Vec<CPage>>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A single page entry inside `cPages.pages[]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CPage {
    pub id: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl RemarkableContent {
    /// Return ordered page UUIDs from either the modern `cPages` format or
    /// the legacy flat `pages` array.
    pub fn page_ids(&self) -> Vec<String> {
        if let Some(cp) = &self.c_pages {
            if let Some(pages) = &cp.pages {
                return pages.iter().map(|p| p.id.clone()).collect();
            }
        }
        self.pages.clone().unwrap_or_default()
    }

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
    fn parses_metadata_with_missing_deleted_field() {
        // Verbatim from a real reMarkable 2 device (firmware as of 2026-04).
        // Fields metadatamodified/modified/synced/version are omitted and
        // must not be required by the parser.
        let json = r#"{
            "createdTime": "1730729401244",
            "lastModified": "1730733260890",
            "lastOpened": "1730729401570",
            "lastOpenedPage": 0,
            "parent": "trash",
            "pinned": false,
            "type": "DocumentType",
            "visibleName": "General"
        }"#;
        let md: RemarkableMetadata = serde_json::from_str(json).unwrap();
        assert!(!md.deleted);
        assert!(md.is_deleted()); // parent == "trash"
        assert_eq!(md.last_opened_page, Some(0));
        assert_eq!(md.visible_name, "General");
    }

    #[test]
    fn parses_metadata_with_negative_last_opened_page() {
        // The device writes -1 for "never opened" — must not overflow u32.
        let json = r#"{
            "lastModified": "1",
            "lastOpenedPage": -1,
            "parent": "",
            "type": "DocumentType",
            "visibleName": "Unopened"
        }"#;
        let md: RemarkableMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(md.last_opened_page, Some(-1));
        assert!(!md.is_deleted());
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

    #[test]
    fn page_ids_from_legacy_flat_array() {
        let c: RemarkableContent = serde_json::from_str(CONTENT_SAMPLE).unwrap();
        assert_eq!(c.page_ids(), vec!["p1", "p2", "p3", "p4", "p5"]);
    }

    const CONTENT_V2_SAMPLE: &str = r#"{
        "cPages": {
            "lastOpened": { "timestamp": "1:6", "value": "id-c" },
            "original": { "timestamp": "0:0", "value": -1 },
            "pages": [
                { "id": "id-a", "idx": { "timestamp": "1:3", "value": "aab" },
                  "template": { "timestamp": "1:3", "value": "Blank" } },
                { "id": "id-b", "idx": { "timestamp": "1:2", "value": "ba" },
                  "template": { "timestamp": "1:1", "value": "P Lines medium" } },
                { "id": "id-c", "idx": { "timestamp": "1:2", "value": "bb" } }
            ]
        },
        "fileType": "notebook",
        "formatVersion": 2,
        "orientation": "portrait",
        "pageCount": 3,
        "textScale": 1
    }"#;

    #[test]
    fn page_ids_from_cpages_v2_format() {
        let c: RemarkableContent = serde_json::from_str(CONTENT_V2_SAMPLE).unwrap();
        assert_eq!(c.page_ids(), vec!["id-a", "id-b", "id-c"]);
        assert_eq!(c.page_count, Some(3));
        assert!(c.pages.is_none());
        assert!(c.c_pages.is_some());
    }

    #[test]
    fn page_ids_returns_empty_when_both_absent() {
        let json = r#"{ "fileType": "notebook", "formatVersion": 2 }"#;
        let c: RemarkableContent = serde_json::from_str(json).unwrap();
        assert!(c.page_ids().is_empty());
    }

    #[test]
    fn cpages_roundtrip_preserves_extra_fields() {
        let c: RemarkableContent = serde_json::from_str(CONTENT_V2_SAMPLE).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        let c2: RemarkableContent = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.page_ids(), vec!["id-a", "id-b", "id-c"]);
        let page_a = &c2.c_pages.unwrap().pages.unwrap()[0];
        assert!(page_a.extra.contains_key("template"));
    }
}

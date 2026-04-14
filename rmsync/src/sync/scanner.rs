//! Scanners that enumerate local and remote file trees for diffing.
//!
//! Spec 08 implements `scan_remote`, which walks the xochitl directory over
//! SFTP, parses each document's metadata/content, and returns a manifest with
//! deterministic content hashes. Spec 09 adds the local counterpart.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::device::connection::{DeviceConnection, RemoteFileInfo};
use crate::remarkable::metadata::{RemarkableContent, RemarkableMetadata};

pub const XOCHITL_PATH: &str = "/home/root/.local/share/remarkable/xochitl";

/// Files smaller than this are hashed in full. Larger files use
/// size+mtime as a cheap proxy to avoid pulling megabytes over SFTP just to
/// know whether they changed.
pub const FULL_HASH_THRESHOLD: u64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct RemoteDocumentSnapshot {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub content_hash: String,
    pub total_size_bytes: u64,
    pub mtime: u64,
    pub page_count: usize,
    pub has_pdf: bool,
    pub file_list: Vec<RemoteFileInfo>,
}

#[derive(Debug)]
pub struct RemoteManifest {
    pub documents: Vec<RemoteDocumentSnapshot>,
    pub scanned_at: u64,
    pub total_documents: usize,
    pub total_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub current: usize,
    pub total: usize,
    pub current_name: String,
}

pub async fn scan_remote(conn: &DeviceConnection) -> Result<RemoteManifest> {
    scan_remote_with_progress(conn, |_| {}).await
}

pub async fn scan_remote_with_progress<F>(
    conn: &DeviceConnection,
    progress_callback: F,
) -> Result<RemoteManifest>
where
    F: Fn(ScanProgress) + Send + 'static,
{
    let entries = conn
        .list_dir(XOCHITL_PATH)
        .await
        .with_context(|| format!("listing {XOCHITL_PATH}"))?;

    let uuids = collect_uuids(&entries);
    let total = uuids.len();

    let mut documents = Vec::new();
    let mut total_size = 0u64;
    for (i, uuid) in uuids.iter().enumerate() {
        let visible_name = match build_snapshot(conn, uuid, &entries).await {
            Ok(Some(snap)) => {
                let name = snap.metadata.visible_name.clone();
                total_size += snap.total_size_bytes;
                documents.push(snap);
                name
            }
            Ok(None) => uuid.clone(), // deleted/trashed
            Err(e) => {
                tracing::warn!("scan: skipping {uuid}: {e:#}");
                uuid.clone()
            }
        };
        progress_callback(ScanProgress {
            current: i + 1,
            total,
            current_name: visible_name,
        });
    }

    Ok(RemoteManifest {
        total_documents: documents.len(),
        total_size_bytes: total_size,
        documents,
        scanned_at: now_unix(),
    })
}

fn collect_uuids(entries: &[RemoteFileInfo]) -> Vec<String> {
    let mut uuids: Vec<String> = entries
        .iter()
        .filter_map(|e| e.name.strip_suffix(".metadata").map(|u| u.to_string()))
        .collect();
    uuids.sort();
    uuids
}

async fn build_snapshot(
    conn: &DeviceConnection,
    uuid: &str,
    xochitl_entries: &[RemoteFileInfo],
) -> Result<Option<RemoteDocumentSnapshot>> {
    let meta_bytes = conn
        .read_file(&format!("{XOCHITL_PATH}/{uuid}.metadata"))
        .await?;
    let metadata: RemarkableMetadata = serde_json::from_slice(&meta_bytes)
        .with_context(|| format!("parsing {uuid}.metadata"))?;

    if metadata.is_deleted() {
        return Ok(None);
    }

    let content_path = format!("{XOCHITL_PATH}/{uuid}.content");
    let content = match conn.read_file(&content_path).await {
        Ok(bytes) => match serde_json::from_slice::<RemarkableContent>(&bytes) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::debug!("content parse failed for {uuid}: {e}");
                None
            }
        },
        Err(_) => None,
    };

    let mut file_list = files_for_uuid(xochitl_entries, uuid);

    // Page files live in {uuid}/ subdir.
    let subdir = format!("{XOCHITL_PATH}/{uuid}");
    if let Ok(children) = conn.list_dir(&subdir).await {
        file_list.extend(children);
    }

    let page_count = file_list
        .iter()
        .filter(|f| f.name.ends_with(".rm"))
        .count();
    let has_pdf = file_list.iter().any(|f| f.name.ends_with(".pdf"));
    let total_size_bytes = file_list.iter().map(|f| f.size).sum();
    let mtime = file_list.iter().map(|f| f.mtime).max().unwrap_or(0);
    let content_hash = compute_remote_hash(conn, &file_list).await?;

    Ok(Some(RemoteDocumentSnapshot {
        uuid: uuid.to_string(),
        metadata,
        content,
        content_hash,
        total_size_bytes,
        mtime,
        page_count,
        has_pdf,
        file_list,
    }))
}

fn files_for_uuid(entries: &[RemoteFileInfo], uuid: &str) -> Vec<RemoteFileInfo> {
    entries
        .iter()
        .filter(|e| {
            e.name == format!("{uuid}.metadata")
                || e.name == format!("{uuid}.content")
                || e.name == format!("{uuid}.pdf")
                || e.name == format!("{uuid}.pagedata")
                || e.name == format!("{uuid}.epub")
                || e.name.starts_with(&format!("{uuid}."))
        })
        .cloned()
        .collect()
}

async fn compute_remote_hash(
    conn: &DeviceConnection,
    file_list: &[RemoteFileInfo],
) -> Result<String> {
    let mut sorted: Vec<&RemoteFileInfo> = file_list.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = Sha256::new();
    for f in sorted {
        hasher.update(f.path.as_bytes());
        hasher.update([0u8]);
        if f.is_dir {
            continue;
        }
        if f.size <= FULL_HASH_THRESHOLD {
            match conn.read_file(&f.path).await {
                Ok(bytes) => hasher.update(&bytes),
                Err(_) => hasher.update(format!("stat:{}:{}", f.size, f.mtime).as_bytes()),
            }
        } else {
            hasher.update(format!("stat:{}:{}", f.size, f.mtime).as_bytes());
        }
        hasher.update([0u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(path: &str, size: u64, mtime: u64) -> RemoteFileInfo {
        let name = path.rsplit('/').next().unwrap().to_string();
        RemoteFileInfo {
            name,
            path: path.to_string(),
            size,
            mtime,
            is_dir: false,
        }
    }

    #[test]
    fn collect_uuids_picks_metadata_files() {
        let entries = vec![
            info("/x/aaa.metadata", 10, 1),
            info("/x/aaa.content", 5, 1),
            info("/x/bbb.metadata", 10, 1),
            info("/x/random.txt", 1, 1),
        ];
        let uuids = collect_uuids(&entries);
        assert_eq!(uuids, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[test]
    fn files_for_uuid_matches_all_extensions() {
        let entries = vec![
            info("/x/aaa.metadata", 1, 1),
            info("/x/aaa.content", 1, 1),
            info("/x/aaa.pdf", 1, 1),
            info("/x/aaa.pagedata", 1, 1),
            info("/x/bbb.metadata", 1, 1),
        ];
        let files = files_for_uuid(&entries, "aaa");
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn hex_encode_matches_known() {
        assert_eq!(hex::encode([0, 1, 255]), "0001ff");
    }

    #[test]
    fn parses_mock_metadata_from_bytes() {
        let bytes = br#"{
            "deleted": false, "lastModified": "42", "parent": "",
            "pinned": false, "type": "DocumentType", "visibleName": "x"
        }"#;
        let md: RemarkableMetadata = serde_json::from_slice(bytes).unwrap();
        assert_eq!(md.visible_name, "x");
        assert!(!md.is_deleted());
    }

    #[test]
    fn deleted_metadata_is_flagged() {
        let bytes = br#"{
            "deleted": true, "lastModified": "1", "parent": "",
            "pinned": false, "type": "DocumentType", "visibleName": "g"
        }"#;
        let md: RemarkableMetadata = serde_json::from_slice(bytes).unwrap();
        assert!(md.is_deleted());
    }

    #[test]
    fn scan_progress_struct_builds() {
        let p = ScanProgress {
            current: 3,
            total: 10,
            current_name: "doc".into(),
        };
        assert_eq!(p.current, 3);
        assert_eq!(p.total, 10);
    }

    #[test]
    fn remote_manifest_construction() {
        let m = RemoteManifest {
            documents: vec![],
            scanned_at: 1,
            total_documents: 0,
            total_size_bytes: 0,
        };
        assert_eq!(m.total_documents, 0);
    }

    #[test]
    fn xochitl_path_constant() {
        assert!(XOCHITL_PATH.ends_with("/xochitl"));
    }
}

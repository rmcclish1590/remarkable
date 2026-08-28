//! Scanners that enumerate local and remote file trees for diffing.
//!
//! Spec 08 implements `scan_remote`, which walks the xochitl directory over
//! SFTP, parses each document's metadata/content, and returns a manifest with
//! deterministic content hashes. Spec 09 adds the local counterpart.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::device::connection::{DeviceConnection, RemoteFileInfo};
use crate::remarkable::metadata::{RemarkableContent, RemarkableMetadata};
use crate::sync::state_db::SyncFileState;

pub const XOCHITL_PATH: &str = "/home/root/.local/share/remarkable/xochitl";

/// Files smaller than this are hashed in full. Larger files use
/// size+mtime as a cheap proxy to avoid pulling megabytes over SFTP just to
/// know whether they changed.
pub const FULL_HASH_THRESHOLD: u64 = 1_000_000;

/// Bound on how far the parent chain is followed when filling in folders.
/// Deeper than any real reMarkable hierarchy, and stops a cycle in device
/// metadata from looping forever.
const MAX_FOLDER_DEPTH: usize = 32;

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

    // Documents carry only their parent's UUID, so a folder that the sweep
    // above missed leaves its children looking parentless — they get promoted
    // to the root and the local tree collapses into one flat list.
    fetch_missing_parents(conn, &entries, &mut documents, &mut total_size).await;

    Ok(RemoteManifest {
        total_documents: documents.len(),
        total_size_bytes: total_size,
        documents,
        scanned_at: now_unix(),
    })
}

/// Walk up the parent chain, pulling in any ancestor folder the scan does not
/// already have. Repeats until the set is closed so folders nested many deep
/// are all present, and is bounded in case the device reports a parent cycle.
async fn fetch_missing_parents(
    conn: &DeviceConnection,
    entries: &[RemoteFileInfo],
    documents: &mut Vec<RemoteDocumentSnapshot>,
    total_size: &mut u64,
) {
    for _ in 0..MAX_FOLDER_DEPTH {
        let missing = missing_parent_uuids(documents);
        if missing.is_empty() {
            return;
        }
        let before = documents.len();
        for uuid in missing {
            match build_snapshot(conn, &uuid, entries).await {
                Ok(Some(snap)) => {
                    *total_size += snap.total_size_bytes;
                    documents.push(snap);
                }
                // A trashed or unreadable ancestor is not fatal: the children
                // simply stay at the root, which is what they did before.
                Ok(None) => tracing::debug!("parent folder {uuid} is deleted; skipping"),
                Err(e) => tracing::warn!("could not read parent folder {uuid}: {e:#}"),
            }
        }
        if documents.len() == before {
            return; // nothing resolvable; stop rather than spin
        }
    }
    tracing::warn!("folder nesting exceeded {MAX_FOLDER_DEPTH} levels; tree may be partial");
}

/// Parent UUIDs referenced by these snapshots that are not themselves present.
/// Roots (`""`) and the trash bucket are not folders and never resolve.
pub(crate) fn missing_parent_uuids(documents: &[RemoteDocumentSnapshot]) -> Vec<String> {
    let present: std::collections::HashSet<&str> =
        documents.iter().map(|d| d.uuid.as_str()).collect();
    let mut missing: Vec<String> = documents
        .iter()
        .map(|d| d.metadata.parent.as_str())
        .filter(|p| !p.is_empty() && *p != "trash" && !present.contains(p))
        .map(str::to_string)
        .collect();
    missing.sort();
    missing.dedup();
    missing
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

/// Relativise a remote path so it hashes identically to the local scanner's
/// relative paths. `/home/root/.../xochitl/uuid.metadata` → `uuid.metadata`.
fn remote_path_to_relative(path: &str) -> &str {
    let prefix = format!("{}/", XOCHITL_PATH);
    path.strip_prefix(&prefix)
        .or_else(|| path.strip_prefix(XOCHITL_PATH))
        .unwrap_or(path)
}

async fn compute_remote_hash(
    conn: &DeviceConnection,
    file_list: &[RemoteFileInfo],
) -> Result<String> {
    let mut sorted: Vec<&RemoteFileInfo> = file_list.iter().collect();
    sorted.sort_by(|a, b| {
        remote_path_to_relative(&a.path).cmp(remote_path_to_relative(&b.path))
    });

    let mut hasher = Sha256::new();
    for f in sorted {
        hasher.update(remote_path_to_relative(&f.path).as_bytes());
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

// =========================================================================
// Local scanner (spec 09)
// =========================================================================

/// Subdirectory under the user's sync destination that mirrors the tablet's
/// xochitl tree.
pub const RAW_SUBDIR: &str = "raw";

#[derive(Debug, Clone)]
pub struct LocalFileInfo {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct LocalDocumentSnapshot {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub content_hash: String,
    pub total_size_bytes: u64,
    pub mtime: u64,
    pub page_count: usize,
    pub has_pdf: bool,
    pub file_list: Vec<LocalFileInfo>,
}

#[derive(Debug)]
pub struct LocalManifest {
    pub documents: Vec<LocalDocumentSnapshot>,
    pub scanned_at: u64,
    pub total_documents: usize,
    pub total_size_bytes: u64,
    pub sync_dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChangeType {
    Unchanged,
    ModifiedLocally,
    ModifiedRemotely,
    ModifiedBoth,
    NewLocal,
    NewRemote,
    DeletedLocally,
    DeletedRemotely,
}

pub fn get_watch_paths(sync_dir: &Path) -> Vec<PathBuf> {
    vec![sync_dir.join(RAW_SUBDIR)]
}

pub fn scan_local(sync_dir: &Path) -> Result<LocalManifest> {
    let raw = sync_dir.join(RAW_SUBDIR);
    if !raw.exists() {
        std::fs::create_dir_all(&raw)
            .with_context(|| format!("creating {}", raw.display()))?;
        return Ok(LocalManifest {
            documents: vec![],
            scanned_at: now_unix(),
            total_documents: 0,
            total_size_bytes: 0,
            sync_dir: sync_dir.to_path_buf(),
        });
    }

    let mut uuids = Vec::new();
    for entry in std::fs::read_dir(&raw)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(u) = name.strip_suffix(".metadata") {
                uuids.push(u.to_string());
            }
        }
    }
    uuids.sort();

    let mut documents = Vec::new();
    let mut total_size = 0u64;
    for uuid in &uuids {
        if let Some(snap) = build_local_snapshot(&raw, uuid)? {
            total_size += snap.total_size_bytes;
            documents.push(snap);
        }
    }

    Ok(LocalManifest {
        total_documents: documents.len(),
        total_size_bytes: total_size,
        documents,
        scanned_at: now_unix(),
        sync_dir: sync_dir.to_path_buf(),
    })
}

fn build_local_snapshot(raw: &Path, uuid: &str) -> Result<Option<LocalDocumentSnapshot>> {
    let meta_path = raw.join(format!("{uuid}.metadata"));
    let metadata = RemarkableMetadata::from_file(&meta_path)?;
    if metadata.is_deleted() {
        return Ok(None);
    }

    let content_path = raw.join(format!("{uuid}.content"));
    let content = if content_path.exists() {
        RemarkableContent::from_file(&content_path).ok()
    } else {
        None
    };

    let mut file_list = Vec::new();
    for entry in std::fs::read_dir(raw)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.starts_with(&format!("{uuid}.")) {
            if let Ok(info) = local_info(&entry.path(), &fname) {
                file_list.push(info);
            }
        }
    }
    let subdir = raw.join(uuid);
    if subdir.is_dir() {
        for entry in std::fs::read_dir(&subdir)? {
            let entry = entry?;
            let fname = entry.file_name().to_string_lossy().into_owned();
            if let Ok(info) = local_info(&entry.path(), &fname) {
                file_list.push(info);
            }
        }
    }

    let page_count = file_list.iter().filter(|f| f.name.ends_with(".rm")).count();
    let has_pdf = file_list.iter().any(|f| f.name.ends_with(".pdf"));
    let total_size_bytes = file_list.iter().map(|f| f.size).sum();
    let mtime = file_list.iter().map(|f| f.mtime).max().unwrap_or(0);
    let content_hash = compute_local_hash(uuid, &file_list, raw)?;

    Ok(Some(LocalDocumentSnapshot {
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

fn local_info(path: &Path, name: &str) -> Result<LocalFileInfo> {
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(LocalFileInfo {
        path: path.to_path_buf(),
        name: name.to_string(),
        size: md.len(),
        mtime,
    })
}

pub fn compute_local_hash(
    _uuid: &str,
    files: &[LocalFileInfo],
    raw_dir: &Path,
) -> Result<String> {
    let mut sorted: Vec<&LocalFileInfo> = files.iter().collect();
    sorted.sort_by(|a, b| {
        let ra = a.path.strip_prefix(raw_dir).unwrap_or(&a.path);
        let rb = b.path.strip_prefix(raw_dir).unwrap_or(&b.path);
        ra.cmp(rb)
    });

    let mut hasher = Sha256::new();
    for f in sorted {
        let rel = f.path.strip_prefix(raw_dir).unwrap_or(&f.path);
        let rel_str = rel.to_string_lossy();
        hasher.update(rel_str.as_bytes());
        hasher.update([0u8]);
        if f.size <= FULL_HASH_THRESHOLD {
            match std::fs::read(&f.path) {
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

pub fn classify_change(
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> ChangeType {
    match (local, remote, synced) {
        (None, None, _) => ChangeType::Unchanged,
        (Some(_), None, None) => ChangeType::NewLocal,
        (None, Some(_), None) => ChangeType::NewRemote,
        (Some(_), None, Some(_)) => ChangeType::DeletedRemotely,
        (None, Some(_), Some(_)) => ChangeType::DeletedLocally,
        (Some(l), Some(r), None) => {
            if l.content_hash == r.content_hash {
                ChangeType::Unchanged
            } else {
                ChangeType::ModifiedBoth
            }
        }
        (Some(l), Some(r), Some(s)) => {
            let local_changed = s.synced_hash.as_deref() != Some(l.content_hash.as_str());
            let remote_changed = s.synced_hash.as_deref() != Some(r.content_hash.as_str());
            match (local_changed, remote_changed) {
                (false, false) => ChangeType::Unchanged,
                (true, false) => ChangeType::ModifiedLocally,
                (false, true) => ChangeType::ModifiedRemotely,
                (true, true) => {
                    if l.content_hash == r.content_hash {
                        ChangeType::Unchanged
                    } else {
                        ChangeType::ModifiedBoth
                    }
                }
            }
        }
    }
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

    #[test]
    fn remote_path_to_relative_strips_xochitl_prefix() {
        assert_eq!(
            remote_path_to_relative(&format!("{XOCHITL_PATH}/abc.metadata")),
            "abc.metadata"
        );
        assert_eq!(
            remote_path_to_relative(&format!("{XOCHITL_PATH}/abc/p1.rm")),
            "abc/p1.rm"
        );
        assert_eq!(remote_path_to_relative("other/path"), "other/path");
    }

    // --- local scanner (spec 09) ---

    use crate::sync::state_db::{SyncFileState, SyncStatus};
    use std::fs;
    use tempfile::tempdir;

    fn write_doc(raw: &Path, uuid: &str, name: &str, rm_content: &[u8]) {
        let meta = format!(
            r#"{{
                "deleted": false,
                "lastModified": "1",
                "parent": "",
                "pinned": false,
                "type": "DocumentType",
                "visibleName": "{name}"
            }}"#
        );
        fs::write(raw.join(format!("{uuid}.metadata")), meta).unwrap();
        let content = r#"{"fileType":"notebook","pageCount":1,"pages":["p1"]}"#;
        fs::write(raw.join(format!("{uuid}.content")), content).unwrap();
        let pages = raw.join(uuid);
        fs::create_dir_all(&pages).unwrap();
        fs::write(pages.join("p1.rm"), rm_content).unwrap();
    }

    #[test]
    fn scan_local_empty_dir_returns_empty_manifest() {
        let dir = tempdir().unwrap();
        let m = scan_local(dir.path()).unwrap();
        assert_eq!(m.total_documents, 0);
        assert!(dir.path().join("raw").exists());
    }

    #[test]
    fn scan_local_single_doc_populates_snapshot() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        fs::create_dir_all(&raw).unwrap();
        write_doc(&raw, "abc", "Hello", b"pagebytes");
        let m = scan_local(dir.path()).unwrap();
        assert_eq!(m.total_documents, 1);
        let d = &m.documents[0];
        assert_eq!(d.uuid, "abc");
        assert_eq!(d.metadata.visible_name, "Hello");
        assert_eq!(d.page_count, 1);
        assert!(!d.has_pdf);
        assert!(!d.content_hash.is_empty());
    }

    #[test]
    fn local_hash_is_deterministic() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        fs::create_dir_all(&raw).unwrap();
        write_doc(&raw, "abc", "Same", b"aaaa");
        let m1 = scan_local(dir.path()).unwrap();
        let m2 = scan_local(dir.path()).unwrap();
        assert_eq!(m1.documents[0].content_hash, m2.documents[0].content_hash);
    }

    #[test]
    fn local_hash_changes_when_file_changes() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        fs::create_dir_all(&raw).unwrap();
        write_doc(&raw, "abc", "Same", b"aaaa");
        let h1 = scan_local(dir.path()).unwrap().documents[0].content_hash.clone();
        fs::write(raw.join("abc").join("p1.rm"), b"bbbb").unwrap();
        let h2 = scan_local(dir.path()).unwrap().documents[0].content_hash.clone();
        assert_ne!(h1, h2);
    }

    #[test]
    fn deleted_local_doc_excluded() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        fs::create_dir_all(&raw).unwrap();
        fs::write(
            raw.join("xxx.metadata"),
            r#"{"deleted":true,"lastModified":"1","parent":"","pinned":false,"type":"DocumentType","visibleName":"g"}"#,
        )
        .unwrap();
        let m = scan_local(dir.path()).unwrap();
        assert_eq!(m.total_documents, 0);
    }

    fn mk_local(hash: &str) -> LocalDocumentSnapshot {
        LocalDocumentSnapshot {
            uuid: "u".into(),
            metadata: serde_json::from_str(
                r#"{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"DocumentType","visibleName":"x"}"#,
            )
            .unwrap(),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 0,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn mk_remote(hash: &str) -> RemoteDocumentSnapshot {
        RemoteDocumentSnapshot {
            uuid: "u".into(),
            metadata: serde_json::from_str(
                r#"{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"DocumentType","visibleName":"x"}"#,
            )
            .unwrap(),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 0,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn snap_with_parent(uuid: &str, parent: &str, doc_type: &str) -> RemoteDocumentSnapshot {
        let mut snap = mk_remote("h");
        snap.uuid = uuid.into();
        snap.metadata = serde_json::from_str(&format!(
            r#"{{"deleted":false,"lastModified":"1","parent":"{parent}","pinned":false,"type":"{doc_type}","visibleName":"n"}}"#
        ))
        .unwrap();
        snap
    }

    #[test]
    fn missing_parents_are_reported_once_each() {
        let docs = vec![
            snap_with_parent("d1", "folder-A", "DocumentType"),
            snap_with_parent("d2", "folder-A", "DocumentType"),
            snap_with_parent("d3", "folder-B", "DocumentType"),
        ];
        assert_eq!(missing_parent_uuids(&docs), vec!["folder-A", "folder-B"]);
    }

    #[test]
    fn present_parents_and_non_folders_are_not_reported() {
        let docs = vec![
            snap_with_parent("folder-A", "", "CollectionType"),
            snap_with_parent("d1", "folder-A", "DocumentType"), // parent present
            snap_with_parent("d2", "", "DocumentType"),         // root
            snap_with_parent("d3", "trash", "DocumentType"),    // trashed
        ];
        assert!(
            missing_parent_uuids(&docs).is_empty(),
            "nothing left to fetch"
        );
    }

    #[test]
    fn nested_parents_resolve_one_level_per_pass() {
        // grandchild -> sub -> top, with only the grandchild present at first.
        let mut docs = vec![snap_with_parent("d1", "sub", "DocumentType")];
        assert_eq!(missing_parent_uuids(&docs), vec!["sub"]);

        docs.push(snap_with_parent("sub", "top", "CollectionType"));
        assert_eq!(
            missing_parent_uuids(&docs),
            vec!["top"],
            "resolving one level exposes the next"
        );

        docs.push(snap_with_parent("top", "", "CollectionType"));
        assert!(missing_parent_uuids(&docs).is_empty());
    }

    fn mk_synced(hash: &str) -> SyncFileState {
        SyncFileState {
            uuid: "u".into(),
            visible_name: "x".into(),
            parent_uuid: String::new(),
            doc_type: "DocumentType".into(),
            local_hash: Some(hash.into()),
            remote_hash: Some(hash.into()),
            synced_hash: Some(hash.into()),
            local_mtime: None,
            remote_mtime: None,
            synced_mtime: None,
            last_sync_at: None,
            sync_status: SyncStatus::Synced,
            conflict_info: None,
        }
    }

    #[test]
    fn classify_unchanged() {
        let l = mk_local("h1");
        let r = mk_remote("h1");
        let s = mk_synced("h1");
        assert_eq!(
            classify_change(Some(&l), Some(&r), Some(&s)),
            ChangeType::Unchanged
        );
    }

    #[test]
    fn classify_modified_locally() {
        let l = mk_local("h2");
        let r = mk_remote("h1");
        let s = mk_synced("h1");
        assert_eq!(
            classify_change(Some(&l), Some(&r), Some(&s)),
            ChangeType::ModifiedLocally
        );
    }

    #[test]
    fn classify_modified_remotely() {
        let l = mk_local("h1");
        let r = mk_remote("h2");
        let s = mk_synced("h1");
        assert_eq!(
            classify_change(Some(&l), Some(&r), Some(&s)),
            ChangeType::ModifiedRemotely
        );
    }

    #[test]
    fn classify_modified_both() {
        let l = mk_local("h2");
        let r = mk_remote("h3");
        let s = mk_synced("h1");
        assert_eq!(
            classify_change(Some(&l), Some(&r), Some(&s)),
            ChangeType::ModifiedBoth
        );
    }

    #[test]
    fn classify_new_local_and_remote() {
        let l = mk_local("h1");
        let r = mk_remote("h1");
        assert_eq!(classify_change(Some(&l), None, None), ChangeType::NewLocal);
        assert_eq!(classify_change(None, Some(&r), None), ChangeType::NewRemote);
    }

    #[test]
    fn classify_deleted_sides() {
        let l = mk_local("h1");
        let r = mk_remote("h1");
        let s = mk_synced("h1");
        assert_eq!(
            classify_change(Some(&l), None, Some(&s)),
            ChangeType::DeletedRemotely
        );
        assert_eq!(
            classify_change(None, Some(&r), Some(&s)),
            ChangeType::DeletedLocally
        );
    }

    #[test]
    fn watch_paths_returns_raw() {
        let paths = get_watch_paths(Path::new("/sync"));
        assert_eq!(paths, vec![PathBuf::from("/sync/raw")]);
    }
}

//! Reconstructs the reMarkable folder hierarchy from flat UUID-based files.

use crate::remarkable::metadata::{RemarkableContent, RemarkableMetadata};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DocumentNode {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub children: Vec<DocumentNode>,
}

#[derive(Debug)]
pub struct DocumentTree {
    pub roots: Vec<DocumentNode>,
}

impl DocumentTree {
    pub fn build_from_directory(dir: &Path) -> Result<Self> {
        let mut entries: HashMap<String, (RemarkableMetadata, Option<RemarkableContent>)> =
            HashMap::new();

        for entry in
            fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("metadata") {
                continue;
            }
            let Some(uuid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };

            let metadata = RemarkableMetadata::from_file(&path)?;
            let content_path = dir.join(format!("{uuid}.content"));
            let content = if content_path.exists() {
                Some(RemarkableContent::from_file(&content_path)?)
            } else {
                None
            };

            entries.insert(uuid, (metadata, content));
        }

        let live: HashMap<String, (RemarkableMetadata, Option<RemarkableContent>)> = entries
            .into_iter()
            .filter(|(_, (md, _))| !md.is_deleted())
            .collect();

        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut root_uuids: Vec<String> = Vec::new();
        for (uuid, (md, _)) in &live {
            if md.parent.is_empty() || !live.contains_key(&md.parent) {
                root_uuids.push(uuid.clone());
            } else {
                children_map
                    .entry(md.parent.clone())
                    .or_default()
                    .push(uuid.clone());
            }
        }

        let mut roots: Vec<DocumentNode> = root_uuids
            .iter()
            .map(|u| build_node(u, &live, &children_map))
            .collect();
        sort_nodes(&mut roots);

        Ok(DocumentTree { roots })
    }

    pub fn find_by_uuid(&self, uuid: &str) -> Option<&DocumentNode> {
        fn walk<'a>(nodes: &'a [DocumentNode], uuid: &str) -> Option<&'a DocumentNode> {
            for n in nodes {
                if n.uuid == uuid {
                    return Some(n);
                }
                if let Some(hit) = walk(&n.children, uuid) {
                    return Some(hit);
                }
            }
            None
        }
        walk(&self.roots, uuid)
    }

    pub fn flat_list(&self) -> Vec<&DocumentNode> {
        fn walk<'a>(nodes: &'a [DocumentNode], out: &mut Vec<&'a DocumentNode>) {
            for n in nodes {
                if n.metadata.is_document() {
                    out.push(n);
                }
                walk(&n.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, &mut out);
        out
    }
}

fn build_node(
    uuid: &str,
    live: &HashMap<String, (RemarkableMetadata, Option<RemarkableContent>)>,
    children_map: &HashMap<String, Vec<String>>,
) -> DocumentNode {
    let (md, content) = live[uuid].clone();
    let mut children: Vec<DocumentNode> = children_map
        .get(uuid)
        .map(|ids| ids.iter().map(|c| build_node(c, live, children_map)).collect())
        .unwrap_or_default();
    sort_nodes(&mut children);
    DocumentNode {
        uuid: uuid.to_string(),
        metadata: md,
        content,
        children,
    }
}

fn sort_nodes(nodes: &mut [DocumentNode]) {
    nodes.sort_by(|a, b| {
        let a_folder = a.metadata.is_folder();
        let b_folder = b.metadata.is_folder();
        b_folder
            .cmp(&a_folder)
            .then_with(|| a.metadata.visible_name.cmp(&b.metadata.visible_name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_meta(
        dir: &Path,
        uuid: &str,
        parent: &str,
        doc_type: &str,
        name: &str,
        deleted: bool,
    ) {
        let json = format!(
            r#"{{
              "deleted": {deleted},
              "lastModified": "1",
              "parent": "{parent}",
              "pinned": false,
              "type": "{doc_type}",
              "visibleName": "{name}"
            }}"#
        );
        fs::write(dir.join(format!("{uuid}.metadata")), json).unwrap();
    }

    fn write_content(dir: &Path, uuid: &str, pages: usize) {
        let page_ids: Vec<String> = (0..pages).map(|i| format!("\"p{i}\"")).collect();
        let json = format!(
            r#"{{
              "fileType": "notebook",
              "formatVersion": 2,
              "pageCount": {pages},
              "pages": [{}]
            }}"#,
            page_ids.join(",")
        );
        fs::write(dir.join(format!("{uuid}.content")), json).unwrap();
    }

    #[test]
    fn builds_nested_tree_with_content_and_sorting() {
        let td = tempdir().unwrap();
        let p = td.path();

        write_meta(p, "folder-A", "", "CollectionType", "Work", false);
        write_meta(p, "doc-1", "folder-A", "DocumentType", "Notes", false);
        write_content(p, "doc-1", 3);
        write_meta(p, "sub-folder", "folder-A", "CollectionType", "Sub", false);
        write_meta(p, "doc-2", "sub-folder", "DocumentType", "Deep", false);
        write_meta(p, "doc-3", "", "DocumentType", "Alone", false);

        let tree = DocumentTree::build_from_directory(p).unwrap();

        assert_eq!(tree.roots.len(), 2);
        assert_eq!(tree.roots[0].uuid, "folder-A");
        assert_eq!(tree.roots[1].uuid, "doc-3");

        let fa = &tree.roots[0];
        assert_eq!(fa.children.len(), 2);
        assert_eq!(fa.children[0].uuid, "sub-folder");
        assert_eq!(fa.children[1].uuid, "doc-1");
        assert_eq!(
            fa.children[1].content.as_ref().and_then(|c| c.page_count),
            Some(3)
        );

        let sf = &fa.children[0];
        assert_eq!(sf.children.len(), 1);
        assert_eq!(sf.children[0].uuid, "doc-2");

        assert_eq!(
            tree.find_by_uuid("doc-2").unwrap().metadata.visible_name,
            "Deep"
        );
        assert!(tree.find_by_uuid("nope").is_none());

        let docs: Vec<&str> = tree.flat_list().iter().map(|n| n.uuid.as_str()).collect();
        assert_eq!(docs, vec!["doc-2", "doc-1", "doc-3"]);
    }

    #[test]
    fn orphan_parent_goes_to_root() {
        let td = tempdir().unwrap();
        let p = td.path();
        write_meta(p, "orphan", "ghost", "DocumentType", "Orphan", false);
        let tree = DocumentTree::build_from_directory(p).unwrap();
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].uuid, "orphan");
    }

    #[test]
    fn excludes_deleted_and_trashed() {
        let td = tempdir().unwrap();
        let p = td.path();
        write_meta(p, "gone", "", "DocumentType", "Gone", true);
        write_meta(p, "trashed", "trash", "DocumentType", "Trashed", false);
        write_meta(p, "alive", "", "DocumentType", "Alive", false);
        let tree = DocumentTree::build_from_directory(p).unwrap();
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].uuid, "alive");
    }
}

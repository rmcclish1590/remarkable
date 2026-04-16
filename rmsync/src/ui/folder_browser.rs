//! Document tree sidebar showing the reMarkable folder hierarchy.
//!
//! Uses GTK4's idiomatic tree pattern: a `GtkTreeListModel` over nested
//! `Gio::ListStore`s of `glib::BoxedAnyObject`-wrapped `TreeItem`s, with a
//! `GtkListView` and `SignalListItemFactory` rendering each row as
//! `[TreeExpander] [Icon] [Name] [Page count]`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, BoxedAnyObject};
use gtk::prelude::*;

use crate::remarkable::document::{DocumentNode, DocumentTree};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeItemType {
    Folder,
    Notebook,
    Pdf,
    Epub,
}

#[derive(Debug, Clone)]
pub struct TreeItem {
    pub uuid: String,
    pub name: String,
    pub item_type: TreeItemType,
    pub page_count: Option<u32>,
    pub last_modified: Option<u64>,
    pub children: Vec<TreeItem>,
}

impl TreeItem {
    pub fn from_node(node: &DocumentNode) -> Self {
        let item_type = classify_node(node);
        let page_count = node.content.as_ref().and_then(|c| c.page_count);
        let last_modified = node.metadata.last_modified_ms().ok();
        let children = if matches!(item_type, TreeItemType::Folder) {
            node.children.iter().map(TreeItem::from_node).collect()
        } else {
            Vec::new()
        };
        TreeItem {
            uuid: node.uuid.clone(),
            name: node.metadata.visible_name.clone(),
            item_type,
            page_count,
            last_modified,
            children,
        }
    }

    pub fn from_tree(tree: &DocumentTree) -> Vec<TreeItem> {
        tree.roots.iter().map(TreeItem::from_node).collect()
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.item_type, TreeItemType::Folder)
    }

    pub fn icon_name(&self) -> &'static str {
        match self.item_type {
            TreeItemType::Folder => "folder-symbolic",
            TreeItemType::Notebook => "document-edit-symbolic",
            TreeItemType::Pdf => "application-pdf-symbolic",
            TreeItemType::Epub => "document-symbolic",
        }
    }
}

fn classify_node(node: &DocumentNode) -> TreeItemType {
    if node.metadata.is_folder() {
        return TreeItemType::Folder;
    }
    match node.content.as_ref().and_then(|c| c.file_type.as_deref()) {
        Some("pdf") => TreeItemType::Pdf,
        Some("epub") => TreeItemType::Epub,
        _ => TreeItemType::Notebook,
    }
}

type SelectedCallback = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;

#[derive(Clone)]
pub struct FolderBrowser {
    pub widget: gtk::Box,
    root_store: gio::ListStore,
    selection: gtk::SingleSelection,
    tree_model: gtk::TreeListModel,
    selected_callback: SelectedCallback,
}

impl FolderBrowser {
    pub fn new() -> Self {
        let root_store = gio::ListStore::new::<BoxedAnyObject>();
        let tree_model = gtk::TreeListModel::new(
            root_store.clone(),
            false,
            false,
            |obj: &glib::Object| -> Option<gio::ListModel> {
                let boxed = obj.downcast_ref::<BoxedAnyObject>()?;
                let item: std::cell::Ref<TreeItem> = boxed.borrow();
                if !item.is_folder() || item.children.is_empty() {
                    return None;
                }
                let store = gio::ListStore::new::<BoxedAnyObject>();
                for child in &item.children {
                    store.append(&BoxedAnyObject::new(child.clone()));
                }
                Some(store.upcast())
            },
        );

        let selection = gtk::SingleSelection::new(Some(tree_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem");
            let expander = gtk::TreeExpander::new();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::new();
            let name = gtk::Label::new(None);
            let pages = gtk::Label::new(None);
            name.set_hexpand(true);
            name.set_halign(gtk::Align::Start);
            pages.set_halign(gtk::Align::End);
            pages.add_css_class("dim-label");
            hbox.append(&icon);
            hbox.append(&name);
            hbox.append(&pages);
            expander.set_child(Some(&hbox));
            list_item.set_child(Some(&expander));
        });
        factory.connect_bind(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem");
            let tree_row = list_item
                .item()
                .and_then(|i| i.downcast::<gtk::TreeListRow>().ok());
            let expander = list_item
                .child()
                .and_then(|c| c.downcast::<gtk::TreeExpander>().ok());
            let (Some(tree_row), Some(expander)) = (tree_row, expander) else {
                return;
            };
            expander.set_list_row(Some(&tree_row));

            let Some(boxed) = tree_row
                .item()
                .and_then(|i| i.downcast::<BoxedAnyObject>().ok())
            else {
                return;
            };
            let item: std::cell::Ref<TreeItem> = boxed.borrow();
            let hbox = expander.child().and_then(|c| c.downcast::<gtk::Box>().ok());
            let Some(hbox) = hbox else { return };
            let icon = hbox.first_child().and_then(|w| w.downcast::<gtk::Image>().ok());
            let name_label = icon
                .as_ref()
                .and_then(|i| i.next_sibling())
                .and_then(|w| w.downcast::<gtk::Label>().ok());
            let page_label = name_label
                .as_ref()
                .and_then(|l| l.next_sibling())
                .and_then(|w| w.downcast::<gtk::Label>().ok());
            if let Some(icon) = icon {
                icon.set_icon_name(Some(item.icon_name()));
            }
            if let Some(name_label) = name_label {
                if item.is_folder() {
                    name_label.set_markup(&format!(
                        "<b>{}</b>",
                        glib::markup_escape_text(&item.name)
                    ));
                } else {
                    name_label.set_text(&item.name);
                }
            }
            if let Some(page_label) = page_label {
                match item.page_count {
                    Some(n) if n > 0 => page_label.set_text(&format!("{n} pages")),
                    _ => page_label.set_text(""),
                }
            }
        });

        let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list_view.add_css_class("navigation-sidebar");

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&list_view);
        widget.set_vexpand(true);
        widget.set_hexpand(true);

        let selected_callback: SelectedCallback = Rc::new(RefCell::new(None));
        let cb_for_signal = selected_callback.clone();
        let selection_for_signal = selection.clone();
        selection.connect_selection_changed(move |_, _, _| {
            let Some(row) = selection_for_signal
                .selected_item()
                .and_then(|i| i.downcast::<gtk::TreeListRow>().ok())
            else {
                return;
            };
            let Some(boxed) = row
                .item()
                .and_then(|i| i.downcast::<BoxedAnyObject>().ok())
            else {
                return;
            };
            let item: std::cell::Ref<TreeItem> = boxed.borrow();
            if item.is_folder() {
                return;
            }
            if let Some(cb) = cb_for_signal.borrow().as_ref() {
                cb(item.uuid.clone());
            }
        });

        Self {
            widget,
            root_store,
            selection,
            tree_model,
            selected_callback,
        }
    }

    pub fn load_tree(&self, tree: &DocumentTree) {
        self.root_store.remove_all();
        for root in &tree.roots {
            self.root_store
                .append(&BoxedAnyObject::new(TreeItem::from_node(root)));
        }
    }

    pub fn clear(&self) {
        self.root_store.remove_all();
    }

    pub fn connect_document_selected<F>(&self, callback: F)
    where
        F: Fn(String) + 'static,
    {
        *self.selected_callback.borrow_mut() = Some(Box::new(callback));
    }

    pub fn selected_uuid(&self) -> Option<String> {
        let row = self
            .selection
            .selected_item()?
            .downcast::<gtk::TreeListRow>()
            .ok()?;
        let boxed = row.item()?.downcast::<BoxedAnyObject>().ok()?;
        let item: std::cell::Ref<TreeItem> = boxed.borrow();
        if item.is_folder() {
            None
        } else {
            Some(item.uuid.clone())
        }
    }

    pub fn expand_all(&self) {
        for i in 0..self.tree_model.n_items() {
            if let Some(row) = self
                .tree_model
                .item(i)
                .and_then(|o| o.downcast::<gtk::TreeListRow>().ok())
            {
                row.set_expanded(true);
            }
        }
    }

    pub fn collapse_all(&self) {
        for i in 0..self.tree_model.n_items() {
            if let Some(row) = self
                .tree_model
                .item(i)
                .and_then(|o| o.downcast::<gtk::TreeListRow>().ok())
            {
                row.set_expanded(false);
            }
        }
    }
}

impl Default for FolderBrowser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remarkable::metadata::{RemarkableContent, RemarkableMetadata};

    fn node(uuid: &str, name: &str, doc_type: &str, file_type: Option<&str>, pages: Option<u32>) -> DocumentNode {
        let md: RemarkableMetadata = serde_json::from_str(&format!(
            r#"{{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"{doc_type}","visibleName":"{name}"}}"#
        ))
        .unwrap();
        let content = file_type.map(|ft| RemarkableContent {
            file_type: Some(ft.to_string()),
            format_version: None,
            orientation: None,
            page_count: pages,
            pages: None,
            c_pages: None,
            text_scale: None,
        });
        DocumentNode {
            uuid: uuid.to_string(),
            metadata: md,
            content,
            children: vec![],
        }
    }

    #[test]
    fn tree_item_classifies_notebook() {
        let n = node("a", "nb", "DocumentType", Some("notebook"), Some(3));
        let t = TreeItem::from_node(&n);
        assert_eq!(t.item_type, TreeItemType::Notebook);
        assert_eq!(t.page_count, Some(3));
        assert!(!t.is_folder());
    }

    #[test]
    fn tree_item_classifies_pdf_and_epub() {
        let p = TreeItem::from_node(&node("a", "x", "DocumentType", Some("pdf"), None));
        assert_eq!(p.item_type, TreeItemType::Pdf);
        let e = TreeItem::from_node(&node("a", "x", "DocumentType", Some("epub"), None));
        assert_eq!(e.item_type, TreeItemType::Epub);
    }

    #[test]
    fn tree_item_classifies_folder() {
        let f = TreeItem::from_node(&node("a", "f", "CollectionType", None, None));
        assert_eq!(f.item_type, TreeItemType::Folder);
        assert!(f.is_folder());
    }

    #[test]
    fn icon_names_match_types() {
        let f = TreeItem::from_node(&node("a", "f", "CollectionType", None, None));
        assert_eq!(f.icon_name(), "folder-symbolic");
        let nb = TreeItem::from_node(&node("a", "n", "DocumentType", Some("notebook"), None));
        assert_eq!(nb.icon_name(), "document-edit-symbolic");
        let p = TreeItem::from_node(&node("a", "p", "DocumentType", Some("pdf"), None));
        assert_eq!(p.icon_name(), "application-pdf-symbolic");
    }

    #[test]
    fn tree_item_from_tree_preserves_structure() {
        let mut folder = node("f", "Folder", "CollectionType", None, None);
        folder.children.push(node("d1", "Doc", "DocumentType", Some("notebook"), Some(2)));
        let tree = DocumentTree { roots: vec![folder] };
        let items = TreeItem::from_tree(&tree);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_folder());
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].name, "Doc");
    }

    #[test]
    fn documents_have_no_children_even_if_nested() {
        // Defensive: classify_node ensures non-folders never carry kids.
        let mut nb = node("n", "x", "DocumentType", Some("notebook"), None);
        nb.children.push(node("c", "stray", "DocumentType", None, None));
        let t = TreeItem::from_node(&nb);
        assert!(t.children.is_empty());
    }
}

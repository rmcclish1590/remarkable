# Spec 16 — Folder/Document Browser Tree

**Layer:** 4 — UI Shell  
**Dependencies:** 02 (metadata parser), 15 (main window)  
**Estimated effort:** 2 hours  

## Objective

Implement the sidebar document browser that displays the reMarkable's folder hierarchy as an expandable tree, with icons distinguishing folders from notebooks from PDFs.

## Context

The reMarkable organizes documents in a virtual folder hierarchy. The metadata parser (Spec 02) reconstructs this as a `DocumentTree`. This spec renders that tree in the sidebar using GTK4's tree/list view widgets.

## Technical Requirements

### 1. Tree widget (`src/ui/folder_browser.rs`)

Use `GtkTreeListModel` + `GtkListView` with `GtkTreeExpander` (the GTK4 idiomatic approach — GTK4 replaced `GtkTreeView` with this model):

```rust
pub struct FolderBrowser {
    pub widget: gtk::Box,          // The root container to insert into the sidebar
    list_view: gtk::ListView,
    tree_model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
}

impl FolderBrowser {
    /// Create the browser widget.
    pub fn new() -> Self

    /// Load a document tree into the browser.
    pub fn load_tree(&self, tree: &DocumentTree)

    /// Clear the browser.
    pub fn clear(&self)

    /// Connect a callback for when a document is selected.
    pub fn connect_document_selected<F>(&self, callback: F)
    where
        F: Fn(String) + 'static,  // Receives the UUID of the selected document

    /// Get the currently selected document UUID, if any.
    pub fn selected_uuid(&self) -> Option<String>

    /// Expand all folders.
    pub fn expand_all(&self)

    /// Collapse all folders.
    pub fn collapse_all(&self)
}
```

### 2. Tree item model

```rust
/// Represents a single row in the tree view.
#[derive(Debug, Clone)]
pub struct TreeItem {
    pub uuid: String,
    pub name: String,
    pub item_type: TreeItemType,
    pub page_count: Option<u32>,
    pub last_modified: Option<u64>,
    pub children: Vec<TreeItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TreeItemType {
    Folder,
    Notebook,
    Pdf,
    Epub,
}
```

### 3. Row rendering

Each row in the tree should display:

```
[▶/▼] [Icon] Document Name                    [Page count]
```

- **Expander arrow:** `GtkTreeExpander` handles this for folders.
- **Icon:** Use system icon names:
  - Folder: `"folder-symbolic"`
  - Notebook: `"document-edit-symbolic"` or `"accessories-text-editor-symbolic"`
  - PDF: `"application-pdf-symbolic"` or `"document-symbolic"`
  - Epub: `"document-symbolic"`
- **Name:** Bold for folders, normal weight for documents.
- **Page count:** Right-aligned, dimmed text (e.g., "5 pages").

Use a `GtkSignalListItemFactory` to create and bind row widgets:

```rust
fn setup_row(item: &gtk::ListItem) {
    let expander = gtk::TreeExpander::new();
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let icon = gtk::Image::new();
    let label = gtk::Label::new(None);
    let page_label = gtk::Label::new(None);
    
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);
    page_label.set_halign(gtk::Align::End);
    page_label.add_css_class("dim-label");
    
    hbox.append(&icon);
    hbox.append(&label);
    hbox.append(&page_label);
    expander.set_child(Some(&hbox));
    item.set_child(Some(&expander));
}
```

### 4. Sorting

Within each folder level:
1. Folders first, sorted alphabetically.
2. Documents second, sorted alphabetically.

### 5. Selection handling

- Single selection mode.
- Clicking a folder expands/collapses it (does not trigger document view).
- Clicking a document emits the `document_selected` signal with the UUID.
- Double-clicking a folder expands it.

### 6. Convert DocumentTree → TreeListModel

Write a conversion function that takes the `DocumentTree` from Spec 02 and produces the `GListModel` hierarchy that `GtkTreeListModel` expects. Each `DocumentNode` becomes a `GObject` subclass or a `glib::BoxedAnyObject` wrapping `TreeItem`.

## Files to Create/Modify

- `src/ui/folder_browser.rs` — full implementation
- `src/ui/mod.rs` — export the module
- `src/ui/window.rs` — replace sidebar placeholder with `FolderBrowser`

## Test Strategy

1. **Empty tree** — load an empty `DocumentTree`, verify the browser shows "No documents" or empty state.
2. **Flat list** — load a tree with 3 documents (no folders), verify 3 rows appear.
3. **Nested folders** — load a tree with folders containing documents, verify expand/collapse works.
4. **Selection** — click a document, verify `selected_uuid()` returns the correct UUID.
5. **Sorting** — verify folders appear before documents within each level.
6. **Icon assignment** — verify notebooks get the edit icon, PDFs get the document icon.

## Acceptance Criteria

1. The sidebar displays the document tree with expandable folders.
2. Icons distinguish folders, notebooks, and PDFs.
3. Clicking a document emits a selection event with the UUID.
4. Folders sort before documents, both alphabetically.
5. Page counts are displayed for notebooks.
6. The tree loads from a `DocumentTree` struct.

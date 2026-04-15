//! Document viewer panel — continuous multi-page scroll (spec 21).
//!
//! Each page of the currently loaded notebook gets its own `GtkPicture`
//! showing the cached SVG under `{sync_dir}/.rmsync/cache/`. Pictures are
//! stacked vertically inside a `ScrolledWindow`; `GtkPicture::set_filename`
//! defers decoding until the picture becomes visible, giving effectively
//! lazy rendering without bespoke machinery. A scroll listener updates the
//! page counter based on viewport centre.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::glib;
use gtk::prelude::*;

use crate::remarkable::metadata::RemarkableContent;
use crate::remarkable::rm_parser::parse_rm_file;
use crate::remarkable::svg_renderer::render_page_to_svg;

const REMARKABLE_PAGE_WIDTH: i32 = 1404;
const REMARKABLE_PAGE_HEIGHT: i32 = 1872;
const INTER_PAGE_SPACING: i32 = 16;

#[derive(Debug)]
struct LoadedDocument {
    uuid: String,
    cache_paths: Vec<PathBuf>,
    page_widgets: Vec<gtk::Box>,
}

pub struct DocumentViewer {
    pub widget: gtk::Box,
    scroll: gtk::ScrolledWindow,
    pages_box: gtk::Box,
    placeholder: gtk::Label,
    stack: gtk::Stack,
    page_info_label: gtk::Label,
    current_doc: Rc<RefCell<Option<LoadedDocument>>>,
}

impl DocumentViewer {
    pub fn new() -> Self {
        let pages_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(INTER_PAGE_SPACING)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(16)
            .margin_end(16)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&pages_box)
            .build();
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);

        let placeholder = gtk::Label::builder()
            .label("Select a document from the sidebar to view it")
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .vexpand(true)
            .build();
        placeholder.add_css_class("dim-label");

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&placeholder, Some("placeholder"));
        stack.add_named(&scroll, Some("pages"));
        stack.set_visible_child_name("placeholder");
        stack.set_vexpand(true);
        stack.set_hexpand(true);

        let page_info_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Center)
            .margin_top(2)
            .margin_bottom(6)
            .build();
        page_info_label.add_css_class("dim-label");

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        widget.append(&stack);
        widget.append(&page_info_label);

        let current_doc: Rc<RefCell<Option<LoadedDocument>>> = Rc::new(RefCell::new(None));

        // Update page info as the user scrolls.
        let adj = scroll.vadjustment();
        let info_for_scroll = page_info_label.clone();
        let current_for_scroll = current_doc.clone();
        adj.connect_value_changed(move |adj| {
            if let Some(doc) = &*current_for_scroll.borrow() {
                let idx = visible_page_index(adj, &doc.page_widgets);
                info_for_scroll.set_text(&format!(
                    "Page {} of {}",
                    idx + 1,
                    doc.page_widgets.len()
                ));
            }
        });

        Self {
            widget,
            scroll,
            pages_box,
            placeholder,
            stack,
            page_info_label,
            current_doc,
        }
    }

    pub fn load_document(&self, uuid: &str, sync_dir: &Path) -> Result<()> {
        self.clear_pages_box();

        let raw = sync_dir.join("raw");
        let cache = sync_dir.join(".rmsync").join("cache");
        std::fs::create_dir_all(&cache).ok();

        let content_path = raw.join(format!("{uuid}.content"));
        let content = RemarkableContent::from_file(&content_path)
            .with_context(|| format!("reading {}", content_path.display()))?;
        let page_ids = content.pages.unwrap_or_default();

        let mut cache_paths = Vec::new();
        let mut page_widgets = Vec::new();
        for (i, page_id) in page_ids.iter().enumerate() {
            let rm_path = raw.join(uuid).join(format!("{page_id}.rm"));
            if !rm_path.exists() {
                continue;
            }
            let cache_path = cache.join(format!("{uuid}_{page_id}.svg"));
            if !cache_path.exists() {
                let bytes = std::fs::read(&rm_path)
                    .with_context(|| format!("reading {}", rm_path.display()))?;
                let page = parse_rm_file(&bytes).map_err(anyhow::Error::from)?;
                let svg = render_page_to_svg(&page);
                std::fs::write(&cache_path, svg.as_bytes())
                    .with_context(|| format!("writing {}", cache_path.display()))?;
            }
            let page_widget = build_page_widget(&cache_path, i + 1);
            self.pages_box.append(&page_widget);
            page_widgets.push(page_widget);
            cache_paths.push(cache_path);
        }

        if page_widgets.is_empty() {
            self.clear();
            return Ok(());
        }

        let total = page_widgets.len();
        self.page_info_label.set_text(&format!("Page 1 of {total}"));
        self.stack.set_visible_child_name("pages");
        *self.current_doc.borrow_mut() = Some(LoadedDocument {
            uuid: uuid.to_string(),
            cache_paths,
            page_widgets,
        });
        Ok(())
    }

    pub fn clear(&self) {
        self.clear_pages_box();
        *self.current_doc.borrow_mut() = None;
        self.page_info_label.set_text("");
        self.stack.set_visible_child_name("placeholder");
    }

    fn clear_pages_box(&self) {
        while let Some(child) = self.pages_box.first_child() {
            self.pages_box.remove(&child);
        }
    }

    pub fn current_uuid(&self) -> Option<String> {
        self.current_doc.borrow().as_ref().map(|d| d.uuid.clone())
    }

    pub fn page_count(&self) -> usize {
        self.current_doc
            .borrow()
            .as_ref()
            .map(|d| d.page_widgets.len())
            .unwrap_or(0)
    }

    pub fn scroll_to_page(&self, page_number: usize) {
        let Some(doc) = &*self.current_doc.borrow() else {
            return;
        };
        if page_number == 0 || page_number > doc.page_widgets.len() {
            return;
        }
        let widget = &doc.page_widgets[page_number - 1];
        let y = widget_y_in_box(widget);
        let adj = self.scroll.vadjustment();
        adj.set_value(y as f64);
    }

    pub fn current_visible_page(&self) -> usize {
        let Some(doc) = &*self.current_doc.borrow() else {
            return 0;
        };
        let adj = self.scroll.vadjustment();
        visible_page_index(&adj, &doc.page_widgets) + 1
    }
}

impl Default for DocumentViewer {
    fn default() -> Self {
        Self::new()
    }
}

fn build_page_widget(svg_path: &Path, page_number: usize) -> gtk::Box {
    let picture = gtk::Picture::for_filename(svg_path);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    // Lock the intrinsic aspect so the scroll extent is correct before the
    // SVG decodes.
    picture.set_height_request(600);

    let separator = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Fill)
        .build();
    let left_rule = gtk::Separator::new(gtk::Orientation::Horizontal);
    left_rule.set_hexpand(true);
    left_rule.set_valign(gtk::Align::Center);
    let label = gtk::Label::new(Some(&format!("Page {page_number}")));
    label.add_css_class("dim-label");
    let right_rule = gtk::Separator::new(gtk::Orientation::Horizontal);
    right_rule.set_hexpand(true);
    right_rule.set_valign(gtk::Align::Center);
    separator.append(&left_rule);
    separator.append(&label);
    separator.append(&right_rule);

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    page.append(&separator);
    page.append(&picture);
    page
}

fn widget_y_in_box(widget: &gtk::Box) -> i32 {
    widget.allocation().y()
}

fn visible_page_index(adj: &gtk::Adjustment, pages: &[gtk::Box]) -> usize {
    if pages.is_empty() {
        return 0;
    }
    let target = adj.value() + adj.page_size() / 2.0;
    let mut best = 0usize;
    let mut best_distance = f64::MAX;
    for (i, w) in pages.iter().enumerate() {
        let alloc = w.allocation();
        let centre = alloc.y() as f64 + alloc.height() as f64 / 2.0;
        let d = (centre - target).abs();
        if d < best_distance {
            best_distance = d;
            best = i;
        }
    }
    best
}

pub const REMARKABLE_PAGE_ASPECT: (i32, i32) = (REMARKABLE_PAGE_WIDTH, REMARKABLE_PAGE_HEIGHT);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_ratio_is_reMarkable_native() {
        assert_eq!(REMARKABLE_PAGE_ASPECT, (1404, 1872));
    }

    #[test]
    fn inter_page_spacing_is_positive() {
        assert!(INTER_PAGE_SPACING > 0);
    }
}

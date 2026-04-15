//! Document viewer panel — renders rendered SVG pages for the selected notebook.
//!
//! Spec 20 — single-page mode. `DocumentViewer::load_document` parses
//! `.content` + each referenced `.rm` page, renders to SVG (cached under
//! `{sync_dir}/.rmsync/cache/`), and displays one page at a time inside a
//! `GtkPicture` with `ContentFit::Contain`. Prev/Next buttons + keyboard
//! arrows navigate.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::remarkable::metadata::RemarkableContent;
use crate::remarkable::rm_parser::{parse_rm_file, RmPage};
use crate::remarkable::svg_renderer::render_page_to_svg;

const REMARKABLE_PAGE_WIDTH: i32 = 1404;
const REMARKABLE_PAGE_HEIGHT: i32 = 1872;

#[derive(Debug)]
struct LoadedDocument {
    uuid: String,
    name: String,
    pages: Vec<RmPage>,
    cache_paths: Vec<PathBuf>,
    current_page: usize,
}

pub struct DocumentViewer {
    pub widget: gtk::Box,
    picture: gtk::Picture,
    placeholder: gtk::Label,
    page_info_label: gtk::Label,
    prev_button: gtk::Button,
    next_button: gtk::Button,
    current_doc: Rc<RefCell<Option<LoadedDocument>>>,
}

impl DocumentViewer {
    pub fn new() -> Self {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        picture.set_hexpand(true);

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
        stack.add_named(&picture, Some("picture"));
        stack.set_visible_child_name("placeholder");
        stack.set_vexpand(true);
        stack.set_hexpand(true);

        let page_info_label = gtk::Label::new(Some(""));
        let prev_button = gtk::Button::builder()
            .label("◀ Prev")
            .sensitive(false)
            .build();
        let next_button = gtk::Button::builder()
            .label("Next ▶")
            .sensitive(false)
            .build();
        let nav_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(8)
            .halign(gtk::Align::Center)
            .build();
        nav_box.append(&prev_button);
        nav_box.append(&page_info_label);
        nav_box.append(&next_button);

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        widget.append(&stack);
        widget.append(&nav_box);

        let current_doc: Rc<RefCell<Option<LoadedDocument>>> = Rc::new(RefCell::new(None));

        // Navigation wiring
        {
            let current_doc_for_prev = current_doc.clone();
            let picture_for_prev = picture.clone();
            let info_for_prev = page_info_label.clone();
            let prev_btn_clone = prev_button.clone();
            let next_btn_clone = next_button.clone();
            prev_button.connect_clicked(move |_| {
                navigate(
                    &current_doc_for_prev,
                    &picture_for_prev,
                    &info_for_prev,
                    &prev_btn_clone,
                    &next_btn_clone,
                    -1,
                );
            });
        }
        {
            let current_doc_for_next = current_doc.clone();
            let picture_for_next = picture.clone();
            let info_for_next = page_info_label.clone();
            let prev_btn_clone = prev_button.clone();
            let next_btn_clone = next_button.clone();
            next_button.connect_clicked(move |_| {
                navigate(
                    &current_doc_for_next,
                    &picture_for_next,
                    &info_for_next,
                    &prev_btn_clone,
                    &next_btn_clone,
                    1,
                );
            });
        }
        // Keyboard arrow nav on the root widget
        let key_controller = gtk::EventControllerKey::new();
        let current_doc_for_key = current_doc.clone();
        let picture_for_key = picture.clone();
        let info_for_key = page_info_label.clone();
        let prev_for_key = prev_button.clone();
        let next_for_key = next_button.clone();
        key_controller.connect_key_pressed(move |_, key, _, _| {
            let delta = match key {
                gdk::Key::Left => -1,
                gdk::Key::Right => 1,
                _ => return glib::Propagation::Proceed,
            };
            navigate(
                &current_doc_for_key,
                &picture_for_key,
                &info_for_key,
                &prev_for_key,
                &next_for_key,
                delta,
            );
            glib::Propagation::Stop
        });
        widget.add_controller(key_controller);

        Self {
            widget,
            picture,
            placeholder,
            page_info_label,
            prev_button,
            next_button,
            current_doc,
        }
    }

    /// Load a document from `{sync_dir}/raw/{uuid}*` and display its first
    /// page. Renders pages to SVG on first load and caches to disk.
    pub fn load_document(&self, uuid: &str, sync_dir: &Path) -> Result<()> {
        let raw = sync_dir.join("raw");
        let cache = sync_dir.join(".rmsync").join("cache");
        std::fs::create_dir_all(&cache).ok();

        let content_path = raw.join(format!("{uuid}.content"));
        let content = RemarkableContent::from_file(&content_path)
            .with_context(|| format!("reading {}", content_path.display()))?;
        let page_ids = content.pages.unwrap_or_default();

        let mut pages = Vec::new();
        let mut cache_paths = Vec::new();
        for page_id in &page_ids {
            let rm_path = raw.join(uuid).join(format!("{page_id}.rm"));
            if !rm_path.exists() {
                continue;
            }
            let bytes = std::fs::read(&rm_path)
                .with_context(|| format!("reading {}", rm_path.display()))?;
            let page = parse_rm_file(&bytes).map_err(anyhow::Error::from)?;
            let cache_path = cache.join(format!("{uuid}_{page_id}.svg"));
            if !cache_path.exists() {
                let svg = render_page_to_svg(&page);
                std::fs::write(&cache_path, svg.as_bytes())
                    .with_context(|| format!("writing {}", cache_path.display()))?;
            }
            pages.push(page);
            cache_paths.push(cache_path);
        }

        if pages.is_empty() {
            self.clear();
            return Ok(());
        }

        let doc = LoadedDocument {
            uuid: uuid.to_string(),
            name: uuid.to_string(),
            pages,
            cache_paths,
            current_page: 0,
        };

        display_page(&self.picture, &doc.cache_paths[0]);
        update_navigation(&doc, &self.page_info_label, &self.prev_button, &self.next_button);
        show_picture(&self.widget);
        *self.current_doc.borrow_mut() = Some(doc);
        Ok(())
    }

    pub fn clear(&self) {
        *self.current_doc.borrow_mut() = None;
        self.page_info_label.set_text("");
        self.prev_button.set_sensitive(false);
        self.next_button.set_sensitive(false);
        show_placeholder(&self.widget);
    }

    pub fn current_uuid(&self) -> Option<String> {
        self.current_doc.borrow().as_ref().map(|d| d.uuid.clone())
    }

    pub fn page_count(&self) -> usize {
        self.current_doc
            .borrow()
            .as_ref()
            .map(|d| d.pages.len())
            .unwrap_or(0)
    }

    pub fn current_page_index(&self) -> Option<usize> {
        self.current_doc.borrow().as_ref().map(|d| d.current_page)
    }
}

impl Default for DocumentViewer {
    fn default() -> Self {
        Self::new()
    }
}

fn display_page(picture: &gtk::Picture, svg_path: &Path) {
    picture.set_filename(Some(svg_path));
    picture.set_size_request(-1, -1);
}

fn navigate(
    current_doc: &Rc<RefCell<Option<LoadedDocument>>>,
    picture: &gtk::Picture,
    info_label: &gtk::Label,
    prev: &gtk::Button,
    next: &gtk::Button,
    delta: i32,
) {
    let Some(doc) = &mut *current_doc.borrow_mut() else {
        return;
    };
    let new_idx = (doc.current_page as i32 + delta).max(0) as usize;
    if new_idx >= doc.pages.len() {
        return;
    }
    doc.current_page = new_idx;
    display_page(picture, &doc.cache_paths[new_idx]);
    update_navigation(doc, info_label, prev, next);
}

fn update_navigation(
    doc: &LoadedDocument,
    info: &gtk::Label,
    prev: &gtk::Button,
    next: &gtk::Button,
) {
    info.set_text(&format!(
        "Page {} of {}",
        doc.current_page + 1,
        doc.pages.len()
    ));
    prev.set_sensitive(doc.current_page > 0);
    next.set_sensitive(doc.current_page + 1 < doc.pages.len());
}

fn show_placeholder(widget: &gtk::Box) {
    if let Some(stack) = widget.first_child().and_then(|w| w.downcast::<gtk::Stack>().ok()) {
        stack.set_visible_child_name("placeholder");
    }
}

fn show_picture(widget: &gtk::Box) {
    if let Some(stack) = widget.first_child().and_then(|w| w.downcast::<gtk::Stack>().ok()) {
        stack.set_visible_child_name("picture");
    }
}

pub const REMARKABLE_PAGE_ASPECT: (i32, i32) = (REMARKABLE_PAGE_WIDTH, REMARKABLE_PAGE_HEIGHT);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_ratio_is_reMarkable_native() {
        // 1404 x 1872 is the device viewport.
        assert_eq!(REMARKABLE_PAGE_ASPECT, (1404, 1872));
    }
}

//! Document viewer panel — continuous multi-page scroll (spec 21).
//! Error handling and diagnostic feedback (spec 27).
//!
//! Each page of the currently loaded notebook gets its own `GtkPicture`
//! showing the cached SVG under `{sync_dir}/.rmsync/cache/`. Pictures are
//! stacked vertically inside a `ScrolledWindow`; `GtkPicture::set_filename`
//! defers decoding until the picture becomes visible, giving effectively
//! lazy rendering without bespoke machinery. A scroll listener updates the
//! page counter based on viewport centre.
//!
//! Selecting a document always lands on page 1 at the very top, regardless
//! of where the previously open document was scrolled to (MCC-52).
//!
//! Rasterizing a cached SVG (usvg parse + tiny-skia render) dominates
//! page-open time — tens to hundreds of milliseconds per page, all on the
//! GTK main thread, made a multi-page notebook feel frozen while opening
//! (MCC-53). `load_document` now rasterizes only the first page inline
//! (so the viewer never shows a blank page 1) and hands the rest to a
//! background thread pool; each `GtkPicture` starts as a correctly sized
//! placeholder and gets its texture filled in as results arrive over
//! `raster_rx`. A load generation counter (`load_generation`) discards
//! results from a document the user has since navigated away from.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::remarkable::metadata::RemarkableContent;
use crate::remarkable::rm_parser::parse_rm_file;
use crate::remarkable::svg_renderer::{render_page_to_svg, RENDER_VERSION};
use crate::sync::transfer::{is_safe_component, is_safe_uuid};

const REMARKABLE_PAGE_WIDTH: i32 = 1404;
const REMARKABLE_PAGE_HEIGHT: i32 = 1872;
const INTER_PAGE_SPACING: i32 = 16;

#[derive(Debug)]
struct LoadedDocument {
    uuid: String,
    page_widgets: Vec<gtk::Box>,
}

#[derive(Clone)]
pub struct DocumentViewer {
    pub widget: gtk::Box,
    scroll: gtk::ScrolledWindow,
    pages_box: gtk::Box,
    error_heading: gtk::Label,
    error_detail: gtk::Label,
    stack: gtk::Stack,
    page_info_label: gtk::Label,
    current_doc: Rc<RefCell<Option<LoadedDocument>>>,
    /// Set by `load_document` so the first re-layout after the new pages
    /// are attached snaps the viewport back to the top. See
    /// `reset_scroll_to_top`.
    pending_top_reset: Rc<Cell<bool>>,
    /// Incremented on every `load_document`/`clear`. Tags each background
    /// rasterization job so a result that arrives after the user has
    /// opened a different document (or closed the viewer) is dropped
    /// instead of being drawn into the wrong picture slot.
    load_generation: Rc<Cell<u64>>,
    /// The pictures belonging to the document currently on screen, in
    /// page order — index-addressable so a background result (tagged
    /// with its page index) can find the right widget to fill in.
    current_pictures: Rc<RefCell<Vec<gtk::Picture>>>,
    /// Sender half of the background-rasterization channel; cloned into
    /// each worker thread `load_document` spawns.
    raster_tx: async_channel::Sender<RasterizedPage>,
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

        // Error pane — shown when load_document fails entirely.
        let error_icon = gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .pixel_size(48)
            .margin_bottom(12)
            .build();
        let error_heading = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .build();
        error_heading.add_css_class("title-3");
        let error_detail = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .wrap(true)
            .selectable(true)
            .build();
        error_detail.add_css_class("dim-label");
        let error_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .vexpand(true)
            .margin_start(32)
            .margin_end(32)
            .build();
        error_box.append(&error_icon);
        error_box.append(&error_heading);
        error_box.append(&error_detail);

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&placeholder, Some("placeholder"));
        stack.add_named(&scroll, Some("pages"));
        stack.add_named(&error_box, Some("error"));
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

        // Snapping to the top in `load_document` is not enough on its own:
        // at that moment the new page widgets have not been allocated, so
        // the scrolled window's `upper` still describes the *previous*
        // document. When layout catches up, GTK re-emits `changed` with the
        // new extents and the stale offset would resurface. The flag makes
        // the reset survive that first re-layout.
        let pending_top_reset = Rc::new(Cell::new(false));
        let pending_for_changed = pending_top_reset.clone();
        let hadj_for_changed = scroll.hadjustment();
        scroll.vadjustment().connect_changed(move |adj| {
            if pending_for_changed.get() {
                pending_for_changed.set(false);
                scroll_adjustments_to_top(&hadj_for_changed, adj);
            }
        });

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

        let load_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let current_pictures: Rc<RefCell<Vec<gtk::Picture>>> = Rc::new(RefCell::new(Vec::new()));

        // Background rasterization results land here. This channel lives
        // for the lifetime of the viewer — each load_document call reuses
        // it and relies on `load_generation` to discard stale results
        // rather than tearing the channel down and rebuilding it per
        // document.
        let (raster_tx, raster_rx) = async_channel::unbounded::<RasterizedPage>();
        {
            let load_generation = load_generation.clone();
            let current_pictures = current_pictures.clone();
            glib::spawn_future_local(async move {
                while let Ok(result) = raster_rx.recv().await {
                    if result.generation != load_generation.get() {
                        continue; // stale: user moved on to a different document
                    }
                    let pictures = current_pictures.borrow();
                    let Some(picture) = pictures.get(result.index) else {
                        continue;
                    };
                    let texture = rgba_to_texture(result.width, result.height, &result.rgba);
                    picture.set_paintable(Some(&texture));
                }
            });
        }

        Self {
            widget,
            scroll,
            pages_box,
            error_heading,
            error_detail,
            stack,
            page_info_label,
            current_doc,
            pending_top_reset,
            load_generation,
            current_pictures,
            raster_tx,
        }
    }

    /// Show an error message inside the viewer pane (no dialog needed).
    pub fn show_error(&self, heading: &str, detail: &str) {
        self.error_heading.set_text(heading);
        self.error_detail.set_text(detail);
        self.page_info_label.set_text("");
        self.stack.set_visible_child_name("error");
    }

    /// Load and display a document from the local sync directory.
    /// Resilient: attempts every page and accumulates per-page failures
    /// instead of aborting on the first error.
    pub fn load_document(&self, uuid: &str, sync_dir: &Path) -> Result<()> {
        self.clear_pages_box();

        if !is_safe_uuid(uuid) {
            self.show_error("Invalid document", "This document's identifier is invalid.");
            return Ok(());
        }

        let raw = sync_dir.join("raw");
        let cache = sync_dir.join(".rmsync").join("cache");
        std::fs::create_dir_all(&cache)
            .with_context(|| format!("creating cache dir {}", cache.display()))?;
        ensure_cache_version(&cache)
            .with_context(|| format!("refreshing render cache {}", cache.display()))?;

        let content_path = raw.join(format!("{uuid}.content"));
        let content = RemarkableContent::from_file(&content_path)
            .with_context(|| format!("reading {}", content_path.display()))?;
        let page_ids = content.page_ids();
        tracing::info!(
            uuid,
            pages = page_ids.len(),
            content = %content_path.display(),
            "opening document"
        );

        if page_ids.is_empty() {
            self.show_error(
                "No pages found",
                "This document's .content file lists zero pages.",
            );
            return Ok(());
        }

        // Bump the load generation before anything else so any background
        // rasterization results still in flight for the previous document
        // (or a document the user never finished opening) are discarded by
        // the receiver loop in `new()` rather than landing in this one's
        // pictures.
        let generation = self.load_generation.get() + 1;
        self.load_generation.set(generation);

        let mut page_widgets = Vec::new();
        let mut pictures = Vec::new();
        let mut background_jobs: Vec<(usize, PathBuf)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for (i, page_id) in page_ids.iter().enumerate() {
            // page_id comes from the tablet-supplied .content JSON — a
            // compromised/malicious tablet could embed path-traversal
            // sequences here, so it must be validated as a single safe path
            // component before it's used to build any filesystem path.
            if !is_safe_component(page_id) {
                failures.push(format!("page {}: invalid page id in document metadata", i + 1));
                continue;
            }
            let rm_path = raw.join(uuid).join(format!("{page_id}.rm"));
            if !rm_path.starts_with(&raw) {
                failures.push(format!("page {}: rejected unsafe page path", i + 1));
                continue;
            }
            if !rm_path.exists() {
                failures.push(format!("page {}: .rm file not found", i + 1));
                continue;
            }
            let cache_path = cache.join(format!("{uuid}_{page_id}.svg"));
            if !cache_path.starts_with(&cache) {
                failures.push(format!("page {}: rejected unsafe cache path", i + 1));
                continue;
            }
            if !cache_path.exists() {
                match render_and_cache(&rm_path, &cache_path) {
                    Ok(()) => {}
                    Err(e) => {
                        // {:#} prints the whole context chain — the outer
                        // context alone ("rmscene fallback for …") hides
                        // the actual reason a page failed to render.
                        failures.push(format!("page {}: {e:#}", i + 1));
                        continue;
                    }
                }
            }
            if !cache_path.exists() {
                failures.push(format!("page {}: SVG cache missing after render", i + 1));
                continue;
            }

            // Rasterizing (usvg parse + tiny-skia render) is the expensive
            // part of opening a page — see the module doc. The very first
            // rendered page is rasterized inline so the viewer never shows
            // page 1 blank; every later page gets a correctly sized
            // placeholder now and is filled in by a background worker.
            let render_index = pictures.len();
            let (page_widget, picture) = if render_index == 0 {
                let (widget, picture) = build_page_widget(&cache_path, i + 1);
                (widget, picture)
            } else {
                build_placeholder_page_widget(&cache_path, i + 1)
            };
            self.pages_box.append(&page_widget);
            page_widgets.push(page_widget);
            pictures.push(picture);
            if render_index != 0 {
                background_jobs.push((render_index, cache_path));
            }
        }

        // Publish the picture slots before dispatching workers — a result
        // arriving before this point would find nothing to update against.
        *self.current_pictures.borrow_mut() = pictures;
        spawn_rasterization_workers(background_jobs, generation, self.raster_tx.clone());

        let rendered = page_widgets.len();
        let total = page_ids.len();

        if rendered == 0 {
            tracing::error!(
                uuid,
                pages = total,
                failures = failures.join("; "),
                "no pages could be rendered"
            );
            let detail = if failures.is_empty() {
                "All page .rm files are missing from the sync directory.".to_string()
            } else {
                failures.join("\n")
            };
            self.show_error(
                &format!("Could not render any of {total} pages"),
                &detail,
            );
            return Ok(());
        }

        if !failures.is_empty() {
            tracing::warn!(
                uuid,
                rendered,
                total,
                failures = failures.join("; "),
                "some pages could not be rendered"
            );
            let warning = gtk::Label::builder()
                .label(format!(
                    "⚠ {rendered} of {total} pages rendered — {} failed:\n{}",
                    failures.len(),
                    failures.join("\n")
                ))
                .wrap(true)
                .halign(gtk::Align::Start)
                .margin_start(16)
                .margin_end(16)
                .margin_bottom(8)
                .build();
            warning.add_css_class("warning");
            self.pages_box.prepend(&warning);
        }

        self.page_info_label
            .set_text(&format!("Page 1 of {rendered}"));
        self.stack.set_visible_child_name("pages");
        *self.current_doc.borrow_mut() = Some(LoadedDocument {
            uuid: uuid.to_string(),
            page_widgets,
        });
        self.reset_scroll_to_top();
        Ok(())
    }

    /// Put the viewport back at the top-left of page 1.
    ///
    /// Runs in three beats because a scrolled window's extents lag its
    /// content by one layout pass: zero the adjustments now (covers the
    /// case where the new document has the same extents as the old one,
    /// so no `changed` is ever emitted), arm the `changed` handler
    /// installed in `new()` for the re-layout, and disarm it on the next
    /// idle so a later window resize cannot yank the user back to the top.
    fn reset_scroll_to_top(&self) {
        scroll_adjustments_to_top(&self.scroll.hadjustment(), &self.scroll.vadjustment());
        self.pending_top_reset.set(true);

        let pending = self.pending_top_reset.clone();
        let scroll = self.scroll.clone();
        glib::idle_add_local_once(move || {
            if pending.replace(false) {
                scroll_adjustments_to_top(&scroll.hadjustment(), &scroll.vadjustment());
            }
        });
    }

    pub fn clear(&self) {
        self.clear_pages_box();
        self.pending_top_reset.set(false);
        scroll_adjustments_to_top(&self.scroll.hadjustment(), &self.scroll.vadjustment());
        // Invalidate any background rasterization still running for the
        // document being closed, and drop the picture slots it would
        // otherwise try to fill in.
        self.load_generation.set(self.load_generation.get() + 1);
        self.current_pictures.borrow_mut().clear();
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

/// Default render width in pixels. The reMarkable's native viewport is
/// 1404×1872; rendering at 800px keeps pages crisp while using ~2.5 MB
/// per page instead of ~10 MB at full resolution.
const DEFAULT_RENDER_WIDTH: u32 = 800;

fn svg_to_texture(svg_path: &Path) -> Result<gdk::MemoryTexture> {
    svg_to_texture_scaled(svg_path, DEFAULT_RENDER_WIDTH)
}

/// A background worker's rasterization result, tagged with the document
/// generation and page slot it belongs to. `rgba` is raw
/// R8g8b8a8Premultiplied pixel data — building the actual `GdkTexture`
/// happens back on the main thread in the receiver loop installed in
/// `DocumentViewer::new`, since GDK/GTK types are not `Send`.
struct RasterizedPage {
    generation: u64,
    index: usize,
    width: i32,
    height: i32,
    rgba: Vec<u8>,
}

fn rgba_to_texture(width: i32, height: i32, rgba: &[u8]) -> gdk::MemoryTexture {
    let bytes = glib::Bytes::from(rgba);
    gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (width * 4) as usize,
    )
}

/// Parse and rasterize `svg_path` to raw pixel data at `target_width`.
/// Pure computation over `resvg`/`tiny-skia` types only — no GTK/GDK
/// involved — so it is safe to run on a background thread.
fn rasterize_svg(svg_path: &Path, target_width: u32) -> Result<(i32, i32, Vec<u8>)> {
    let svg_bytes = std::fs::read(svg_path)
        .with_context(|| format!("reading {}", svg_path.display()))?;
    let options = resvg::usvg::Options {
        fontdb: shared_fontdb(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(&svg_bytes, &options)
        .with_context(|| "parsing SVG")?;
    let size = tree.size();
    let scale = target_width as f32 / size.width();
    let width = target_width;
    let height = (size.height() * scale) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("pixmap allocation failed ({width}x{height})"))?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok((width as i32, height as i32, pixmap.data().to_vec()))
}

/// Read just enough of a cached SVG (our own renderer always puts
/// `viewBox` in the opening tag) to size a placeholder picture before
/// rasterizing it — a full read-and-parse would cost as much as the
/// rasterization we're trying to defer to a background thread.
fn peek_svg_dimensions(svg_path: &Path) -> Option<(f64, f64)> {
    use std::io::Read;
    let mut buf = [0u8; 512];
    let n = std::fs::File::open(svg_path).ok()?.read(&mut buf).ok()?;
    let header = std::str::from_utf8(&buf[..n]).ok()?;
    let start = header.find("viewBox=\"")? + "viewBox=\"".len();
    let end = start + header[start..].find('"')?;
    let mut fields = header[start..end].split_whitespace();
    let (_x, _y) = (fields.next()?, fields.next()?);
    let width: f64 = fields.next()?.parse().ok()?;
    let height: f64 = fields.next()?.parse().ok()?;
    if width > 0.0 && height > 0.0 {
        Some((width, height))
    } else {
        None
    }
}

/// Split `jobs` across a small pool of OS threads (bounded by available
/// parallelism, since a 20+ page notebook shouldn't spawn 20+ threads) and
/// rasterize each in the background, reporting results through `tx`.
/// `generation` lets the receiver in `DocumentViewer::new` recognize and
/// drop results for a document the viewer has since navigated away from.
fn spawn_rasterization_workers(
    jobs: Vec<(usize, PathBuf)>,
    generation: u64,
    tx: async_channel::Sender<RasterizedPage>,
) {
    if jobs.is_empty() {
        return;
    }
    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(jobs.len());
    let mut buckets: Vec<Vec<(usize, PathBuf)>> = (0..worker_count).map(|_| Vec::new()).collect();
    for (slot, job) in jobs.into_iter().enumerate() {
        buckets[slot % worker_count].push(job);
    }
    for bucket in buckets {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for (index, cache_path) in bucket {
                match rasterize_svg(&cache_path, DEFAULT_RENDER_WIDTH) {
                    Ok((width, height, rgba)) => {
                        let msg = RasterizedPage {
                            generation,
                            index,
                            width,
                            height,
                            rgba,
                        };
                        // Only fails if the receiver (the viewer itself)
                        // has been dropped, e.g. app shutdown mid-render.
                        let _ = tx.send_blocking(msg);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %cache_path.display(),
                            error = format!("{e:#}"),
                            "background page rasterization failed"
                        );
                    }
                }
            }
        });
    }
}

/// System fonts for rendering `<text>` elements (typed-text pages).
/// usvg's default font database is empty, which silently drops text.
/// Loading system fonts takes tens of milliseconds, so do it once.
///
/// The generic `sans-serif` family must also be pointed at a font that is
/// actually installed: fontdb's built-in default resolves to a face most
/// Linux systems don't have, and an unresolvable family makes usvg drop
/// the text as silently as an empty database does.
fn shared_fontdb() -> std::sync::Arc<resvg::usvg::fontdb::Database> {
    static DB: std::sync::OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
        std::sync::OnceLock::new();
    DB.get_or_init(|| {
        use resvg::usvg::fontdb;
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let candidates = [
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
            "Ubuntu",
            "Cantarell",
            "Arial",
        ];
        let installed = |name: &str| {
            db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                ..Default::default()
            })
            .is_some()
        };
        let family = candidates
            .iter()
            .find(|name| installed(name))
            .map(|name| name.to_string())
            .or_else(|| {
                // Last resort: any face at all beats invisible text.
                db.faces()
                    .next()
                    .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            });
        if let Some(family) = family {
            db.set_sans_serif_family(family);
        }
        std::sync::Arc::new(db)
    })
    .clone()
}

fn svg_to_texture_scaled(svg_path: &Path, target_width: u32) -> Result<gdk::MemoryTexture> {
    let (width, height, rgba) = rasterize_svg(svg_path, target_width)?;
    Ok(rgba_to_texture(width, height, &rgba))
}

fn render_and_cache(rm_path: &Path, cache_path: &Path) -> Result<()> {
    let bytes = std::fs::read(rm_path)
        .with_context(|| format!("reading {}", rm_path.display()))?;
    tracing::debug!(
        path = %rm_path.display(),
        bytes = bytes.len(),
        "rendering page"
    );
    match parse_rm_file(&bytes) {
        Ok(page) => {
            // The counts here are what distinguishes "the parser worked and
            // the page really is blank" from "the parser silently recovered
            // nothing" — the ambiguity at the heart of MCC-49/MCC-37.
            tracing::debug!(
                path = %rm_path.display(),
                version = page.version,
                layers = page.layers.len(),
                strokes = page.total_strokes(),
                points = page.total_points(),
                text_chars = page.text.as_ref().map(|t| t.char_count()).unwrap_or(0),
                "parsed .rm page"
            );
            let svg = render_page_to_svg(&page);
            // A page that yields no strokes may be using scene features the
            // native parser skips, so let rmscene try. Keep the native blank
            // render when it is unavailable — an empty page is not an error.
            if page.is_empty() {
                tracing::debug!(
                    path = %rm_path.display(),
                    "page has no content; trying rmscene fallback"
                );
                match render_via_rmscene(rm_path, cache_path) {
                    Ok(()) => {
                        tracing::debug!(path = %rm_path.display(), "rmscene fallback rendered page");
                        return Ok(());
                    }
                    Err(e) => tracing::debug!(
                        path = %rm_path.display(),
                        error = format!("{e:#}"),
                        "rmscene fallback unavailable; keeping the blank native render"
                    ),
                }
            }
            std::fs::write(cache_path, svg.as_bytes())
                .with_context(|| format!("writing {}", cache_path.display()))?;
            tracing::debug!(
                cache = %cache_path.display(),
                bytes = svg.len(),
                "cached rendered page"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                path = %rm_path.display(),
                error = %e,
                "native .rm parser failed; trying rmscene fallback"
            );
            // Keep the native error in the chain: when the fallback also
            // fails, "why the native parser gave up" is the diagnostic
            // that matters.
            render_via_rmscene(rm_path, cache_path).with_context(|| {
                format!(
                    "native parser failed ({e}) and rmscene fallback failed for {}",
                    rm_path.display()
                )
            })
        }
    }
}

/// Clear cached SVGs rendered by an older parser/renderer. Cached pages
/// are otherwise only re-rendered when the file is missing, which would
/// pin every already-viewed page to the old (possibly broken) output.
fn ensure_cache_version(cache_dir: &Path) -> Result<()> {
    let marker = cache_dir.join(".render-version");
    let current = RENDER_VERSION.to_string();
    if let Ok(stored) = std::fs::read_to_string(&marker) {
        if stored.trim() == current {
            return Ok(());
        }
    }
    let mut cleared = 0usize;
    for entry in std::fs::read_dir(cache_dir)
        .with_context(|| format!("reading cache dir {}", cache_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "svg") {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale cache {}", path.display()))?;
            cleared += 1;
        }
    }
    tracing::info!(
        cleared,
        render_version = RENDER_VERSION,
        dir = %cache_dir.display(),
        "cleared page cache rendered by an older version"
    );
    std::fs::write(&marker, &current)
        .with_context(|| format!("writing {}", marker.display()))?;
    Ok(())
}

fn render_via_rmscene(rm_path: &Path, cache_path: &Path) -> Result<()> {
    let script = find_rm_to_svg_script()?;
    let pythons = candidate_pythons();
    tracing::debug!(
        script = %script.display(),
        interpreters = pythons.join(", "),
        "invoking rmscene fallback"
    );
    let mut last_err = String::new();
    for python in &pythons {
        let result = std::process::Command::new(python)
            .arg(&script)
            .arg(rm_path)
            .arg(cache_path)
            .output();
        match result {
            Ok(output) if output.status.success() && cache_path.exists() => {
                tracing::debug!(python, "rmscene fallback succeeded");
                return Ok(());
            }
            Ok(output) => {
                last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                tracing::debug!(python, status = ?output.status.code(), error = %last_err,
                    "rmscene interpreter failed");
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::debug!(python, error = %last_err, "could not run interpreter");
            }
        }
    }
    anyhow::bail!(
        "rm_to_svg.py failed with all Python candidates ({}): {last_err}",
        pythons.join(", ")
    )
}

fn candidate_pythons() -> Vec<String> {
    // Only ever resolve interpreters from locations the current user owns
    // (their XDG data dir) or from PATH. A shared, world-writable path like
    // /tmp must never be trusted here: any local user could plant a binary
    // there and have it executed with rmsync's privileges the next time a
    // document triggers the rmscene fallback.
    let mut out = vec!["python3".to_string()];
    if let Some(home) = dirs::home_dir() {
        let venv = home.join(".local/share/rmsync/venv/bin/python3");
        if venv.exists() {
            out.insert(0, venv.to_string_lossy().into_owned());
        }
    }
    out
}

fn find_rm_to_svg_script() -> Result<std::path::PathBuf> {
    // 1. Next to the rmsync binary
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.parent().unwrap_or(Path::new(".")).join("rm_to_svg.py");
        if beside.exists() {
            return Ok(beside);
        }
    }
    // 2. Installed by the .deb package
    let installed = Path::new("/usr/share/rmsync/rm_to_svg.py");
    if installed.exists() {
        return Ok(installed.to_path_buf());
    }
    // 3. In the source tree's scripts/ directory (dev mode)
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dev = manifest_dir.join("scripts").join("rm_to_svg.py");
    if dev.exists() {
        return Ok(dev);
    }
    // 4. In PATH
    let which = std::process::Command::new("which")
        .arg("rm_to_svg.py")
        .output();
    if let Ok(out) = which {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return Ok(PathBuf::from(p));
        }
    }
    anyhow::bail!(
        "rm_to_svg.py not found. Install: pip install rmscene, then place \
         scripts/rm_to_svg.py next to the rmsync binary or in PATH."
    )
}

/// Build a page's widget (a labelled separator plus a `GtkPicture`)
/// around an already-created picture, and return both — callers need the
/// picture handle back so a background rasterization result can be drawn
/// into it later.
fn assemble_page_widget(picture: gtk::Picture, page_number: usize) -> gtk::Box {
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

/// Build a page immediately, with its texture rasterized inline. Used only
/// for the first page of a document, so the viewer never opens on a blank
/// page 1 while the rest render in the background.
fn build_page_widget(svg_path: &Path, page_number: usize) -> (gtk::Box, gtk::Picture) {
    let picture = match svg_to_texture(svg_path) {
        Ok(texture) => {
            let p = gtk::Picture::for_paintable(&texture);
            p.set_content_fit(gtk::ContentFit::Fill);
            p.set_can_shrink(true);
            p.set_hexpand(true);
            p.set_vexpand(false);
            // Set the height request to match the rendered texture's
            // aspect ratio at the current width allocation. The texture
            // is already scaled to DEFAULT_RENDER_WIDTH, so use its
            // actual pixel height as the request — this ensures each
            // page takes exactly the right vertical space.
            p.set_height_request(texture.height());
            p
        }
        Err(e) => {
            tracing::warn!("page {page_number}: SVG render failed: {e}");
            let p = gtk::Picture::new();
            p.set_hexpand(true);
            p.set_height_request(200);
            p
        }
    };
    let widget = assemble_page_widget(picture.clone(), page_number);
    (widget, picture)
}

/// Build a page's widget ahead of its texture being ready: sized correctly
/// (from the SVG's own `viewBox`, not a full rasterize — see
/// `peek_svg_dimensions`) so later layout doesn't jump, but with an empty
/// `GtkPicture` a background worker fills in once rasterization finishes.
fn build_placeholder_page_widget(svg_path: &Path, page_number: usize) -> (gtk::Box, gtk::Picture) {
    let height = peek_svg_dimensions(svg_path)
        .map(|(w, h)| (DEFAULT_RENDER_WIDTH as f64 * h / w) as i32)
        .unwrap_or_else(|| {
            (DEFAULT_RENDER_WIDTH as f64 * REMARKABLE_PAGE_HEIGHT as f64
                / REMARKABLE_PAGE_WIDTH as f64) as i32
        });

    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Fill);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(false);
    picture.set_height_request(height);

    let widget = assemble_page_widget(picture.clone(), page_number);
    (widget, picture)
}

/// Scroll both axes back to their lower bound (top-left of the content).
///
/// `lower` rather than a literal `0.0`: an adjustment's origin is not
/// required to be zero, and GTK clamps out-of-range values silently.
fn scroll_adjustments_to_top(hadj: &gtk::Adjustment, vadj: &gtk::Adjustment) {
    vadj.set_value(vadj.lower());
    hadj.set_value(hadj.lower());
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
    fn aspect_ratio_is_remarkable_native() {
        assert_eq!(REMARKABLE_PAGE_ASPECT, (1404, 1872));
    }

    // Regression tests for the path-traversal fix: page_id and uuid values
    // come from tablet-supplied JSON and must be rejected before they're
    // used to build filesystem paths. DocumentViewer itself needs a live
    // GTK display to construct, so these exercise the same validation
    // helpers load_document guards its path-building with.
    #[test]
    fn malicious_page_id_is_rejected_as_unsafe_component() {
        for bad in [
            "../../../../etc/passwd",
            "..",
            "a/../../b",
            "a/b",
            ".hidden",
            "evil\0name",
        ] {
            assert!(!is_safe_component(bad), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn legitimate_page_id_is_accepted_as_safe_component() {
        assert!(is_safe_component("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
    }

    #[test]
    fn malicious_uuid_is_rejected() {
        assert!(!is_safe_uuid("../../../etc/passwd"));
        assert!(!is_safe_uuid("abc/def"));
    }

    #[test]
    fn candidate_pythons_never_trusts_a_shared_tmp_path() {
        // Regression test: /tmp is world-writable, so any local user could
        // plant a binary there and have it executed with rmsync's
        // privileges via the rmscene fallback.
        for candidate in candidate_pythons() {
            assert!(
                !candidate.starts_with("/tmp"),
                "candidate python interpreter must not come from /tmp: {candidate}"
            );
        }
    }

    #[test]
    fn inter_page_spacing_matches_constant() {
        assert_eq!(INTER_PAGE_SPACING, 16);
    }

    #[test]
    fn cache_version_marker_is_written_on_first_use() {
        let td = tempfile::tempdir().unwrap();
        ensure_cache_version(td.path()).unwrap();
        let stored = std::fs::read_to_string(td.path().join(".render-version")).unwrap();
        assert_eq!(stored, RENDER_VERSION.to_string());
    }

    #[test]
    fn stale_cache_svgs_are_cleared_on_version_bump() {
        let td = tempfile::tempdir().unwrap();
        // A cache from an older build: no marker (or an old one), plus
        // rendered pages and the unrelated state files we must not touch.
        std::fs::write(td.path().join("abc_p1.svg"), "old render").unwrap();
        std::fs::write(td.path().join(".render-version"), "1").unwrap();
        std::fs::write(td.path().join("notes.txt"), "keep me").unwrap();

        ensure_cache_version(td.path()).unwrap();

        assert!(!td.path().join("abc_p1.svg").exists(), "stale SVG kept");
        assert!(td.path().join("notes.txt").exists(), "non-SVG removed");
        let stored = std::fs::read_to_string(td.path().join(".render-version")).unwrap();
        assert_eq!(stored, RENDER_VERSION.to_string());
    }

    #[test]
    fn current_cache_is_left_alone() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join(".render-version"), RENDER_VERSION.to_string()).unwrap();
        std::fs::write(td.path().join("abc_p1.svg"), "current render").unwrap();

        ensure_cache_version(td.path()).unwrap();

        assert!(td.path().join("abc_p1.svg").exists());
    }

    // MCC-53: page-open performance. peek_svg_dimensions is what lets a
    // background page get a correctly sized placeholder without paying for
    // a full rasterize up front — these pin its parsing behaviour.
    #[test]
    fn peek_svg_dimensions_reads_the_view_box() {
        let td = tempfile::tempdir().unwrap();
        let svg_path = td.path().join("page.svg");
        std::fs::write(
            &svg_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-100.0 0.0 1404.0 1872.0" width="1404" height="1872">"#,
        )
        .unwrap();

        let (w, h) = peek_svg_dimensions(&svg_path).unwrap();
        assert_eq!((w, h), (1404.0, 1872.0));
    }

    #[test]
    fn peek_svg_dimensions_rejects_a_degenerate_view_box() {
        // A zero-size viewBox would produce a zero-height placeholder;
        // callers must fall back to a default instead.
        let td = tempfile::tempdir().unwrap();
        let svg_path = td.path().join("page.svg");
        std::fs::write(
            &svg_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0.0 0.0 0.0 0.0" width="0" height="0">"#,
        )
        .unwrap();

        assert!(peek_svg_dimensions(&svg_path).is_none());
    }

    #[test]
    fn peek_svg_dimensions_returns_none_without_a_view_box() {
        let td = tempfile::tempdir().unwrap();
        let svg_path = td.path().join("page.svg");
        std::fs::write(&svg_path, "not an svg at all").unwrap();

        assert!(peek_svg_dimensions(&svg_path).is_none());
    }

    #[test]
    fn rasterize_svg_scales_to_the_requested_width() {
        let td = tempfile::tempdir().unwrap();
        let svg_path = td.path().join("page.svg");
        std::fs::write(
            &svg_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1404 1872" width="1404" height="1872"><rect x="0" y="0" width="1404" height="1872" fill="white"/></svg>"#,
        )
        .unwrap();

        let (width, height, rgba) = rasterize_svg(&svg_path, 400).unwrap();
        assert_eq!(width, 400);
        // 1872/1404 * 400, truncated the same way rasterize_svg computes it.
        assert_eq!(height, (1872.0 * (400.0 / 1404.0)) as i32);
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }

    #[test]
    fn spawn_rasterization_workers_is_a_noop_for_no_jobs() {
        // Must not spawn a zero-length thread pool or panic on an empty
        // notebook's background job list.
        let (tx, rx) = async_channel::unbounded();
        spawn_rasterization_workers(Vec::new(), 1, tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn spawn_rasterization_workers_reports_results_for_every_job() {
        let td = tempfile::tempdir().unwrap();
        let mut jobs = Vec::new();
        for i in 0..5 {
            let svg_path = td.path().join(format!("p{i}.svg"));
            std::fs::write(
                &svg_path,
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 200" width="100" height="200"></svg>"#,
            )
            .unwrap();
            jobs.push((i, svg_path));
        }

        let (tx, rx) = async_channel::unbounded();
        spawn_rasterization_workers(jobs, 7, tx);

        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            let result = rx.recv_blocking().unwrap();
            assert_eq!(result.generation, 7);
            // Workers always rasterize at DEFAULT_RENDER_WIDTH, regardless
            // of the source SVG's own width.
            assert_eq!(result.width, DEFAULT_RENDER_WIDTH as i32);
            seen.insert(result.index);
        }
        assert_eq!(seen, (0..5).collect());
    }
}

//! Document viewer panel — continuous multi-page scroll (spec 21).
//! Error handling and diagnostic feedback (spec 27).
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
            error_heading,
            error_detail,
            stack,
            page_info_label,
            current_doc,
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

        if page_ids.is_empty() {
            self.show_error(
                "No pages found",
                "This document's .content file lists zero pages.",
            );
            return Ok(());
        }

        let mut page_widgets = Vec::new();
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
            let page_widget = build_page_widget(&cache_path, i + 1);
            self.pages_box.append(&page_widget);
            page_widgets.push(page_widget);
        }

        let rendered = page_widgets.len();
        let total = page_ids.len();

        if rendered == 0 {
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

/// Default render width in pixels. The reMarkable's native viewport is
/// 1404×1872; rendering at 800px keeps pages crisp while using ~2.5 MB
/// per page instead of ~10 MB at full resolution.
const DEFAULT_RENDER_WIDTH: u32 = 800;

fn svg_to_texture(svg_path: &Path) -> Result<gdk::MemoryTexture> {
    svg_to_texture_scaled(svg_path, DEFAULT_RENDER_WIDTH)
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
    let bytes = glib::Bytes::from(pixmap.data());
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (width * 4) as usize,
    );
    Ok(texture)
}

fn render_and_cache(rm_path: &Path, cache_path: &Path) -> Result<()> {
    let bytes = std::fs::read(rm_path)
        .with_context(|| format!("reading {}", rm_path.display()))?;
    match parse_rm_file(&bytes) {
        Ok(page) => {
            let svg = render_page_to_svg(&page);
            // A page that yields no strokes may be using scene features the
            // native parser skips, so let rmscene try. Keep the native blank
            // render when it is unavailable — an empty page is not an error.
            if page.is_empty() && render_via_rmscene(rm_path, cache_path).is_ok() {
                return Ok(());
            }
            std::fs::write(cache_path, svg.as_bytes())
                .with_context(|| format!("writing {}", cache_path.display()))?;
            Ok(())
        }
        Err(e) => {
            tracing::debug!("native .rm parser failed ({e}), trying rmscene fallback");
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
    for entry in std::fs::read_dir(cache_dir)
        .with_context(|| format!("reading cache dir {}", cache_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "svg") {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale cache {}", path.display()))?;
        }
    }
    std::fs::write(&marker, &current)
        .with_context(|| format!("writing {}", marker.display()))?;
    Ok(())
}

fn render_via_rmscene(rm_path: &Path, cache_path: &Path) -> Result<()> {
    let script = find_rm_to_svg_script()?;
    let pythons = candidate_pythons();
    let mut last_err = String::new();
    for python in &pythons {
        let result = std::process::Command::new(python)
            .arg(&script)
            .arg(rm_path)
            .arg(cache_path)
            .output();
        match result {
            Ok(output) if output.status.success() && cache_path.exists() => return Ok(()),
            Ok(output) => {
                last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            }
            Err(e) => {
                last_err = e.to_string();
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

fn build_page_widget(svg_path: &Path, page_number: usize) -> gtk::Box {
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
}

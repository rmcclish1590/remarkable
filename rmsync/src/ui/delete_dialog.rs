//! Type-the-name confirmation before a document is deleted.
//!
//! The dialog only ever *reports* a confirmed request through `on_confirm`;
//! it touches no files and starts no sync. Cancel, Escape and the close
//! button all resolve to "cancel", and clicking outside a modal dialog does
//! nothing, so none of them can reach `on_confirm`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::ui::folder_browser::DeleteRequest;

/// The typed text counts only if it equals the displayed name exactly:
/// case-sensitive, no trimming.
pub fn name_matches(typed: &str, expected: &str) -> bool {
    typed == expected
}

/// Inline hint under the field, or `None` when nothing needs saying.
pub fn mismatch_hint(typed: &str, expected: &str) -> Option<&'static str> {
    if typed.is_empty() || typed == expected {
        return None;
    }
    if typed.trim() == expected || typed.to_lowercase() == expected.to_lowercase() {
        Some("Almost — capitalisation and spaces must match exactly.")
    } else {
        Some("That doesn't match the name yet.")
    }
}

/// The only combination that deletes: the "delete" response with a matching name.
pub fn should_delete(response: &str, typed: &str, expected: &str) -> bool {
    response == "delete" && name_matches(typed, expected)
}

thread_local! {
    /// At most one delete dialog may be open at a time.
    static OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Ask the user to type `request.name` before `on_confirm` runs with it.
/// Does nothing if a delete dialog is already open.
pub fn present_delete_dialog<F: Fn(DeleteRequest) + 'static>(
    window: &impl IsA<gtk::Window>,
    request: DeleteRequest,
    on_confirm: F,
) {
    if OPEN.with(|o| o.replace(true)) {
        return;
    }

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Delete this notebook?")
        .body_use_markup(true)
        .body(format!(
            "<b>“{}”</b> will be deleted from this computer and from your reMarkable. \
             This can't be undone from the app (a backup copy is kept on this computer).\n\n\
             Type the name exactly as shown to confirm:",
            glib::markup_escape_text(&request.name)
        ))
        .build();

    let entry = gtk::Entry::builder()
        .activates_default(false)
        .placeholder_text(&request.name)
        .build();
    let hint = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .css_classes(["error"])
        .visible(false)
        .build();
    let extra = gtk::Box::new(gtk::Orientation::Vertical, 6);
    extra.append(&entry);
    extra.append(&hint);
    dialog.set_extra_child(Some(&extra));

    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("delete", "_Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_response_enabled("delete", false);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let expected = request.name.clone();
    let typed = Rc::new(RefCell::new(String::new()));
    {
        let (dialog, hint, typed, expected) =
            (dialog.clone(), hint.clone(), typed.clone(), expected.clone());
        entry.connect_changed(move |e| {
            let text = e.text().to_string();
            dialog.set_response_enabled("delete", name_matches(&text, &expected));
            match mismatch_hint(&text, &expected) {
                Some(h) => {
                    hint.set_text(h);
                    hint.set_visible(true);
                }
                None => hint.set_visible(false),
            }
            *typed.borrow_mut() = text;
        });
    }

    dialog.connect_response(None, move |d, response| {
        let confirmed = should_delete(response, &typed.borrow(), &expected);
        d.close();
        OPEN.with(|o| o.set(false));
        if confirmed {
            on_confirm(request.clone());
        }
    });
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_enables_deletion() {
        assert!(name_matches("Church", "Church"));
        assert!(name_matches(
            "Home Designs (conflict 2026-10-02)",
            "Home Designs (conflict 2026-10-02)"
        ));
    }

    #[test]
    fn near_misses_stay_disabled() {
        assert!(!name_matches("church", "Church"), "case");
        assert!(!name_matches("Church ", "Church"), "trailing space");
        assert!(!name_matches(" Church", "Church"), "leading space");
        assert!(!name_matches("Church", "Church (conflict 2026-10-02)"), "the duplicate is distinct");
        assert!(!name_matches("", "Church"));
    }

    #[test]
    fn hints_distinguish_near_misses_and_stay_quiet_otherwise() {
        assert_eq!(mismatch_hint("", "Church"), None);
        assert_eq!(mismatch_hint("Church", "Church"), None);
        assert!(mismatch_hint("church", "Church").unwrap().contains("exactly"));
        assert!(mismatch_hint("Church ", "Church").unwrap().contains("exactly"));
        assert!(mismatch_hint("Chu", "Church").unwrap().contains("doesn't match"));
    }

    #[test]
    fn only_a_matching_delete_response_deletes() {
        assert!(should_delete("delete", "Church", "Church"));
        assert!(!should_delete("delete", "church", "Church"));
        for r in ["cancel", "close", "", "other"] {
            assert!(!should_delete(r, "Church", "Church"), "{r:?} must never delete");
        }
    }
}

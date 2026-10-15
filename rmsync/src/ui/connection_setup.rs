//! Guided "Set up new connection" dialogs (MCC-108).
//!
//! The UI only collects input and shows progress; the staged, cancellable
//! flow itself is `device::setup::run_guided_setup`, which runs on its own
//! thread (GTK widgets are never touched from it — events come back over a
//! channel).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::config::AppConfig;
use crate::device::setup::{
    run_guided_setup, HostFingerprint, SetupError, SetupObserver, SetupOutcome, SetupRequest,
    SetupStep, SshSetupTransport,
};

enum UiEvent {
    Step(SetupStep),
    Confirm {
        fingerprint: HostFingerprint,
        reply: async_channel::Sender<bool>,
    },
    Done(Result<SetupOutcome, SetupError>),
}

struct ChannelObserver {
    tx: async_channel::Sender<UiEvent>,
}

#[async_trait::async_trait]
impl SetupObserver for ChannelObserver {
    fn step(&self, step: SetupStep) {
        let _ = self.tx.try_send(UiEvent::Step(step));
    }

    async fn confirm_fingerprint(&self, fingerprint: &HostFingerprint) -> bool {
        let (reply, answer) = async_channel::bounded(1);
        let event = UiEvent::Confirm { fingerprint: fingerprint.clone(), reply };
        if self.tx.send(event).await.is_err() {
            return false;
        }
        answer.recv().await.unwrap_or(false)
    }
}

fn step_text(step: SetupStep) -> &'static str {
    match step {
        SetupStep::Probing => "Contacting the tablet…",
        SetupStep::AwaitingFingerprint => "Waiting for you to check the device's fingerprint…",
        SetupStep::InstallingKey => "Creating a new key and installing it on the tablet…",
        SetupStep::VerifyingKeyLogin => "Testing the new key…",
        SetupStep::Committing => "Saving the new connection…",
    }
}

fn error_text(e: &SetupError) -> String {
    match e {
        SetupError::Unreachable(detail) => format!(
            "Could not reach the tablet. Check that it is on and connected by USB, \
             and that the address is right.\n\n{detail}"
        ),
        SetupError::WrongPassword => "The tablet rejected that password. Check it under \
             Settings → Help → Copyrights and licenses on the tablet and try again."
            .to_string(),
        SetupError::HostKeyChanged => "The tablet presented a different security key partway \
             through setup, so it was stopped. Nothing was changed."
            .to_string(),
        SetupError::KeyLoginFailed(detail) => format!(
            "The new key was installed, but logging in with it failed, so the new setup was \
             not saved. Your previous setup is unchanged.\n\n{detail}"
        ),
        other => other.to_string(),
    }
}

fn message(window: &adw::ApplicationWindow, heading: &str, body: &str) {
    let d = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading(heading)
        .body(body)
        .build();
    d.add_response("ok", "_OK");
    d.set_default_response(Some("ok"));
    d.set_close_response("ok");
    d.present();
}

/// Step 1: address and password. Everything after this can be cancelled and
/// leaves the existing configuration alone. `on_done` runs after a setup
/// that was fully tested and saved.
pub fn present_connection_setup<F: Fn() + 'static>(
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    on_done: F,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Set up new connection")
        .body(
            "This creates a new SSH key for rmSync and installs it on your tablet, \
             replacing the current one.\n\n\
             • Address: 10.11.99.1 when connected by USB.\n\
             • Password: on the tablet, open Settings → Help → Copyrights and licenses \
             and scroll to the bottom (older firmware: Settings → General → Software, \
             with Developer Mode on).\n\n\
             The password is used once, never saved, and cleared afterwards.",
        )
        .build();

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(16)
        .margin_end(16)
        .build();
    let host_entry = gtk::Entry::builder()
        .text(config.borrow().device.host.as_str())
        .placeholder_text("Device address")
        .build();
    let password_entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("Tablet SSH password")
        .build();
    form.append(&host_entry);
    form.append(&password_entry);
    dialog.set_extra_child(Some(&form));

    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("continue", "C_ontinue");
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("continue"));
    dialog.set_close_response("cancel");

    let window = window.clone();
    let on_done = Rc::new(on_done);
    dialog.connect_response(None, move |d, response| {
        if response != "continue" {
            d.close();
            return;
        }
        let host = host_entry.text().trim().to_string();
        let password = Zeroizing::new(password_entry.text().to_string());
        if host.is_empty() || password.is_empty() {
            message(&window, "Missing details", "Enter both the address and the password.");
            return;
        }
        password_entry.set_text("");
        d.close();
        run_setup(&window, config.clone(), host, password, on_done.clone());
    });
    dialog.present();
}

fn run_setup<F: Fn() + 'static>(
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    host: String,
    password: Zeroizing<String>,
    on_done: Rc<F>,
) {
    let conn = config.borrow().to_connection_config();
    let request = SetupRequest {
        host: host.clone(),
        port: conn.port,
        username: conn.username,
        timeout_secs: conn.timeout_secs,
        password,
        key_path: conn.key_path,
        known_hosts_path: conn.known_hosts_path,
    };

    let cancel = CancellationToken::new();
    let progress = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Setting up connection")
        .body(step_text(SetupStep::Probing))
        .build();
    progress.add_response("cancel", "_Cancel");
    progress.set_close_response("cancel");
    {
        let cancel = cancel.clone();
        progress.connect_response(None, move |_, _| cancel.cancel());
    }
    progress.present();

    let (tx, rx) = async_channel::unbounded::<UiEvent>();
    let thread_cancel = cancel.clone();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send_blocking(UiEvent::Done(Err(SetupError::Failed(format!("runtime: {e}")))));
                return;
            }
        };
        let observer = ChannelObserver { tx: tx.clone() };
        let result = runtime.block_on(run_guided_setup(
            &request,
            &SshSetupTransport,
            &observer,
            &thread_cancel,
        ));
        // `request` (and its password buffer) is wiped when it drops here.
        drop(request);
        let _ = tx.send_blocking(UiEvent::Done(result));
    });

    let window = window.clone();
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                UiEvent::Step(step) => progress.set_body(step_text(step)),
                UiEvent::Confirm { fingerprint, reply } => {
                    confirm_fingerprint(&window, &fingerprint, reply);
                }
                UiEvent::Done(result) => {
                    progress.close();
                    match result {
                        Ok(outcome) => {
                            {
                                let mut cfg = config.borrow_mut();
                                cfg.device.host = outcome.host.clone();
                                cfg.device.key_path = Some(outcome.key_path.clone());
                                if let Err(e) = cfg.save() {
                                    tracing::error!(error = format!("{e:#}"), "could not save config after setup");
                                }
                            }
                            message(
                                &window,
                                "Connection ready",
                                &format!(
                                    "The new key works: a test login with the key alone succeeded.\n\n\
                                     Device key: {}",
                                    outcome.fingerprint.display()
                                ),
                            );
                            on_done();
                        }
                        // The user's own choice; no error to show.
                        Err(SetupError::Cancelled) | Err(SetupError::DeclinedFingerprint) => {}
                        Err(e) => message(&window, "Setup did not finish", &error_text(&e)),
                    }
                    break;
                }
            }
        }
    });
}

fn confirm_fingerprint(
    window: &adw::ApplicationWindow,
    fingerprint: &HostFingerprint,
    reply: async_channel::Sender<bool>,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Is this your tablet?")
        .body(format!(
            "The tablet identified itself with this key:\n\n{}\n({})\n\n\
             rmSync will remember it and refuse to connect if it ever changes. Only \
             continue if you are connected to your own tablet, for example directly by USB.",
            fingerprint.display(),
            fingerprint.algorithm
        ))
        .build();
    dialog.add_response("cancel", "_Don't Trust");
    dialog.add_response("trust", "_Trust and Continue");
    dialog.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |d, response| {
        let _ = reply.try_send(response == "trust");
        d.close();
    });
    dialog.present();
}

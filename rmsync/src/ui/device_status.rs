//! Device connection status indicator (connected / disconnected / error).
//!
//! Renders a horizontal [icon][label] pair that reflects the current
//! `DeviceState`. Can be bound to a `DeviceMonitor`'s broadcast channel —
//! events flow through a glib async channel so UI updates happen on the GTK
//! main thread.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::device::monitor::{DeviceEvent, DeviceMonitor, DeviceState};

#[derive(Clone)]
pub struct DeviceStatusWidget {
    pub widget: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label,
    state: Rc<RefCell<DeviceState>>,
}

impl DeviceStatusWidget {
    pub fn new() -> Self {
        let icon = gtk::Image::new();
        let label = gtk::Label::new(None);
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build();
        widget.append(&icon);
        widget.append(&label);
        let out = Self {
            widget,
            icon,
            label,
            state: Rc::new(RefCell::new(DeviceState::Disconnected)),
        };
        out.apply_state_presentation(DeviceState::Disconnected);
        out
    }

    pub fn current_state(&self) -> DeviceState {
        *self.state.borrow()
    }

    pub fn set_state(&self, state: DeviceState) {
        *self.state.borrow_mut() = state;
        self.apply_state_presentation(state);
    }

    fn apply_state_presentation(&self, state: DeviceState) {
        let (icon_name, label, css, tooltip) = visual_for(state);
        self.icon.set_icon_name(Some(icon_name));
        self.label.set_text(label);
        for cls in ["error", "warning", "success"] {
            self.widget.remove_css_class(cls);
        }
        self.widget.add_css_class(css);
        self.widget.set_tooltip_text(Some(tooltip));
    }

    /// Subscribe to a monitor's event channel and update the widget on the
    /// GTK main thread whenever an event arrives.
    pub fn bind_to_monitor(&self, monitor: &DeviceMonitor) {
        let mut rx = monitor.subscribe();
        let (tx, mut gtk_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceEvent>();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
        let widget = self.clone();
        glib::spawn_future_local(async move {
            while let Some(ev) = gtk_rx.recv().await {
                widget.apply_event(ev);
            }
        });
    }

    pub fn apply_event(&self, event: DeviceEvent) {
        let new_state = match event {
            DeviceEvent::Disconnected => DeviceState::Disconnected,
            DeviceEvent::UsbDetected => DeviceState::Detected,
            DeviceEvent::Connected => DeviceState::Connected,
            DeviceEvent::ConnectionFailed(_) => DeviceState::Disconnected,
        };
        self.set_state(new_state);
    }
}

impl Default for DeviceStatusWidget {
    fn default() -> Self {
        Self::new()
    }
}

fn visual_for(state: DeviceState) -> (&'static str, &'static str, &'static str, &'static str) {
    match state {
        DeviceState::Disconnected => (
            "network-offline-symbolic",
            "Not connected",
            "error",
            "Connect your reMarkable 2 via USB",
        ),
        DeviceState::Detected => (
            "network-idle-symbolic",
            "Detecting...",
            "warning",
            "reMarkable detected, verifying SSH connection...",
        ),
        DeviceState::Connected => (
            "network-transmit-receive-symbolic",
            "Connected",
            "success",
            "Connected to reMarkable at 10.11.99.1",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_for_disconnected() {
        let (icon, label, css, _) = visual_for(DeviceState::Disconnected);
        assert_eq!(icon, "network-offline-symbolic");
        assert_eq!(label, "Not connected");
        assert_eq!(css, "error");
    }

    #[test]
    fn visual_for_detected() {
        let (icon, label, css, tip) = visual_for(DeviceState::Detected);
        assert_eq!(icon, "network-idle-symbolic");
        assert_eq!(label, "Detecting...");
        assert_eq!(css, "warning");
        assert!(tip.contains("verifying"));
    }

    #[test]
    fn visual_for_connected() {
        let (icon, label, css, tip) = visual_for(DeviceState::Connected);
        assert_eq!(icon, "network-transmit-receive-symbolic");
        assert_eq!(label, "Connected");
        assert_eq!(css, "success");
        assert!(tip.contains("10.11.99.1"));
    }
}

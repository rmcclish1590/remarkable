# Spec 18 — Device Status Indicator

**Layer:** 4 — UI Shell  
**Dependencies:** 07 (udev device monitor), 15 (main window)  
**Estimated effort:** 1 hour  

## Objective

Display a real-time device connection status indicator in the header bar that shows whether the reMarkable 2 is connected, disconnected, or in a transitional state.

## Context

The device monitor (Spec 07) emits `DeviceEvent` signals over a broadcast channel. This spec creates a UI widget that subscribes to those events and updates the header bar to show the current connection state.

## Technical Requirements

### 1. Status widget (`src/ui/device_status.rs`)

```rust
pub struct DeviceStatusWidget {
    pub widget: gtk::Box,         // Container to insert into header bar
    icon: gtk::Image,
    label: gtk::Label,
    state: Arc<Mutex<DeviceState>>,
}

impl DeviceStatusWidget {
    pub fn new() -> Self

    /// Update the display to reflect a new device state.
    pub fn set_state(&self, state: DeviceState)

    /// Subscribe to a DeviceMonitor's event channel and auto-update.
    pub fn bind_to_monitor(&self, monitor: &DeviceMonitor)
}
```

### 2. Visual states

| State | Icon | Label | CSS Class |
|-------|------|-------|-----------|
| Disconnected | `"network-offline-symbolic"` | "Not connected" | `"error"` (red tint) |
| Detected | `"network-idle-symbolic"` | "Detecting..." | `"warning"` (yellow tint) |
| Connected | `"network-transmit-receive-symbolic"` | "Connected" | `"success"` (green tint) |

The icon and label sit side-by-side in a horizontal `GtkBox` with 4px spacing.

### 3. Event handling

Use `glib::MainContext::channel()` to bridge from the Tokio broadcast receiver to the GTK main thread:

```rust
pub fn bind_to_monitor(&self, monitor: &DeviceMonitor) {
    let mut rx = monitor.subscribe();
    let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    
    // Spawn Tokio task to forward events
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if sender.send(event).is_err() {
                break;
            }
        }
    });
    
    // Attach GTK receiver to update UI on main thread
    let widget = self.clone();  // or use weak reference
    receiver.attach(None, move |event| {
        match event {
            DeviceEvent::Disconnected => widget.set_state(DeviceState::Disconnected),
            DeviceEvent::UsbDetected => widget.set_state(DeviceState::Detected),
            DeviceEvent::Connected => widget.set_state(DeviceState::Connected),
            DeviceEvent::ConnectionFailed(_) => widget.set_state(DeviceState::Disconnected),
        }
        glib::ControlFlow::Continue
    });
}
```

### 4. Tooltip

Add a tooltip to the widget showing additional info:
- Disconnected: "Connect your reMarkable 2 via USB"
- Detected: "reMarkable detected, verifying SSH connection..."
- Connected: "Connected to reMarkable at 10.11.99.1"

## Files to Create/Modify

- `src/ui/device_status.rs` — full implementation
- `src/ui/window.rs` — replace header bar placeholder with `DeviceStatusWidget`
- `src/ui/mod.rs` — export module

## Test Strategy

1. **Initial state** — widget starts showing "Not connected" with offline icon.
2. **State transition** — call `set_state(Connected)`, verify icon and label update.
3. **All states** — cycle through Disconnected → Detected → Connected → Disconnected, verify each renders correctly.
4. **Tooltip** — verify tooltip text matches current state.

## Acceptance Criteria

1. Header bar shows a live connection indicator.
2. Icon and label update in real-time when device state changes.
3. Three visual states are clearly distinguishable (color + icon + label).
4. Tooltips provide contextual guidance.
5. Event bridging from Tokio to GTK main thread works without blocking.

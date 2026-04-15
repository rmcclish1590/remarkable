# Spec 23 — Wire Device Monitor to UI

**Layer:** 6 — Integration  
**Dependencies:** 07 (udev monitor), 18 (device status widget), 19 (sync controls)  
**Estimated effort:** 1 hour  

## Objective

Connect the device monitor's events to the UI so that plugging in a reMarkable updates the status indicator, enables the sync button, and optionally triggers auto-sync.

## Context

The device monitor (Spec 07) runs on Tokio and emits events via a broadcast channel. The UI widgets (Specs 18, 19) have methods to update their state. This spec bridges the two with proper thread marshaling.

## Technical Requirements

### 1. Startup wiring (`src/app.rs` — extend)

During application startup:

```rust
fn setup_device_monitoring(
    device_status: &DeviceStatusWidget,
    sync_controls: &SyncControls,
    config: &AppConfig,
) -> DeviceMonitor {
    let conn_config = config.to_connection_config();
    let (monitor, _rx) = DeviceMonitor::new(conn_config);
    
    // Bind status widget to monitor events
    device_status.bind_to_monitor(&monitor);
    
    // Bind sync controls to device state
    let mut rx = monitor.subscribe();
    let sync_controls_clone = sync_controls.clone();
    let auto_sync = config.sync.auto_sync_on_connect;
    
    let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            sender.send(event).ok();
        }
    });
    
    receiver.attach(None, move |event| {
        match event {
            DeviceEvent::Connected => {
                sync_controls_clone.set_device_connected(true);
                if auto_sync {
                    // Trigger sync automatically
                    sync_controls_clone.sync_button.emit_clicked();
                }
            }
            DeviceEvent::Disconnected => {
                sync_controls_clone.set_device_connected(false);
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    });
    
    // Check for already-connected device
    monitor.check_now();
    
    // Start background monitoring
    monitor.start();
    
    monitor
}
```

### 2. Auto-sync on connect

If `config.sync.auto_sync_on_connect` is true:
- When `DeviceEvent::Connected` fires, automatically trigger the sync button.
- Show a toast notification: "reMarkable connected — syncing automatically..."
- If sync is already in progress, skip (don't queue a second sync).

### 3. Disconnection during sync

If the device disconnects while a sync is in progress:
- The sync engine's SFTP operations will fail.
- The cancel token should be triggered.
- Show an error toast: "reMarkable disconnected during sync — sync incomplete."
- The partial sync state is preserved in SQLite — next sync will resume correctly.

### 4. First-time SSH setup

If the device is detected but SSH connection fails (likely no key setup yet):

```rust
DeviceEvent::ConnectionFailed(reason) => {
    // Check if this is an auth failure
    if reason.contains("auth") {
        // Show a dialog prompting for the reMarkable password
        show_ssh_setup_dialog(window, config);
    }
}
```

The SSH setup dialog:
- `adw::MessageDialog` explaining: "First connection — enter your reMarkable's root password (found in Settings → General → Software)"
- `GtkEntry` (password mode) for the password.
- "Connect" button that:
  1. Attempts password auth.
  2. On success, calls `connection.setup_key_auth()` to install a keypair.
  3. Saves the key path to config.
  4. Shows success toast: "SSH key installed — future connections will be automatic."

## Files to Create/Modify

- `src/app.rs` — add `setup_device_monitoring` and SSH setup dialog
- `src/ui/sync_controls.rs` — add `is_syncing()` method to prevent duplicate syncs

## Test Strategy

1. **Device connect** — emit `Connected`, verify sync button enables.
2. **Device disconnect** — emit `Disconnected`, verify sync button disables.
3. **Auto-sync** — set config flag, emit `Connected`, verify sync triggers.
4. **No duplicate sync** — emit `Connected` while sync is running, verify no second sync starts.
5. **Disconnect during sync** — start sync, emit `Disconnected`, verify error shows.

## Acceptance Criteria

1. Plugging in a reMarkable updates the status indicator and enables sync.
2. Unplugging disables the sync button and shows disconnected state.
3. Auto-sync triggers on connect when configured.
4. First-time connection prompts for password and sets up SSH keys.
5. Disconnect during sync is handled gracefully with user notification.

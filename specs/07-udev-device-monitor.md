# Spec 07 — udev USB Device Monitor

**Layer:** 1 — Connectivity  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 1–2 hours  

## Objective

Implement a background service that detects when a reMarkable 2 tablet is plugged in or unplugged via USB, and emits events that the UI and sync engine can subscribe to.

## Context

When the reMarkable 2 is connected via USB-C, Linux creates a virtual network interface (USB Ethernet gadget). The udev subsystem sees this as a network device appearing. We need to monitor udev events for this specific device and verify SSH reachability before declaring the device "connected."

## Technical Requirements

### 1. Device monitor (`src/device/monitor.rs`)

```rust
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEvent {
    /// USB device detected, SSH not yet verified
    UsbDetected,
    /// SSH connection verified — device is ready for sync
    Connected,
    /// USB device removed
    Disconnected,
    /// USB detected but SSH connection failed
    ConnectionFailed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    Disconnected,
    Detected,      // USB seen, SSH not verified
    Connected,     // SSH verified, ready to sync
}

pub struct DeviceMonitor {
    state: Arc<Mutex<DeviceState>>,
    sender: broadcast::Sender<DeviceEvent>,
    /// The SSH connection config to use for verification
    connection_config: ConnectionConfig,
}

impl DeviceMonitor {
    /// Create a new monitor. Returns the monitor and a receiver for events.
    pub fn new(config: ConnectionConfig) -> (Self, broadcast::Receiver<DeviceEvent>)

    /// Subscribe to device events (additional receivers).
    pub fn subscribe(&self) -> broadcast::Receiver<DeviceEvent>

    /// Get the current device state.
    pub fn state(&self) -> DeviceState

    /// Start the monitoring loop. Runs until the returned handle is dropped.
    /// This spawns a Tokio task that polls udev.
    pub fn start(&self) -> tokio::task::JoinHandle<()>

    /// Manually trigger a connection check (e.g., on app startup).
    pub async fn check_now(&self)
}
```

### 2. udev monitoring logic

The monitoring loop should:

1. Create a `udev::MonitorBuilder` listening on the `net` subsystem (the reMarkable appears as a network device).
2. Poll for `add` and `remove` events.
3. On `add` event:
   - Check if the new network interface matches the reMarkable's USB Ethernet pattern. The interface typically appears as `usb0` or `enp0s*` with the reMarkable's characteristics.
   - Alternative approach: instead of matching device attributes, simply attempt to ping `10.11.99.1` after any new USB network device appears.
   - Emit `DeviceEvent::UsbDetected`.
   - Spawn an async task that waits 2–3 seconds (device boot time), then attempts `DeviceConnection::ping()` at `10.11.99.1`.
   - If ping succeeds → emit `DeviceEvent::Connected`.
   - If ping fails after 3 retries (1 second apart) → emit `DeviceEvent::ConnectionFailed`.
4. On `remove` event:
   - If the removed device matches, emit `DeviceEvent::Disconnected`.
   - Update state to `DeviceState::Disconnected`.

### 3. Fallback polling mode

udev monitoring requires root or specific group membership. If udev access fails:

1. Log a warning.
2. Fall back to a polling loop that runs every 3 seconds.
3. The poll simply calls `DeviceConnection::ping()` against `10.11.99.1`.
4. Emit `Connected` when ping transitions from fail → success.
5. Emit `Disconnected` when ping transitions from success → fail.

```rust
async fn polling_fallback(
    sender: broadcast::Sender<DeviceEvent>,
    state: Arc<Mutex<DeviceState>>,
    config: ConnectionConfig,
) {
    let mut was_connected = false;
    loop {
        let conn = DeviceConnection::new(config.clone());
        let is_connected = conn.ping().await;
        
        if is_connected && !was_connected {
            sender.send(DeviceEvent::Connected).ok();
            *state.lock().unwrap() = DeviceState::Connected;
        } else if !is_connected && was_connected {
            sender.send(DeviceEvent::Disconnected).ok();
            *state.lock().unwrap() = DeviceState::Disconnected;
        }
        was_connected = is_connected;
        
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
```

### 4. Thread safety

- `DeviceState` is behind `Arc<Mutex<>>` for safe reads from the UI thread.
- Events are delivered via `tokio::sync::broadcast` so multiple consumers (UI, sync engine) can subscribe independently.
- The monitor runs on the Tokio runtime, NOT on the GTK main thread.

### 5. Startup behavior

When the application starts:
1. Create the `DeviceMonitor`.
2. Call `check_now()` to see if a device is already connected (user may have plugged it in before launching the app).
3. Start the background monitoring loop.

## Files to Create/Modify

- `src/device/monitor.rs` — full implementation
- `src/device/mod.rs` — export the module

## Test Strategy

1. **State transitions** — create a monitor, manually send events via the broadcast channel, verify state transitions: `Disconnected → Detected → Connected`, `Connected → Disconnected`.
2. **Multiple subscribers** — create 2 subscribers, send an event, verify both receive it.
3. **Fallback mode** — simulate udev failure, verify polling loop activates (mock the ping method).
4. **Initial state** — verify monitor starts in `Disconnected` state.

Manual testing (with device):
5. **Plug in reMarkable** — verify `UsbDetected` then `Connected` events fire.
6. **Unplug** — verify `Disconnected` event fires.
7. **App startup with device already connected** — verify `check_now()` detects it.

## Acceptance Criteria

1. The monitor detects USB connection and disconnection events.
2. SSH reachability is verified before emitting `Connected`.
3. Falls back to polling if udev is unavailable.
4. Multiple consumers can subscribe to events independently.
5. State is thread-safe and queryable.
6. Unit tests pass.

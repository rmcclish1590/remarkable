//! udev-based monitor that detects reMarkable tablets connecting via USB.
//!
//! Watches the `net` subsystem for add/remove events, verifies SSH
//! reachability at 10.11.99.1 before declaring the device `Connected`, and
//! falls back to periodic polling when udev is unavailable (e.g. missing
//! permissions).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::connection::{ConnectionConfig, DeviceConnection};

const BROADCAST_CAPACITY: usize = 32;
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const DEVICE_BOOT_DELAY: Duration = Duration::from_secs(2);
const VERIFY_RETRIES: u32 = 3;
const VERIFY_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    UsbDetected,
    Connected,
    Disconnected,
    ConnectionFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Disconnected,
    Detected,
    Connected,
}

pub struct DeviceMonitor {
    state: Arc<Mutex<DeviceState>>,
    sender: broadcast::Sender<DeviceEvent>,
    connection_config: ConnectionConfig,
}

impl DeviceMonitor {
    pub fn new(config: ConnectionConfig) -> (Self, broadcast::Receiver<DeviceEvent>) {
        let (sender, receiver) = broadcast::channel(BROADCAST_CAPACITY);
        let monitor = Self {
            state: Arc::new(Mutex::new(DeviceState::Disconnected)),
            sender,
            connection_config: config,
        };
        (monitor, receiver)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.sender.subscribe()
    }

    pub fn state(&self) -> DeviceState {
        *self.state.lock().expect("state mutex poisoned")
    }

    /// Ping the device and update state/emit events based on whether this is
    /// an edge (off → on or on → off). Used by both `check_now` and the
    /// polling fallback.
    async fn check_and_broadcast(
        sender: &broadcast::Sender<DeviceEvent>,
        state: &Arc<Mutex<DeviceState>>,
        config: &ConnectionConfig,
    ) -> bool {
        let conn = DeviceConnection::new(config.clone());
        let reachable = conn.ping().await;
        let previous = *state.lock().expect("state mutex poisoned");
        match (previous, reachable) {
            (DeviceState::Connected, false) => {
                *state.lock().expect("state mutex poisoned") = DeviceState::Disconnected;
                let _ = sender.send(DeviceEvent::Disconnected);
            }
            (DeviceState::Disconnected | DeviceState::Detected, true) => {
                *state.lock().expect("state mutex poisoned") = DeviceState::Connected;
                let _ = sender.send(DeviceEvent::Connected);
            }
            _ => {}
        }
        reachable
    }

    pub async fn check_now(&self) {
        Self::check_and_broadcast(&self.sender, &self.state, &self.connection_config).await;
    }

    /// Start the monitoring loop. Tries udev first, falls back to polling.
    pub fn start(&self) -> JoinHandle<()> {
        let sender = self.sender.clone();
        let state = self.state.clone();
        let config = self.connection_config.clone();
        tokio::spawn(async move {
            match spawn_udev_thread(sender.clone()) {
                Ok(mut events) => {
                    info!("device monitor: using udev for USB events");
                    run_with_udev(&sender, &state, &config, &mut events).await;
                }
                Err(e) => {
                    warn!("device monitor: udev unavailable ({e}); falling back to polling");
                    polling_fallback(sender, state, config).await;
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UdevSignal {
    Added,
    Removed,
}

fn spawn_udev_thread(
    _sender: broadcast::Sender<DeviceEvent>,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<UdevSignal>, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    std::thread::spawn(move || {
        let socket = match udev::MonitorBuilder::new()
            .and_then(|b| b.match_subsystem("net"))
            .and_then(|b| b.listen())
        {
            Ok(s) => {
                let _ = ready_tx.send(Ok(()));
                s
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        loop {
            for event in socket.iter() {
                let signal = match event.event_type() {
                    udev::EventType::Add => Some(UdevSignal::Added),
                    udev::EventType::Remove => Some(UdevSignal::Removed),
                    _ => None,
                };
                if let Some(sig) = signal {
                    if tx.send(sig).is_err() {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(e.to_string()),
    }
}

async fn run_with_udev(
    sender: &broadcast::Sender<DeviceEvent>,
    state: &Arc<Mutex<DeviceState>>,
    config: &ConnectionConfig,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<UdevSignal>,
) {
    while let Some(signal) = events.recv().await {
        match signal {
            UdevSignal::Added => {
                debug!("udev: net device added");
                *state.lock().expect("state mutex poisoned") = DeviceState::Detected;
                let _ = sender.send(DeviceEvent::UsbDetected);
                tokio::time::sleep(DEVICE_BOOT_DELAY).await;
                verify_with_retries(sender, state, config).await;
            }
            UdevSignal::Removed => {
                debug!("udev: net device removed");
                if *state.lock().expect("state mutex poisoned") != DeviceState::Disconnected {
                    *state.lock().expect("state mutex poisoned") = DeviceState::Disconnected;
                    let _ = sender.send(DeviceEvent::Disconnected);
                }
            }
        }
    }
}

async fn verify_with_retries(
    sender: &broadcast::Sender<DeviceEvent>,
    state: &Arc<Mutex<DeviceState>>,
    config: &ConnectionConfig,
) {
    for attempt in 1..=VERIFY_RETRIES {
        let conn = DeviceConnection::new(config.clone());
        if conn.ping().await {
            *state.lock().expect("state mutex poisoned") = DeviceState::Connected;
            let _ = sender.send(DeviceEvent::Connected);
            return;
        }
        debug!("device verify attempt {attempt} failed");
        tokio::time::sleep(VERIFY_RETRY_DELAY).await;
    }
    let _ = sender.send(DeviceEvent::ConnectionFailed(format!(
        "SSH not reachable at {}:{} after {VERIFY_RETRIES} attempts",
        config.host, config.port
    )));
}

async fn polling_fallback(
    sender: broadcast::Sender<DeviceEvent>,
    state: Arc<Mutex<DeviceState>>,
    config: ConnectionConfig,
) {
    loop {
        DeviceMonitor::check_and_broadcast(&sender, &state, &config).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ConnectionConfig {
        ConnectionConfig {
            host: "192.0.2.1".to_string(), // non-routable
            port: 22,
            timeout_secs: 1,
            ..ConnectionConfig::default()
        }
    }

    #[tokio::test]
    async fn initial_state_is_disconnected() {
        let (monitor, _rx) = DeviceMonitor::new(test_config());
        assert_eq!(monitor.state(), DeviceState::Disconnected);
    }

    #[tokio::test]
    async fn broadcast_delivers_to_multiple_subscribers() {
        let (monitor, mut rx1) = DeviceMonitor::new(test_config());
        let mut rx2 = monitor.subscribe();
        monitor.sender.send(DeviceEvent::UsbDetected).unwrap();
        assert_eq!(rx1.recv().await.unwrap(), DeviceEvent::UsbDetected);
        assert_eq!(rx2.recv().await.unwrap(), DeviceEvent::UsbDetected);
    }

    #[tokio::test]
    async fn check_now_against_unreachable_host_stays_disconnected() {
        let (monitor, mut rx) = DeviceMonitor::new(test_config());
        monitor.check_now().await;
        assert_eq!(monitor.state(), DeviceState::Disconnected);
        // No event should fire on a no-op transition.
        assert!(tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn transition_from_connected_to_disconnected_emits_event() {
        let (monitor, mut rx) = DeviceMonitor::new(test_config());
        *monitor.state.lock().unwrap() = DeviceState::Connected;
        monitor.check_now().await;
        assert_eq!(monitor.state(), DeviceState::Disconnected);
        assert_eq!(rx.recv().await.unwrap(), DeviceEvent::Disconnected);
    }

    #[tokio::test]
    async fn device_event_and_state_eq() {
        assert_eq!(DeviceEvent::Connected, DeviceEvent::Connected);
        assert_ne!(DeviceEvent::Connected, DeviceEvent::Disconnected);
        assert_eq!(DeviceState::Detected, DeviceState::Detected);
    }

    #[tokio::test]
    async fn connection_failed_carries_message() {
        let ev = DeviceEvent::ConnectionFailed("nope".to_string());
        match ev {
            DeviceEvent::ConnectionFailed(m) => assert_eq!(m, "nope"),
            _ => panic!("wrong variant"),
        }
    }
}

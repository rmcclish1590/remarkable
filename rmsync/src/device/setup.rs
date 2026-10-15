//! Guided setup of a brand-new SSH connection to the tablet (MCC-108).
//!
//! The flow is staged so that nothing the user already has is touched until
//! the whole thing has worked:
//!
//! 1. probe the device's host key (no authentication) and ask the user to
//!    confirm its fingerprint;
//! 2. write the confirmed key to a *staged* `known_hosts`, then log in once
//!    with the password and install a freshly generated rmsync key (also
//!    staged);
//! 3. prove the new key works with key-only authentication;
//! 4. only then rename the staged files over the old ones.
//!
//! Any failure, decline or cancellation removes the staged files and leaves
//! the previous key and pinned host key exactly as they were. The password
//! lives in a `Zeroizing` buffer and is never written anywhere.
//!
//! The orchestration is generic over [`SetupTransport`] so it can be tested
//! without an SSH server (there is no `DeviceConnection` mock).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::key as russh_key;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::connection::{
    map_connect_err, public_key_path, AuthMethod, ConnectionConfig, ConnectionError,
    DeviceConnection, REJECTED_CREDENTIALS,
};

/// Host key as presented by the device, before it has been trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFingerprint {
    pub algorithm: String,
    pub fingerprint: String,
}

impl HostFingerprint {
    /// `SHA256:<base64>`, as `ssh-keygen -l` prints it.
    pub fn display(&self) -> String {
        super::connection::sha256_display(&self.fingerprint)
    }
}

pub struct SetupRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub timeout_secs: u64,
    pub password: Zeroizing<String>,
    /// Where the rmsync private key lives once setup succeeds.
    pub key_path: PathBuf,
    pub known_hosts_path: PathBuf,
}

// Hand-written: the derived impl would print the password.
impl std::fmt::Debug for SetupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupRequest")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("key_path", &self.key_path)
            .field("known_hosts_path", &self.known_hosts_path)
            .finish()
    }
}

/// Side-by-side copies of the files setup will replace.
#[derive(Debug, Clone)]
pub struct StagedPaths {
    pub key: PathBuf,
    pub key_pub: PathBuf,
    pub known_hosts: PathBuf,
}

impl StagedPaths {
    fn remove_all(&self) {
        for p in [&self.key, &self.key_pub, &self.known_hosts] {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

impl SetupRequest {
    pub fn staged(&self) -> StagedPaths {
        let key = with_suffix(&self.key_path, ".setup");
        StagedPaths {
            key_pub: public_key_path(&key),
            key,
            known_hosts: with_suffix(&self.known_hosts_path, ".setup"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    Probing,
    AwaitingFingerprint,
    InstallingKey,
    VerifyingKeyLogin,
    Committing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOutcome {
    pub host: String,
    pub key_path: PathBuf,
    pub fingerprint: HostFingerprint,
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error("Setup was cancelled")]
    Cancelled,
    #[error("The device's host key was not trusted")]
    DeclinedFingerprint,
    #[error("Could not reach the device: {0}")]
    Unreachable(String),
    #[error("The device rejected that password")]
    WrongPassword,
    #[error("The device's host key changed while setting up")]
    HostKeyChanged,
    #[error("The new key was installed but key-only login failed: {0}")]
    KeyLoginFailed(String),
    #[error("{0}")]
    Failed(String),
}

fn classify(e: ConnectionError) -> SetupError {
    match e {
        ConnectionError::Unreachable(..) | ConnectionError::Timeout(_) | ConnectionError::Io(_) => {
            SetupError::Unreachable(e.to_string())
        }
        ConnectionError::AuthFailed(ref m) if m == REJECTED_CREDENTIALS => SetupError::WrongPassword,
        ConnectionError::HostKeyMismatch(_) => SetupError::HostKeyChanged,
        other => SetupError::Failed(other.to_string()),
    }
}

#[async_trait]
pub trait SetupTransport: Send + Sync {
    /// Read the device's host key without authenticating.
    async fn probe_host_key(&self, req: &SetupRequest) -> Result<HostFingerprint, ConnectionError>;
    /// Log in with the password against the staged `known_hosts`, generate
    /// the key pair at `staged.key` and install the public half.
    async fn install_key(
        &self,
        req: &SetupRequest,
        staged: &StagedPaths,
    ) -> Result<(), ConnectionError>;
    /// Connect with the staged key only.
    async fn verify_key_login(
        &self,
        req: &SetupRequest,
        staged: &StagedPaths,
    ) -> Result<(), ConnectionError>;
}

#[async_trait]
pub trait SetupObserver: Send + Sync {
    fn step(&self, step: SetupStep);
    /// Show the fingerprint; `true` means the user chose to trust it.
    async fn confirm_fingerprint(&self, fingerprint: &HostFingerprint) -> bool;
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, SetupError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(SetupError::Cancelled),
        v = fut => Ok(v),
    }
}

pub async fn run_guided_setup(
    req: &SetupRequest,
    transport: &dyn SetupTransport,
    observer: &dyn SetupObserver,
    cancel: &CancellationToken,
) -> Result<SetupOutcome, SetupError> {
    tracing::info!(host = %req.host, port = req.port, "guided SSH setup started");
    let staged = req.staged();
    staged.remove_all();
    let result = run_steps(req, &staged, transport, observer, cancel).await;
    staged.remove_all();
    match &result {
        Ok(o) => tracing::info!(
            host = %o.host,
            fingerprint = %o.fingerprint.display(),
            "guided SSH setup finished; new key and host key in place"
        ),
        Err(SetupError::Cancelled) => {
            tracing::info!("guided SSH setup cancelled; previous configuration kept")
        }
        Err(SetupError::DeclinedFingerprint) => {
            tracing::info!("host key fingerprint declined; previous configuration kept")
        }
        Err(e) => tracing::warn!(error = %e, "guided SSH setup failed; previous configuration kept"),
    }
    result
}

async fn run_steps(
    req: &SetupRequest,
    staged: &StagedPaths,
    transport: &dyn SetupTransport,
    observer: &dyn SetupObserver,
    cancel: &CancellationToken,
) -> Result<SetupOutcome, SetupError> {
    observer.step(SetupStep::Probing);
    let fingerprint = cancellable(cancel, transport.probe_host_key(req))
        .await?
        .map_err(classify)?;
    tracing::info!(
        algorithm = %fingerprint.algorithm,
        fingerprint = %fingerprint.display(),
        "device presented host key"
    );

    observer.step(SetupStep::AwaitingFingerprint);
    if !cancellable(cancel, observer.confirm_fingerprint(&fingerprint)).await? {
        return Err(SetupError::DeclinedFingerprint);
    }

    if let Some(parent) = staged.known_hosts.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SetupError::Failed(e.to_string()))?;
    }
    std::fs::write(
        &staged.known_hosts,
        format!("{} {}\n", fingerprint.algorithm, fingerprint.fingerprint),
    )
    .map_err(|e| SetupError::Failed(e.to_string()))?;

    observer.step(SetupStep::InstallingKey);
    cancellable(cancel, transport.install_key(req, staged))
        .await?
        .map_err(classify)?;

    observer.step(SetupStep::VerifyingKeyLogin);
    cancellable(cancel, transport.verify_key_login(req, staged))
        .await?
        .map_err(|e| match classify(e) {
            SetupError::Failed(m) => SetupError::KeyLoginFailed(m),
            SetupError::WrongPassword => {
                SetupError::KeyLoginFailed("the device did not accept the new key".to_string())
            }
            other => other,
        })?;

    if cancel.is_cancelled() {
        return Err(SetupError::Cancelled);
    }
    observer.step(SetupStep::Committing);
    commit(req, staged).map_err(|e| SetupError::Failed(format!("saving new connection: {e}")))?;
    Ok(SetupOutcome {
        host: req.host.clone(),
        key_path: req.key_path.clone(),
        fingerprint,
    })
}

/// Rename the staged files over the live ones. Same directory, so each
/// rename is atomic.
fn commit(req: &SetupRequest, staged: &StagedPaths) -> std::io::Result<()> {
    std::fs::rename(&staged.key, &req.key_path)?;
    std::fs::rename(&staged.key_pub, public_key_path(&req.key_path))?;
    std::fs::rename(&staged.known_hosts, &req.known_hosts_path)?;
    Ok(())
}

/// Real SSH implementation of [`SetupTransport`].
pub struct SshSetupTransport;

impl SshSetupTransport {
    fn config(
        req: &SetupRequest,
        staged: &StagedPaths,
        auth: AuthMethod,
    ) -> ConnectionConfig {
        ConnectionConfig {
            host: req.host.clone(),
            port: req.port,
            username: req.username.clone(),
            auth,
            timeout_secs: req.timeout_secs,
            known_hosts_path: staged.known_hosts.clone(),
            key_path: staged.key.clone(),
        }
    }
}

/// Records the presented key and refuses it, so nothing is authenticated or
/// pinned before the user has seen the fingerprint.
struct ProbeHandler {
    seen: Arc<Mutex<Option<HostFingerprint>>>,
}

#[async_trait]
impl Handler for ProbeHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(HostFingerprint {
                algorithm: server_public_key.name().to_string(),
                fingerprint: server_public_key.fingerprint(),
            });
        }
        Ok(false)
    }
}

#[async_trait]
impl SetupTransport for SshSetupTransport {
    async fn probe_host_key(&self, req: &SetupRequest) -> Result<HostFingerprint, ConnectionError> {
        let seen = Arc::new(Mutex::new(None));
        let config = Arc::new(client::Config::default());
        let attempt = tokio::time::timeout(
            Duration::from_secs(req.timeout_secs),
            client::connect(config, (req.host.as_str(), req.port), ProbeHandler { seen: seen.clone() }),
        )
        .await;
        // The handler always refuses the key, so a captured fingerprint is
        // the expected outcome and the resulting error is irrelevant.
        if let Some(fp) = seen.lock().ok().and_then(|mut s| s.take()) {
            return Ok(fp);
        }
        match attempt {
            Err(_) => Err(ConnectionError::Timeout(req.timeout_secs)),
            Ok(Err(e)) => {
                let cfg = ConnectionConfig {
                    host: req.host.clone(),
                    port: req.port,
                    timeout_secs: req.timeout_secs,
                    ..ConnectionConfig::default()
                };
                Err(map_connect_err(e, &cfg, None))
            }
            Ok(Ok(_)) => Err(ConnectionError::SshError(
                "device did not present a host key".to_string(),
            )),
        }
    }

    async fn install_key(
        &self,
        req: &SetupRequest,
        staged: &StagedPaths,
    ) -> Result<(), ConnectionError> {
        // DeviceConnection zeroizes this copy of the password when dropped.
        let cfg = Self::config(req, staged, AuthMethod::Password(req.password.to_string()));
        let mut conn = DeviceConnection::new(cfg);
        conn.connect().await?;
        let result = conn.setup_key_auth().await;
        conn.disconnect().await;
        result.map(|_| ())
    }

    async fn verify_key_login(
        &self,
        req: &SetupRequest,
        staged: &StagedPaths,
    ) -> Result<(), ConnectionError> {
        let cfg = Self::config(req, staged, AuthMethod::KeyFile(staged.key.clone()));
        let mut conn = DeviceConnection::new(cfg);
        conn.connect().await?;
        conn.disconnect().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const OLD_KEY: &str = "OLD-PRIVATE-KEY";
    const OLD_HOSTS: &str = "ssh-ed25519 oldfp\n";

    struct Fake {
        probe_err: Option<fn() -> ConnectionError>,
        install_err: Option<fn() -> ConnectionError>,
        verify_err: Option<fn() -> ConnectionError>,
        installs: AtomicUsize,
    }

    impl Fake {
        fn ok() -> Self {
            Self { probe_err: None, install_err: None, verify_err: None, installs: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl SetupTransport for Fake {
        async fn probe_host_key(&self, _: &SetupRequest) -> Result<HostFingerprint, ConnectionError> {
            match self.probe_err {
                Some(f) => Err(f()),
                None => Ok(HostFingerprint { algorithm: "ssh-ed25519".into(), fingerprint: "newfp".into() }),
            }
        }

        async fn install_key(&self, _: &SetupRequest, s: &StagedPaths) -> Result<(), ConnectionError> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            // The host key must already be pinned before any login.
            assert_eq!(std::fs::read_to_string(&s.known_hosts).unwrap(), "ssh-ed25519 newfp\n");
            if let Some(f) = self.install_err {
                return Err(f());
            }
            write_private(&s.key, "NEW-PRIVATE-KEY");
            std::fs::write(&s.key_pub, "ssh-ed25519 AAAA rmsync@local\n").unwrap();
            Ok(())
        }

        async fn verify_key_login(&self, _: &SetupRequest, _: &StagedPaths) -> Result<(), ConnectionError> {
            self.verify_err.map_or(Ok(()), |f| Err(f()))
        }
    }

    fn write_private(path: &Path, contents: &str) {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    struct Observer {
        trust: bool,
        cancel_on_confirm: Option<CancellationToken>,
        confirmed: AtomicBool,
    }

    impl Observer {
        fn trusting() -> Self {
            Self { trust: true, cancel_on_confirm: None, confirmed: AtomicBool::new(false) }
        }
    }

    #[async_trait]
    impl SetupObserver for Observer {
        fn step(&self, _: SetupStep) {}

        async fn confirm_fingerprint(&self, _: &HostFingerprint) -> bool {
            self.confirmed.store(true, Ordering::SeqCst);
            if let Some(token) = &self.cancel_on_confirm {
                token.cancel();
                std::future::pending::<()>().await;
            }
            self.trust
        }
    }

    fn request(dir: &Path) -> SetupRequest {
        let req = SetupRequest {
            host: "10.11.99.1".into(),
            port: 22,
            username: "root".into(),
            timeout_secs: 1,
            password: Zeroizing::new("hunter2-secret".to_string()),
            key_path: dir.join("id_rmsync"),
            known_hosts_path: dir.join("known_hosts"),
        };
        write_private(&req.key_path, OLD_KEY);
        std::fs::write(&req.known_hosts_path, OLD_HOSTS).unwrap();
        req
    }

    fn assert_previous_config_untouched(req: &SetupRequest) {
        assert_eq!(std::fs::read_to_string(&req.key_path).unwrap(), OLD_KEY);
        assert_eq!(std::fs::read_to_string(&req.known_hosts_path).unwrap(), OLD_HOSTS);
        let s = req.staged();
        assert!(!s.key.exists() && !s.key_pub.exists() && !s.known_hosts.exists());
    }

    #[tokio::test]
    async fn successful_setup_replaces_old_key_and_host_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let out = run_guided_setup(&req, &Fake::ok(), &Observer::trusting(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.key_path, req.key_path);
        assert_eq!(out.fingerprint.display(), "SHA256:newfp");
        assert_eq!(std::fs::read_to_string(&req.key_path).unwrap(), "NEW-PRIVATE-KEY");
        assert_eq!(std::fs::read_to_string(&req.known_hosts_path).unwrap(), "ssh-ed25519 newfp\n");
        assert!(public_key_path(&req.key_path).exists());
        let mode = std::fs::metadata(&req.key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let s = req.staged();
        assert!(!s.key.exists() && !s.key_pub.exists() && !s.known_hosts.exists());
    }

    #[tokio::test]
    async fn wrong_password_keeps_previous_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let fake = Fake {
            install_err: Some(|| ConnectionError::AuthFailed(REJECTED_CREDENTIALS.to_string())),
            ..Fake::ok()
        };
        let err = run_guided_setup(&req, &fake, &Observer::trusting(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, SetupError::WrongPassword));
        assert_previous_config_untouched(&req);
    }

    #[tokio::test]
    async fn unreachable_device_is_reported_and_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let fake = Fake {
            probe_err: Some(|| ConnectionError::Unreachable("10.11.99.1".into(), 22)),
            ..Fake::ok()
        };
        let obs = Observer::trusting();
        let err = run_guided_setup(&req, &fake, &obs, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, SetupError::Unreachable(_)));
        assert!(!obs.confirmed.load(Ordering::SeqCst));
        assert_previous_config_untouched(&req);
    }

    #[tokio::test]
    async fn declining_the_fingerprint_stops_before_any_login() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let fake = Fake::ok();
        let obs = Observer { trust: false, ..Observer::trusting() };
        let err = run_guided_setup(&req, &fake, &obs, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, SetupError::DeclinedFingerprint));
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
        assert_previous_config_untouched(&req);
    }

    #[tokio::test]
    async fn cancelling_mid_flow_stops_and_keeps_previous_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let fake = Fake::ok();
        let cancel = CancellationToken::new();
        let obs = Observer { cancel_on_confirm: Some(cancel.clone()), ..Observer::trusting() };
        let err = run_guided_setup(&req, &fake, &obs, &cancel).await.unwrap_err();
        assert!(matches!(err, SetupError::Cancelled));
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
        assert_previous_config_untouched(&req);
    }

    #[tokio::test]
    async fn failed_key_login_test_keeps_previous_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let fake = Fake {
            verify_err: Some(|| ConnectionError::AuthFailed(REJECTED_CREDENTIALS.to_string())),
            ..Fake::ok()
        };
        let err = run_guided_setup(&req, &fake, &Observer::trusting(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, SetupError::KeyLoginFailed(_)));
        assert_previous_config_untouched(&req);
    }

    #[test]
    fn request_debug_never_shows_password() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        assert!(!format!("{req:?}").contains("hunter2-secret"));
    }
}

//! SSH/SFTP session management for the reMarkable tablet.
//!
//! Provides `DeviceConnection`, a thin wrapper over a russh client session and
//! russh-sftp SFTP subsystem. The module offers TOFU host-key verification,
//! password + key authentication, convenience SFTP operations, and Ed25519
//! key-pair generation for passwordless follow-up connections.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use russh::client::{self, Handle, Handler};
use russh::keys::key as russh_key;
use russh::keys::PublicKeyBase64;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

pub const DEFAULT_HOST: &str = "10.11.99.1";
pub const DEFAULT_PORT: u16 = 22;
pub const DEFAULT_USERNAME: &str = "root";
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("Device not reachable at {0}:{1}")]
    Unreachable(String, u16),
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
    #[error("SSH session error: {0}")]
    SshError(String),
    #[error("SFTP error: {0}")]
    SftpError(String),
    #[error("Not connected")]
    NotConnected,
    #[error("Host key mismatch — possible security issue")]
    HostKeyMismatch,
    #[error("Timeout after {0} seconds")]
    Timeout(u64),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub enum AuthMethod {
    Password(String),
    KeyFile(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub timeout_secs: u64,
    pub known_hosts_path: PathBuf,
    pub key_path: PathBuf,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        let cfg = dirs::config_dir().unwrap_or_default().join("rmsync");
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            username: DEFAULT_USERNAME.to_string(),
            auth: AuthMethod::Password(String::new()),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            known_hosts_path: cfg.join("known_hosts"),
            key_path: cfg.join("id_rmsync"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteFileInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    pub is_dir: bool,
}

pub struct DeviceConnection {
    session: Option<Handle<ClientHandler>>,
    sftp: Option<SftpSession>,
    config: ConnectionConfig,
}

impl DeviceConnection {
    pub fn new(config: ConnectionConfig) -> Self {
        Self {
            session: None,
            sftp: None,
            config,
        }
    }

    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    /// Quick TCP-reachability check without establishing an SSH session.
    pub async fn ping(&self) -> bool {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        matches!(
            tokio::time::timeout(
                Duration::from_secs(self.config.timeout_secs),
                TcpStream::connect(&addr),
            )
            .await,
            Ok(Ok(_))
        )
    }

    /// Establish an SSH session and open the SFTP subsystem.
    pub async fn connect(&mut self) -> Result<(), ConnectionError> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        });

        let handler = ClientHandler {
            known_hosts_path: self.config.known_hosts_path.clone(),
            matched_existing: false,
        };

        let addr = (self.config.host.as_str(), self.config.port);
        let connect_fut = client::connect(config, addr, handler);
        let mut session = match tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            connect_fut,
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(map_connect_err(e, &self.config));
            }
            Err(_) => return Err(ConnectionError::Timeout(self.config.timeout_secs)),
        };

        let authed = match &self.config.auth {
            AuthMethod::Password(pw) => session
                .authenticate_password(&self.config.username, pw)
                .await
                .map_err(|e| ConnectionError::SshError(e.to_string()))?,
            AuthMethod::KeyFile(path) => {
                let key = russh::keys::load_secret_key(path, None)
                    .map_err(|e| ConnectionError::AuthFailed(e.to_string()))?;
                session
                    .authenticate_publickey(&self.config.username, Arc::new(key))
                    .await
                    .map_err(|e| ConnectionError::SshError(e.to_string()))?
            }
        };

        if !authed {
            return Err(ConnectionError::AuthFailed(
                "server rejected credentials".to_string(),
            ));
        }

        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))?;

        self.session = Some(session);
        self.sftp = Some(sftp);
        Ok(())
    }

    pub async fn disconnect(&mut self) {
        if let Some(sftp) = self.sftp.take() {
            let _ = sftp.close().await;
        }
        if let Some(session) = self.session.take() {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "bye", "en")
                .await;
        }
    }

    pub async fn is_connected(&self) -> bool {
        match &self.session {
            Some(s) => !s.is_closed(),
            None => false,
        }
    }

    pub fn sftp(&self) -> Result<&SftpSession, ConnectionError> {
        self.sftp.as_ref().ok_or(ConnectionError::NotConnected)
    }

    // --- SFTP convenience wrappers ---

    pub async fn list_dir(&self, remote_path: &str) -> Result<Vec<RemoteFileInfo>, ConnectionError> {
        let sftp = self.sftp()?;
        let entries = sftp
            .read_dir(remote_path)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))?;
        let mut out = Vec::new();
        for entry in entries {
            let meta = entry.metadata();
            let name = entry.file_name();
            let path = join_remote(remote_path, &name);
            out.push(RemoteFileInfo {
                name,
                path,
                size: meta.len(),
                mtime: systemtime_to_unix(meta.modified().ok()),
                is_dir: meta.is_dir(),
            });
        }
        Ok(out)
    }

    pub async fn read_file(&self, remote_path: &str) -> Result<Vec<u8>, ConnectionError> {
        let sftp = self.sftp()?;
        sftp.read(remote_path)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))
    }

    pub async fn write_file(&self, remote_path: &str, data: &[u8]) -> Result<(), ConnectionError> {
        let sftp = self.sftp()?;
        sftp.write(remote_path, data)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))
    }

    pub async fn download_file(
        &self,
        remote_path: &str,
        local_path: &Path,
    ) -> Result<u64, ConnectionError> {
        let data = self.read_file(remote_path).await?;
        tokio::fs::write(local_path, &data).await?;
        Ok(data.len() as u64)
    }

    pub async fn upload_file(
        &self,
        local_path: &Path,
        remote_path: &str,
    ) -> Result<u64, ConnectionError> {
        let data = tokio::fs::read(local_path).await?;
        let len = data.len() as u64;
        self.write_file(remote_path, &data).await?;
        Ok(len)
    }

    pub async fn stat_file(&self, remote_path: &str) -> Result<RemoteFileInfo, ConnectionError> {
        let sftp = self.sftp()?;
        let meta = sftp
            .metadata(remote_path)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))?;
        let name = remote_path
            .rsplit('/')
            .next()
            .unwrap_or(remote_path)
            .to_string();
        Ok(RemoteFileInfo {
            name,
            path: remote_path.to_string(),
            size: meta.len(),
            mtime: systemtime_to_unix(meta.modified().ok()),
            is_dir: meta.is_dir(),
        })
    }

    pub async fn delete_file(&self, remote_path: &str) -> Result<(), ConnectionError> {
        let sftp = self.sftp()?;
        sftp.remove_file(remote_path)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))
    }

    pub async fn mkdir(&self, remote_path: &str) -> Result<(), ConnectionError> {
        let sftp = self.sftp()?;
        sftp.create_dir(remote_path)
            .await
            .map_err(|e| ConnectionError::SftpError(e.to_string()))
    }

    /// Generate an Ed25519 keypair, install the public key in the device's
    /// `~/.ssh/authorized_keys`, and switch this connection's config to key
    /// authentication. Returns the private-key path on success.
    pub async fn setup_key_auth(&mut self) -> Result<PathBuf, ConnectionError> {
        let keypair = russh_key::KeyPair::generate_ed25519();
        let pub_b64 = keypair.public_key_base64();
        let algorithm = keypair.name();
        let openssh_public = format!("{algorithm} {pub_b64} rmsync@local");

        let private_pem = encode_ed25519_openssh(&keypair)
            .map_err(|e| ConnectionError::AuthFailed(format!("encoding private key: {e}")))?;

        if let Some(parent) = self.config.key_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        write_private_key(&self.config.key_path, &private_pem).await?;
        let pub_path = public_key_path(&self.config.key_path);
        tokio::fs::write(&pub_path, format!("{openssh_public}\n")).await?;

        let session = self
            .session
            .as_mut()
            .ok_or(ConnectionError::NotConnected)?;

        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;
        channel
            .exec(
                true,
                "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && cat >> ~/.ssh/authorized_keys",
            )
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;
        channel
            .data(format!("{openssh_public}\n").as_bytes())
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;
        channel
            .eof()
            .await
            .map_err(|e| ConnectionError::SshError(e.to_string()))?;

        while let Some(msg) = channel.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = msg {
                if exit_status != 0 {
                    return Err(ConnectionError::AuthFailed(format!(
                        "install key: remote exit {exit_status}"
                    )));
                }
                break;
            }
        }

        self.config.auth = AuthMethod::KeyFile(self.config.key_path.clone());
        Ok(self.config.key_path.clone())
    }
}

fn map_connect_err(e: russh::Error, cfg: &ConnectionConfig) -> ConnectionError {
    match e {
        russh::Error::IO(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            ConnectionError::Unreachable(cfg.host.clone(), cfg.port)
        }
        russh::Error::IO(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            ConnectionError::Timeout(cfg.timeout_secs)
        }
        russh::Error::IO(io) => ConnectionError::Io(io),
        other => ConnectionError::SshError(other.to_string()),
    }
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn systemtime_to_unix(t: Option<std::time::SystemTime>) -> u64 {
    t.and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn public_key_path(private: &Path) -> PathBuf {
    let mut p = private.to_path_buf();
    let fname = p
        .file_name()
        .map(|f| format!("{}.pub", f.to_string_lossy()))
        .unwrap_or_else(|| "id_rmsync.pub".to_string());
    p.set_file_name(fname);
    p
}

fn encode_ed25519_openssh(kp: &russh_key::KeyPair) -> Result<String, String> {
    use ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
    use ssh_key::{LineEnding, PrivateKey};

    let russh_key::KeyPair::Ed25519(signing) = kp else {
        return Err("only ed25519 keypairs are supported".to_string());
    };
    let secret_bytes = signing.to_bytes();
    let verifying = signing.verifying_key();
    let pair = Ed25519Keypair {
        public: ssh_key::public::Ed25519PublicKey(verifying.to_bytes()),
        private: Ed25519PrivateKey::from_bytes(&secret_bytes),
    };
    let private = PrivateKey::new(KeypairData::Ed25519(pair), "rmsync")
        .map_err(|e| format!("new PrivateKey: {e}"))?;
    private
        .to_openssh(LineEnding::LF)
        .map(|z| z.to_string())
        .map_err(|e| format!("to_openssh: {e}"))
}

async fn write_private_key(path: &Path, pem: &str) -> Result<(), std::io::Error> {
    use tokio::fs::OpenOptions;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).await?;
    f.write_all(pem.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

/// russh client handler implementing trust-on-first-use host-key pinning.
pub(crate) struct ClientHandler {
    known_hosts_path: PathBuf,
    matched_existing: bool,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint();
        match std::fs::read_to_string(&self.known_hosts_path) {
            Ok(existing) => {
                let stored = existing.trim();
                if stored.is_empty() {
                    write_fingerprint(&self.known_hosts_path, &fingerprint)?;
                    Ok(true)
                } else if stored == fingerprint {
                    self.matched_existing = true;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_fingerprint(&self.known_hosts_path, &fingerprint)?;
                Ok(true)
            }
            Err(e) => Err(russh::Error::IO(e)),
        }
    }
}

fn write_fingerprint(path: &Path, fingerprint: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{fingerprint}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = ConnectionConfig::default();
        assert_eq!(cfg.host, DEFAULT_HOST);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.username, DEFAULT_USERNAME);
        assert_eq!(cfg.timeout_secs, DEFAULT_TIMEOUT_SECS);
        matches!(cfg.auth, AuthMethod::Password(_));
    }

    #[test]
    fn auth_method_variants_construct() {
        let a = AuthMethod::Password("hunter2".into());
        let b = AuthMethod::KeyFile(PathBuf::from("/tmp/key"));
        match a {
            AuthMethod::Password(p) => assert_eq!(p, "hunter2"),
            _ => panic!("wrong variant"),
        }
        match b {
            AuthMethod::KeyFile(p) => assert_eq!(p, PathBuf::from("/tmp/key")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn remote_file_info_struct_builds() {
        let info = RemoteFileInfo {
            name: "x.pdf".into(),
            path: "/home/root/x.pdf".into(),
            size: 42,
            mtime: 1_700_000_000,
            is_dir: false,
        };
        assert_eq!(info.name, "x.pdf");
        assert_eq!(info.size, 42);
        assert!(!info.is_dir);
    }

    #[test]
    fn join_remote_adds_slash_when_missing() {
        assert_eq!(join_remote("/a/b", "c"), "/a/b/c");
        assert_eq!(join_remote("/a/b/", "c"), "/a/b/c");
    }

    #[test]
    fn public_key_path_appends_pub() {
        let p = public_key_path(Path::new("/home/r/.config/rmsync/id_rmsync"));
        assert_eq!(
            p,
            PathBuf::from("/home/r/.config/rmsync/id_rmsync.pub")
        );
    }

    #[tokio::test]
    async fn ping_unreachable_returns_false() {
        let cfg = ConnectionConfig {
            // RFC 5737 TEST-NET-1 — guaranteed non-routable.
            host: "192.0.2.1".to_string(),
            port: 22,
            timeout_secs: 1,
            ..ConnectionConfig::default()
        };
        let conn = DeviceConnection::new(cfg);
        assert!(!conn.ping().await);
    }

    #[tokio::test]
    async fn sftp_before_connect_is_not_connected() {
        let conn = DeviceConnection::new(ConnectionConfig::default());
        assert!(matches!(conn.sftp(), Err(ConnectionError::NotConnected)));
        assert!(!conn.is_connected().await);
    }

    #[test]
    fn tofu_accepts_on_first_sight_and_rejects_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        write_fingerprint(&path, "SHA256:abc").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.trim() == "SHA256:abc");
    }

    #[test]
    fn encode_ed25519_openssh_produces_valid_pem() {
        let kp = russh_key::KeyPair::generate_ed25519();
        let pem = encode_ed25519_openssh(&kp).expect("encode");
        assert!(pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(pem.trim_end().ends_with("-----END OPENSSH PRIVATE KEY-----"));
    }
}

# Spec 06 — SSH/SFTP Connection Module

**Layer:** 1 — Connectivity  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 2 hours  

## Objective

Implement a connection manager that establishes and maintains SSH/SFTP sessions to the reMarkable 2 over USB virtual Ethernet, supporting both password and key-based authentication with connection pooling.

## Context

The reMarkable 2 creates a virtual Ethernet interface over USB-C. It runs an SSH server at `10.11.99.1` accessible as the `root` user. The password is found in Settings → General → Software → Developer Mode. For production use, the app should generate and install an SSH keypair for passwordless access. All file operations happen over SFTP on this connection.

## Technical Requirements

### 1. Connection manager (`src/device/connection.rs`)

```rust
pub struct DeviceConnection {
    session: Option<russh::client::Handle<ClientHandler>>,
    sftp: Option<russh_sftp::client::SftpSession>,
    config: ConnectionConfig,
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,              // Default: "10.11.99.1"
    pub port: u16,                 // Default: 22
    pub username: String,          // Default: "root"
    pub auth: AuthMethod,
    pub timeout_secs: u64,         // Default: 5
    pub known_hosts_path: PathBuf,
    pub key_path: PathBuf,         // ~/.config/rmsync/id_rmsync
}

#[derive(Debug, Clone)]
pub enum AuthMethod {
    Password(String),
    KeyFile(PathBuf),
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            host: "10.11.99.1".to_string(),
            port: 22,
            username: "root".to_string(),
            auth: AuthMethod::Password(String::new()),
            timeout_secs: 5,
            known_hosts_path: dirs::config_dir()
                .unwrap_or_default()
                .join("rmsync/known_hosts"),
            key_path: dirs::config_dir()
                .unwrap_or_default()
                .join("rmsync/id_rmsync"),
        }
    }
}
```

### 2. Core operations

```rust
impl DeviceConnection {
    /// Create a new connection manager with the given config.
    pub fn new(config: ConnectionConfig) -> Self

    /// Attempt to connect to the reMarkable via SSH.
    /// Returns Ok(()) on success, Err with details on failure.
    pub async fn connect(&mut self) -> Result<()>

    /// Disconnect and clean up the session.
    pub async fn disconnect(&mut self)

    /// Check if the connection is alive (SSH keepalive ping).
    pub async fn is_connected(&self) -> bool

    /// Get a reference to the SFTP session. Returns Err if not connected.
    pub fn sftp(&self) -> Result<&SftpSession>

    /// Test connectivity without establishing a full session.
    /// Just checks if TCP port 22 is reachable at the configured host.
    pub async fn ping(&self) -> bool

    /// Generate an Ed25519 keypair and install the public key on the device.
    /// Requires an active password-authenticated session.
    pub async fn setup_key_auth(&mut self) -> Result<PathBuf>
}
```

### 3. SFTP convenience wrapper (`src/device/connection.rs`)

```rust
impl DeviceConnection {
    /// List all files in a remote directory.
    pub async fn list_dir(&self, remote_path: &str) -> Result<Vec<RemoteFileInfo>>

    /// Read a remote file's contents into memory.
    pub async fn read_file(&self, remote_path: &str) -> Result<Vec<u8>>

    /// Write data to a remote file (creates or overwrites).
    pub async fn write_file(&self, remote_path: &str, data: &[u8]) -> Result<()>

    /// Download a remote file to a local path.
    pub async fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<u64>

    /// Upload a local file to a remote path.
    pub async fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<u64>

    /// Get file metadata (size, mtime) for a remote path.
    pub async fn stat_file(&self, remote_path: &str) -> Result<RemoteFileInfo>

    /// Delete a remote file.
    pub async fn delete_file(&self, remote_path: &str) -> Result<()>

    /// Create a remote directory (non-recursive).
    pub async fn mkdir(&self, remote_path: &str) -> Result<()>
}

#[derive(Debug, Clone)]
pub struct RemoteFileInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: u64,        // Unix timestamp
    pub is_dir: bool,
}
```

### 4. SSH client handler

Implement the `russh::client::Handler` trait:

```rust
struct ClientHandler {
    /// Accept all host keys on first connection (TOFU model).
    /// Store the key fingerprint in known_hosts_path for future verification.
}
```

On subsequent connections, verify the host key matches the stored fingerprint. If it doesn't, return an error (possible security issue).

### 5. Key generation for `setup_key_auth`

1. Generate an Ed25519 keypair using Rust crypto (e.g., `ssh-key` crate or `russh-keys`).
2. Save the private key to `~/.config/rmsync/id_rmsync` (permissions 0600).
3. Save the public key to `~/.config/rmsync/id_rmsync.pub`.
4. Over the existing password session, append the public key to `/root/.ssh/authorized_keys` on the device.
5. Update `ConnectionConfig` to use `AuthMethod::KeyFile` going forward.

### 6. Error handling

```rust
#[derive(Debug, thiserror::Error)]
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
```

### 7. Reconnection logic

If an SFTP operation fails due to a broken connection, the caller should get a `ConnectionError::SshError` or `NotConnected`. The sync engine (Spec 12/13) will handle retry logic. This module does NOT auto-reconnect — it provides the primitives.

## Files to Create/Modify

- `src/device/connection.rs` — full implementation
- `src/device/mod.rs` — export the module
- Add `dirs = "5"` and `ssh-key = "0.6"` (or `russh-keys`) to `Cargo.toml` if not already present.

## Test Strategy

> Note: Most tests will be integration tests requiring a live device or mock. Unit tests should focus on config and error handling.

1. **Default config** — verify `ConnectionConfig::default()` produces correct host/port/username.
2. **Ping unreachable host** — call `ping()` against a non-existent IP, verify it returns `false` within the timeout period.
3. **Auth method serialization** — verify `AuthMethod` variants work correctly.
4. **RemoteFileInfo construction** — verify struct construction and field access.
5. **Mock SFTP test (if feasible)** — if `russh` supports test fixtures, mock a session and verify `list_dir` / `read_file` contract.

For live device testing (manual):
6. **Connect with password** — verify SSH session establishes.
7. **List xochitl directory** — verify files are returned.
8. **Download a .metadata file** — verify content is valid JSON.
9. **Setup key auth** — verify keypair is generated and subsequent connections work passwordless.

## Acceptance Criteria

1. `connect()` establishes an SSH session and SFTP channel to `10.11.99.1`.
2. `ping()` returns true/false within the timeout period.
3. All SFTP convenience methods (list, read, write, download, upload, stat, delete, mkdir) work correctly.
4. `setup_key_auth()` generates a keypair and installs it on the device.
5. Errors are typed and descriptive.
6. Unit tests pass. Integration tests pass when a device is connected.

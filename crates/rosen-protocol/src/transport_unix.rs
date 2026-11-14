//! Unix-socket transport (best-effort platforms, SPEC D11). The socket lives in a
//! directory only the current user can write, and the socket itself is mode
//! 0600, which limits access to that user. Clients check the server's uid.

use super::BoxStream;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

pub(super) fn normalize(name: String) -> String {
    name
}

pub(super) fn default_endpoint() -> String {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("rosen"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
            std::env::temp_dir().join(format!("rosen-{user}"))
        }
    };
    dir.join("rosen.sock").to_string_lossy().into_owned()
}

pub(super) struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub(super) fn bind(path: &str) -> io::Result<Self> {
        let path = Path::new(path);
        if let Some(dir) = path.parent() {
            // A bare file name has an empty parent: the working directory.
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            require_private_dir(dir)?;
        }
        let inner = match UnixListener::bind(path) {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => rebind_if_stale(path)?,
            Err(err) => return Err(err),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner })
    }

    pub(super) async fn accept(&mut self) -> io::Result<BoxStream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(Box::new(stream))
    }
}

/// A socket file left by a crashed server still exists but refuses connections;
/// only then is it safe to remove and bind again.
fn rebind_if_stale(path: &Path) -> io::Result<UnixListener> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is already in use", path.display()),
        ));
    }
    std::fs::remove_file(path)?;
    UnixListener::bind(path)
}

/// `create` succeeds on a directory that already exists, so a directory another
/// user made first (in a shared `/tmp`) would be accepted and that user could
/// replace the socket. Refuse unless we own it and nobody else can write to it.
fn require_private_dir(dir: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    let foreign = meta.uid() != current_uid();
    if !meta.is_dir() || foreign || meta.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a directory owned by the current user and writable only by them",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Connects and refuses a server running as another user, who could have
/// bound the socket path first.
pub(super) async fn connect(path: &str) -> io::Result<BoxStream> {
    let stream = UnixStream::connect(path).await?;
    let server = stream.peer_cred()?.uid();
    if server != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path} is served by another user (uid {server}); refusing to connect"),
        ));
    }
    Ok(Box::new(stream))
}

#[allow(unsafe_code)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

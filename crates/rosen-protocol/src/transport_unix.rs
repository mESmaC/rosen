//! Unix-socket transport (best-effort platforms, SPEC D11). The socket lives in a
//! directory only the current user can enter, which limits access to that user.

use super::BoxStream;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
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

pub(super) async fn connect(path: &str) -> io::Result<BoxStream> {
    Ok(Box::new(UnixStream::connect(path).await?))
}

//! The v1 local transport: a named pipe on Windows and a Unix socket elsewhere,
//! both limited to the current OS user (SPEC §1 "Transport").
//!
//! Callers see only [`Listener`], [`connect`] and boxed byte streams, so a network
//! transport can be added later without touching the protocol above it.

use std::fmt;
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

/// The environment variable that overrides the default endpoint, for running more
/// than one daemon (tests, side-by-side builds) on one machine.
pub const ENDPOINT_ENV: &str = "ROSEN_ENDPOINT";

/// A bidirectional byte stream to one peer.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

pub type BoxStream = Box<dyn Duplex>;

/// Where a server listens: a pipe name on Windows, a socket path elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    /// Wraps a pipe name or socket path. On Windows a bare name gets the
    /// `\\.\pipe\` prefix.
    pub fn new(name: impl Into<String>) -> Self {
        Self(imp::normalize(name.into()))
    }

    /// `ROSEN_ENDPOINT` when set, otherwise the per-user default endpoint.
    pub fn resolve() -> Self {
        match std::env::var(ENDPOINT_ENV) {
            Ok(name) if !name.is_empty() => Self::new(name),
            _ => Self(imp::default_endpoint()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Accepts client connections on one endpoint.
pub struct Listener {
    inner: imp::Listener,
}

impl Listener {
    /// Starts listening. Fails with [`io::ErrorKind::AddrInUse`] when another
    /// server already owns the endpoint, so one machine never runs two daemons
    /// on the same endpoint.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        imp::Listener::bind(endpoint.as_str()).map(|inner| Self { inner })
    }

    /// Waits for the next client.
    pub async fn accept(&mut self) -> io::Result<BoxStream> {
        self.inner.accept().await
    }
}

/// Connects to a server. Fails with [`io::ErrorKind::NotFound`] or
/// [`io::ErrorKind::ConnectionRefused`] when no server listens there.
pub async fn connect(endpoint: &Endpoint) -> io::Result<BoxStream> {
    imp::connect(endpoint.as_str()).await
}

/// True when a connect error means "no server is listening", as opposed to a
/// server that exists but failed.
pub fn is_not_running(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(windows)]
#[path = "transport_windows.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "transport_unix.rs"]
mod imp;

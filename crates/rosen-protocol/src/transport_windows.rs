//! Named-pipe transport. Each pipe instance serves one client, so the listener
//! keeps one idle instance ready and creates the next as soon as a client takes it.

use super::BoxStream;
use rosen_platform::security::{
    current_user_sid, named_pipe_server_pid, process_user_sid, OwnerOnlyDescriptor, Rights,
};
use std::ffi::c_void;
use std::io;
use std::time::Duration;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

const PIPE_PREFIX: &str = r"\\.\pipe\";

/// How long a client keeps retrying while every pipe instance is busy.
const BUSY_RETRY_LIMIT: Duration = Duration::from_secs(5);

pub(super) fn normalize(name: String) -> String {
    if name.starts_with(PIPE_PREFIX) {
        name
    } else {
        format!("{PIPE_PREFIX}{name}")
    }
}

/// The pipe namespace is machine-wide, so the default name carries the user's
/// SID: unlike `USERNAME`, it is unique across local and domain accounts.
pub(super) fn default_endpoint() -> String {
    let user = current_user_sid()
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".into());
    format!("{PIPE_PREFIX}rosen-{user}")
}

pub(super) struct Listener {
    name: String,
    /// Full access for the current user and SYSTEM only. The default pipe
    /// DACL also grants read access to Everyone, which would break the
    /// "access is limited to the OS user" rule.
    security: OwnerOnlyDescriptor,
    next: NamedPipeServer,
}

impl Listener {
    pub(super) fn bind(name: &str) -> io::Result<Self> {
        let security = OwnerOnlyDescriptor::for_current_user(Rights::GenericAll)?;
        let first = create_instance(name, &security, true).map_err(|err| {
            // A pipe that already exists refuses a "first instance" create.
            if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{name} is already in use"),
                )
            } else {
                err
            }
        })?;
        Ok(Self {
            name: name.to_owned(),
            security,
            next: first,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<BoxStream> {
        self.next.connect().await?;
        // Create the replacement before handing off, so a client arriving now
        // finds an instance instead of "not found".
        let replacement = create_instance(&self.name, &self.security, false)?;
        let connected = std::mem::replace(&mut self.next, replacement);
        Ok(Box::new(connected))
    }
}

fn create_instance(
    name: &str,
    security: &OwnerOnlyDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = security.attributes();
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attributes` points at a security descriptor owned by `security`,
    // which outlives this call; the OS copies it into the new pipe object.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

pub(super) async fn connect(name: &str) -> io::Result<BoxStream> {
    let started = tokio::time::Instant::now();
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => {
                verify_server_owner(name, &client)?;
                return Ok(Box::new(client));
            }
            // Every instance is taken for the moment; the listener makes a new one
            // right after each accept, so a short wait is enough.
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && started.elapsed() < BUSY_RETRY_LIMIT =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Refuses a pipe whose server process runs as another user. Pipe names are
/// machine-wide, so another account can create ours first and impersonate the
/// daemon; the DACL the real server sets cannot stop that, only this check can.
fn verify_server_owner(name: &str, pipe: &NamedPipeClient) -> io::Result<()> {
    let pid = named_pipe_server_pid(pipe)?;
    let owner = process_user_sid(pid).map_err(|err| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cannot verify the owner of {name} (server process {pid}): {err}"),
        )
    })?;
    if owner != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} is served by another user ({owner}, process {pid}); refusing to connect"),
        ));
    }
    Ok(())
}

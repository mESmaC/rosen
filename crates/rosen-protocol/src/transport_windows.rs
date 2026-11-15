//! Named-pipe transport. Each pipe instance serves one client, so the listener
//! keeps a pool ready so connection bursts do not wait for the accept loop.

use super::BoxStream;
use rosen_platform::security::{
    current_user_sid, named_pipe_server_pid, process_user_sid, OwnerOnlyDescriptor, Rights,
};
use std::ffi::c_void;
use std::future::{poll_fn, Future};
use std::io;
use std::task::Poll;
use std::time::Duration;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

const PIPE_PREFIX: &str = r"\\.\pipe\";

/// How long a client keeps retrying while every pipe instance is busy.
const BUSY_RETRY_LIMIT: Duration = Duration::from_secs(30);
const BUSY_RETRY_INITIAL: Duration = Duration::from_millis(10);
const BUSY_RETRY_MAX: Duration = Duration::from_millis(250);
const WAITING_INSTANCES: usize = 8;

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
    waiting: Vec<NamedPipeServer>,
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
        let mut waiting = vec![first];
        for _ in 1..WAITING_INSTANCES {
            waiting.push(create_instance(name, &security, false)?);
        }
        Ok(Self {
            name: name.to_owned(),
            security,
            waiting,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<BoxStream> {
        let index = connected_instance(&self.waiting).await?;
        // Refill before handing off so the accept loop keeps the full backlog.
        let replacement = create_instance(&self.name, &self.security, false)?;
        let connected = std::mem::replace(&mut self.waiting[index], replacement);
        Ok(Box::new(connected))
    }
}

async fn connected_instance(waiting: &[NamedPipeServer]) -> io::Result<usize> {
    // Poll every instance: Windows can connect a client to any waiting handle.
    // Keeping the handles in the listener also makes cancelling accept safe.
    let mut connections: Vec<_> = waiting
        .iter()
        .map(|pipe| Box::pin(pipe.connect()))
        .collect();
    poll_fn(|cx| {
        for (index, connection) in connections.iter_mut().enumerate() {
            if let Poll::Ready(result) = connection.as_mut().poll(cx) {
                return Poll::Ready(result.map(|()| index));
            }
        }
        Poll::Pending
    })
    .await
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
    let mut delay = BUSY_RETRY_INITIAL;
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => {
                verify_server_owner(name, &client)?;
                return Ok(Box::new(client));
            }
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let remaining = BUSY_RETRY_LIMIT.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "cannot connect to {name} after waiting {BUSY_RETRY_LIMIT:?} \
                             for a free pipe instance: {err}"
                        ),
                    ));
                }
                // Yield without blocking the runtime, and bound retry pressure
                // when a burst exceeds the listener's pool or it is stalled.
                tokio::time::sleep(delay.min(remaining)).await;
                delay = (delay * 2).min(BUSY_RETRY_MAX);
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("cannot connect to {name}: {err}"),
                ));
            }
        }
    }
}

/// Refuses a pipe whose server process runs as another user. Pipe names are
/// machine-wide, so another account can create ours first and impersonate the
/// daemon; the DACL the real server sets cannot stop that, only this check can.
fn verify_server_owner(name: &str, pipe: &NamedPipeClient) -> io::Result<()> {
    let pid = named_pipe_server_pid(pipe)?;
    let owner = process_user_sid(pid).map_err(|err| server_owner_error(name, pid, err))?;
    if owner != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{name} is served by another user ({owner}, process {pid}); refusing to connect"
            ),
        ));
    }
    Ok(())
}

fn server_owner_error(name: &str, pid: u32, err: io::Error) -> io::Error {
    // Access denial cannot establish whose daemon this is. Offer recovery for
    // an elevated daemon without treating elevation as proof of ownership.
    let hint = if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        "; the daemon may be running elevated or as another user. If this is your daemon, \
         stop it from the shell that started it and restart it from a non-elevated shell"
    } else {
        ""
    };
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("cannot verify the owner of {name} (server process {pid}): {err}{hint}; refusing to connect"),
    )
}

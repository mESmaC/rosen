//! DNS-SD is only an address hint; enrollment must still pin the TLS certificate.
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{io, net::SocketAddr, time::Duration};

const SERVICE: &str = "_rosen._tcp.local.";

/// Keeps the announcement alive until the listener stops.
pub struct Advertisement(ServiceDaemon);

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

pub fn advertise(address: SocketAddr, fingerprint: &str) -> io::Result<Advertisement> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid certificate fingerprint",
        ));
    }
    let daemon = ServiceDaemon::new().map_err(io::Error::other)?;
    let instance = format!("rosen-{}-{}", &fingerprint[..16], address.port());
    let hostname = format!("{instance}.local.");
    let properties = [("fingerprint", fingerprint)];
    let addresses = if address.ip().is_unspecified() {
        String::new()
    } else {
        address.ip().to_string()
    };
    let mut info = ServiceInfo::new(
        SERVICE,
        &instance,
        &hostname,
        addresses.as_str(),
        address.port(),
        &properties[..],
    )
    .map_err(io::Error::other)?;
    if address.ip().is_unspecified() {
        info = info.enable_addr_auto();
    }
    daemon.register(info).map_err(io::Error::other)?;
    Ok(Advertisement(daemon))
}

/// Finds LAN endpoints advertising this project within five seconds.
/// TXT records are untrusted: callers authenticate each endpoint with TLS.
pub async fn discover(fingerprint: &str) -> io::Result<Vec<String>> {
    let daemon = ServiceDaemon::new().map_err(io::Error::other)?;
    let guard = Advertisement(daemon);
    let receiver = guard.0.browse(SERVICE).map_err(io::Error::other)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut addresses = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, receiver.recv_async()).await {
        if let ServiceEvent::ServiceResolved(info) = event {
            if info.get_property_val_str("fingerprint") != Some(fingerprint) {
                continue;
            }
            for ip in info.get_addresses() {
                let address = if ip.is_ipv6() {
                    format!("[{}]:{}", ip, info.get_port())
                } else {
                    format!("{}:{}", ip, info.get_port())
                };
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
    }
    Ok(addresses)
}

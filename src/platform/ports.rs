//! TCP and UDP ports: which app takes which port, and the listeners.
//!
//! An app declares a port in toolsite.toml (`[[socket]] protocol = "tcp"`,
//! `port = 1883`), but a declaration opens nothing. A port is the server's,
//! not the app's: the site's owner maps it to one app with `TOOLSITE_PORTS`
//! (`1883=mqtt-broker,5514/udp=syslog`), and a port is live only while it
//! is mapped to an app that declares it. Two apps never share one.
//!
//! Every mapped port is bound at boot, whether or not its app declares it
//! yet, so publishing the app later needs no restart. Each connection that
//! arrives is admitted only if the app exists, is not hidden and declares
//! the port now; otherwise it is closed at once.

use crate::{
    config::Config,
    content::store::{self, PortProtocol, PortSocket},
    runtime::wasm::Runtime,
    AppState,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

/// Ports below this need root, and are where the system's own services live.
pub const MIN_PORT: u16 = 1024;

/// One port the site's owner gave to one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub socket: PortSocket,
    pub app: String,
}

/// The owner's mapping of ports to apps, and the address the listeners
/// bind.
#[derive(Debug, Clone)]
pub struct PortMap {
    /// 0.0.0.0 when deployed; tests bind 127.0.0.1.
    pub bind: IpAddr,
    pub mappings: Vec<Mapping>,
}

impl Default for PortMap {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            mappings: Vec::new(),
        }
    }
}

impl PortMap {
    /// The app a port is mapped to, if any.
    pub fn app_for(&self, socket: PortSocket) -> Option<&str> {
        self.mappings.iter().find(|m| m.socket == socket).map(|m| m.app.as_str())
    }

    /// Whether `socket` is mapped to `app`.
    pub fn maps(&self, app: &str, socket: PortSocket) -> bool {
        self.app_for(socket) == Some(app)
    }
}

/// Reads `TOOLSITE_PORTS`: `<port>[/tcp|/udp]=<app>`, comma-separated. The
/// protocol defaults to tcp. A port listed twice is refused, so two apps
/// can never share one.
pub fn parse(spec: &str) -> Result<Vec<Mapping>, String> {
    let mut mappings: Vec<Mapping> = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        let (port, app) = item
            .split_once('=')
            .ok_or_else(|| format!("TOOLSITE_PORTS: {item:?} is not <port>=<app>"))?;
        let (port, protocol) = match port.trim().split_once('/') {
            Some((port, "tcp")) => (port, PortProtocol::Tcp),
            Some((port, "udp")) => (port, PortProtocol::Udp),
            Some((_, other)) => return Err(format!("TOOLSITE_PORTS: protocol must be tcp or udp, not {other:?}")),
            None => (port.trim(), PortProtocol::Tcp),
        };
        let port: u16 = port
            .parse()
            .ok()
            .filter(|port| *port >= MIN_PORT)
            .ok_or_else(|| format!("TOOLSITE_PORTS: port must be {MIN_PORT} to 65535, not {port:?}"))?;
        let app = app.trim();
        if !crate::platform::export::valid_app(app) {
            return Err(format!("TOOLSITE_PORTS: {app:?} is not an app name"));
        }
        let socket = PortSocket { protocol, port };
        if let Some(taken) = mappings.iter().find(|m| m.socket == socket) {
            return Err(format!("TOOLSITE_PORTS: {socket} is mapped to both {} and {app}; a port goes to one app", taken.app));
        }
        mappings.push(Mapping { socket, app: app.to_string() });
    }
    Ok(mappings)
}

/// Whether a connection on `socket` may reach `app` now: the port is mapped
/// to it, and the app exists, is not hidden and declares the port.
pub async fn admits(config: &Config, app: &str, socket: PortSocket) -> bool {
    config.ports.maps(app, socket)
        && store::app_exists(config, app).await
        && !store::is_hidden(config, app).await
        && store::read_meta(config, app).await.ports.contains(&socket)
}

/// Binds every mapped port and starts taking connections on it. Returns the
/// addresses bound. A port that cannot be bound is an error: a mapping is
/// the owner's promise that the port works.
pub async fn listen(config: Arc<Config>, runtime: Arc<Runtime>) -> std::io::Result<Vec<SocketAddr>> {
    let state = AppState { config: config.clone(), runtime };
    let mut bound = Vec::new();
    for mapping in &config.ports.mappings {
        let addr = SocketAddr::new(config.ports.bind, mapping.socket.port);
        let (app, socket) = (mapping.app.clone(), mapping.socket);
        match socket.protocol {
            PortProtocol::Tcp => {
                let listener = tokio::net::TcpListener::bind(addr).await?;
                bound.push(listener.local_addr()?);
                tokio::spawn(crate::platform::tcp::serve(state.clone(), listener, app.clone(), socket));
            }
            PortProtocol::Udp => {
                let udp = tokio::net::UdpSocket::bind(addr).await?;
                bound.push(udp.local_addr()?);
                tokio::spawn(crate::platform::udp::serve(state.clone(), udp, app.clone(), socket));
            }
        }
        tracing::info!(app = %app, port = %socket, "listening for the app's connections");
    }
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_map_reads_ports_protocols_and_apps() {
        let map = parse("1883=mqtt-broker, 5514/udp=syslog,8883/tcp=mqtt-broker").unwrap();
        assert_eq!(
            map,
            vec![
                Mapping { socket: PortSocket { protocol: PortProtocol::Tcp, port: 1883 }, app: "mqtt-broker".into() },
                Mapping { socket: PortSocket { protocol: PortProtocol::Udp, port: 5514 }, app: "syslog".into() },
                Mapping { socket: PortSocket { protocol: PortProtocol::Tcp, port: 8883 }, app: "mqtt-broker".into() },
            ]
        );
        assert_eq!(parse("").unwrap(), vec![]);
        // One number as TCP and as UDP is two ports.
        assert_eq!(parse("5514=a,5514/udp=b").unwrap().len(), 2);
    }

    #[test]
    fn two_apps_can_never_map_one_port() {
        assert!(parse("1883=a,1883=b").unwrap_err().contains("one app"));
        assert!(parse("1883/tcp=a,1883=b").is_err());
        assert!(parse("5514/udp=a,5514/udp=a").is_err());
    }

    #[test]
    fn a_port_map_refuses_system_ports_and_names_that_are_not_apps() {
        for bad in ["80=a", "0=a", "70000=a", "x=a", "1883=../a", "1883=a/b", "1883=", "1883", "1883/sctp=a"] {
            assert!(parse(bad).is_err(), "{bad} was taken");
        }
    }
}

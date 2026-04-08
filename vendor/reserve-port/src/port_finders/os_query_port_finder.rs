use ::std::net::IpAddr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;

use super::PortFinder;
use crate::bind_and_get_tcp;

const NUM_TRIES: usize = 100;

/// A `PortFinder` which will ask the OS for the port.
#[derive(Default)]
pub struct OsQueryPortFinder {}

impl OsQueryPortFinder {
    pub const fn new() -> Self {
        Self {}
    }
}

impl PortFinder for OsQueryPortFinder {
    fn find_port_for_ip(&mut self, ip: IpAddr) -> Option<(TcpListener, SocketAddr)> {
        (0..NUM_TRIES).find_map(|_| ask_free_tcp_port_for_ip(ip))
    }
}

/// Ask the OS for a TCP port that is available.
fn ask_free_tcp_port_for_ip(ip: IpAddr) -> Option<(TcpListener, SocketAddr)> {
    let ipv = SocketAddr::new(ip, 0);

    bind_and_get_tcp(ipv)
}

#[cfg(test)]
mod tests_find_port {
    use super::*;

    #[test]
    fn it_finds_a_random_port() {
        assert!(OsQueryPortFinder::new().find_port().is_some());
    }
}

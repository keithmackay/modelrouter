use ::std::net::IpAddr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;

use super::OsQueryPortFinder;
use super::PortFinder;
use super::ScanningPortFinder;

pub struct ScanningWithFallbackPortFinder<const MIN: u16, const MAX: u16> {
    scanner: ScanningPortFinder<MIN, MAX>,
    random: OsQueryPortFinder,
}

impl<const MIN: u16, const MAX: u16> ScanningWithFallbackPortFinder<MIN, MAX> {
    pub const fn new() -> Self {
        Self {
            scanner: ScanningPortFinder::new(),
            random: OsQueryPortFinder::new(),
        }
    }
}

impl<const MIN: u16, const MAX: u16> PortFinder for ScanningWithFallbackPortFinder<MIN, MAX> {
    fn find_port_for_ip(&mut self, ip: IpAddr) -> Option<(TcpListener, SocketAddr)> {
        self.scanner
            .find_port_for_ip(ip)
            .or_else(|| self.random.find_port_for_ip(ip))
    }
}

impl<const MIN: u16, const MAX: u16> Default for ScanningWithFallbackPortFinder<MIN, MAX> {
    fn default() -> Self {
        Self::new()
    }
}

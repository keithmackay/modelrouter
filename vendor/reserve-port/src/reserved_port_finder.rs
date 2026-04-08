use crate::Error;
use crate::PortFinder;
use crate::ScanningWithFallbackPortFinder;
use ::std::collections::HashSet;
use ::std::net::IpAddr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;
use ::std::sync::Mutex;
use std::sync::LazyLock;
use std::sync::MutexGuard;

const MIN_PORT: u16 = 8_000;
const MAX_PORT: u16 = 15_999;

static GLOBAL_PORT_FINDER: LazyLock<Mutex<ReservedPortFinder<MIN_PORT, MAX_PORT>>> =
    LazyLock::new(|| Mutex::new(ReservedPortFinder::new()));

pub(crate) fn borrow_global_port_finder()
-> Result<MutexGuard<'static, ReservedPortFinder<MIN_PORT, MAX_PORT>>, Error> {
    let port_finder = GLOBAL_PORT_FINDER
        .lock()
        .map_err(|_| Error::InternalLockError)?;

    Ok(port_finder)
}

pub struct ReservedPortFinder<const MIN: u16, const MAX: u16> {
    finder: ScanningWithFallbackPortFinder<MIN, MAX>,
    ports_in_use: HashSet<u16>,
}

impl<const MIN: u16, const MAX: u16> ReservedPortFinder<MIN, MAX> {
    pub fn new() -> Self {
        Self {
            finder: ScanningWithFallbackPortFinder::new(),
            ports_in_use: HashSet::new(),
        }
    }

    /// Sets a port to be _permanently_ reserved.
    ///
    /// Reservations only affect this port scanner.
    pub fn reserve_port(&mut self, port: u16) {
        self.ports_in_use.insert(port);
    }

    #[must_use]
    pub fn reserve_random_tcp(&mut self, ip: IpAddr) -> Option<(TcpListener, SocketAddr)> {
        // As long as the port finder can keep finding ports,
        // we keep spinning, and checking if the port is in use.
        //
        // When a free port is found, we return it.
        while let Some((tcp_listener, socket_addr)) = self.finder.find_port_for_ip(ip) {
            let port = socket_addr.port();
            if self.ports_in_use.contains(&port) {
                continue;
            }

            self.ports_in_use.insert(port);
            return Some((tcp_listener, socket_addr));
        }

        None
    }

    #[must_use]
    pub fn reserve_random_port(&mut self) -> Option<u16> {
        // As long as the port finder can keep finding ports,
        // we keep spinning, and checking if the port is in use.
        //
        // When a free port is found, we return it.
        while let Some(port) = self.finder.find_port() {
            if self.ports_in_use.contains(&port) {
                continue;
            }

            self.ports_in_use.insert(port);
            return Some(port);
        }

        None
    }

    pub fn free_port(&mut self, port: u16) {
        self.ports_in_use.remove(&port);
    }
}

impl<const MIN: u16, const MAX: u16> Default for ReservedPortFinder<MIN, MAX> {
    fn default() -> Self {
        Self::new()
    }
}

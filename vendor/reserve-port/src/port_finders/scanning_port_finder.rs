use ::std::net::IpAddr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;

use crate::bind_and_get_tcp;
use crate::port_finders::PortFinder;

const RETRY_FOUND_MIN: u32 = 500;

/// A `PortFinder` which will scan for ports along a known range.
pub struct ScanningPortFinder<const MIN: u16, const MAX: u16> {
    last: u16,
    found_count: u32,
}

impl<const MIN: u16, const MAX: u16> ScanningPortFinder<MIN, MAX> {
    pub const fn new() -> Self {
        Self {
            last: MIN,
            found_count: 0,
        }
    }
}

impl<const MIN: u16, const MAX: u16> PortFinder for ScanningPortFinder<MIN, MAX> {
    fn find_port_for_ip(&mut self, ip: IpAddr) -> Option<(TcpListener, SocketAddr)> {
        // This is hit if we loop round the port list,
        // and come back to the start.
        if self.last >= MAX {
            // We found very few ports,
            // then don't bother wrapping,
            if self.found_count < RETRY_FOUND_MIN {
                return None;
            }

            // Otherwise reset the min and wrap around.
            // We will probably find the ports we used last time.
            *self = Self::new();
        }

        let maybe_found = (self.last..MAX).find_map(|port| {
            let socket_addr = SocketAddr::new(ip, port);
            bind_and_get_tcp(socket_addr)
        });

        if let Some((_, socket_addr)) = maybe_found {
            // Set 1 last the port, so we don't start on that next time.
            self.last = socket_addr.port() + 1;
            self.found_count += 1;
        }

        maybe_found
    }
}

impl<const MIN: u16, const MAX: u16> Default for ScanningPortFinder<MIN, MAX> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests_find_port {
    use super::*;

    #[test]
    fn it_finds_a_random_port() {
        assert!(
            ScanningPortFinder::<8000, 9999>::new()
                .find_port()
                .is_some()
        );
    }
}

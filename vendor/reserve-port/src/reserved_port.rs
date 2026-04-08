use ::std::net::IpAddr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;

use crate::Error;
use crate::Result;
use crate::borrow_global_port_finder;

/// A port, that at the time of creation, is guaranteed to be free for use by the OS.
/// This also guarantees not to clash with _other_ `ReservedPort` objects.
///
/// The motivation of this library is to allow one to reserve many ports,
/// ensure they don't clash with each other,
/// and then let them go when they are no longer needed.
#[derive(Debug)]
pub struct ReservedPort {
    port: u16,
}

impl ReservedPort {
    pub(crate) fn new(port: u16) -> Self {
        Self { port }
    }

    pub fn random_with_tcp<I>(ip: I) -> Result<(Self, TcpListener)>
    where
        I: Into<IpAddr>,
    {
        let (tcp_listener, socket_addr) = Self::random_permanently_reserved_tcp(ip)?;
        let port = socket_addr.port();
        let reserved_port = ReservedPort::new(port);

        Ok((reserved_port, tcp_listener))
    }

    pub fn random_permanently_reserved_tcp<I>(ip: I) -> Result<(TcpListener, SocketAddr)>
    where
        I: Into<IpAddr>,
    {
        let mut port_finder = borrow_global_port_finder()?;

        port_finder
            .reserve_random_tcp(ip.into())
            .ok_or(Error::FailedToReservePort)
    }

    pub fn random() -> Result<Self> {
        Self::random_permanently_reserved().map(ReservedPort::new)
    }

    pub fn random_permanently_reserved() -> Result<u16> {
        let mut port_finder = borrow_global_port_finder()?;

        port_finder
            .reserve_random_port()
            .ok_or(Error::FailedToReservePort)
    }

    /// _Permanently_ reserves the given port as being offlimits (for this library).
    ///
    /// This is useful if you have connected to a socket yourself,
    /// and wish to avoid clashing with this library.
    pub fn reserve_port(port: u16) -> Result<()> {
        let mut port_finder = borrow_global_port_finder()?;

        port_finder.reserve_port(port);

        Ok(())
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ReservedPort {
    fn drop(&mut self) {
        let mut port_finder =
            borrow_global_port_finder().expect("Should be able to unlock global port finder");

        port_finder.free_port(self.port);
    }
}

#[cfg(test)]
mod test_reserve_port {
    use super::*;

    #[test]
    fn it_should_reserve_a_port_for_use() {
        const TEST_PORT_NUM: u16 = 1230;

        let reserved = ReservedPort::reserve_port(TEST_PORT_NUM);

        assert!(reserved.is_ok());
    }

    #[test]
    fn it_should_reserve_same_port_twice_in_a_row() {
        const TEST_PORT_NUM: u16 = 1231;

        let _ = ReservedPort::reserve_port(TEST_PORT_NUM);
        let reserved = ReservedPort::reserve_port(TEST_PORT_NUM);

        assert!(reserved.is_ok());
    }

    #[test]
    fn it_should_allow_reserving_random_ports_by_hand() {
        let reserved_1 = ReservedPort::random().unwrap();
        let reserved_2 = ReservedPort::reserve_port(reserved_1.port());

        assert!(reserved_2.is_ok());
    }

    #[test]
    fn it_should_allow_reserving_random_ports_by_hand_after_they_have_dropped() {
        let reserved_1 = ReservedPort::random().unwrap();
        let random_port = reserved_1.port();
        ::std::mem::drop(reserved_1);

        let result = ReservedPort::reserve_port(random_port);

        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod test_reserve_random_port {
    use super::*;

    #[test]
    fn it_should_reserve_a_random_port_for_use() {
        let reserved = ReservedPort::random();

        assert!(reserved.is_ok());
    }

    #[test]
    fn it_should_reserve_different_ports_over_use() {
        let reserved_1 = ReservedPort::random().unwrap();
        let reserved_2 = ReservedPort::random().unwrap();

        assert_ne!(reserved_1.port(), reserved_2.port());
    }
}

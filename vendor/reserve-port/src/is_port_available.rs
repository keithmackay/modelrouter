use ::std::net::Ipv4Addr;
use ::std::net::Ipv6Addr;
use ::std::net::SocketAddr;
use ::std::net::SocketAddrV4;
use ::std::net::SocketAddrV6;
use ::std::net::TcpListener;
use ::std::net::ToSocketAddrs;
use ::std::net::UdpSocket;

/// Check if a port is available on both TCP and UDP.
pub fn is_port_available(port: u16) -> bool {
    is_port_available_tcp(port) && is_port_available_udp(port)
}

/// Check if a port is available on TCP.
pub fn is_port_available_tcp(port: u16) -> bool {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0);

    bind_and_get_tcp_port(ipv6).is_some() && bind_and_get_tcp_port(ipv4).is_some()
}

/// Check if a port is available on UDP.
pub fn is_port_available_udp(port: u16) -> bool {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0);

    bind_and_get_udp_port(ipv6).is_some() && bind_and_get_udp_port(ipv4).is_some()
}

// Binds to a socket using TCP, and returns the Port in use.
pub(crate) fn bind_and_get_tcp<A: ToSocketAddrs>(addr: A) -> Option<(TcpListener, SocketAddr)> {
    let tcp_listener = TcpListener::bind(addr).ok()?;
    let socket_addr = tcp_listener.local_addr().ok()?;

    Some((tcp_listener, socket_addr))
}

// Binds to a socket using UDP, and returns the Port in use.
pub(crate) fn bind_and_get_udp_port<A: ToSocketAddrs>(addr: A) -> Option<u16> {
    Some(UdpSocket::bind(addr).ok()?.local_addr().ok()?.port())
}

// Binds to a socket using TCP, and returns the Port in use.
pub(crate) fn bind_and_get_tcp_port<A: ToSocketAddrs>(addr: A) -> Option<u16> {
    Some(TcpListener::bind(addr).ok()?.local_addr().ok()?.port())
}

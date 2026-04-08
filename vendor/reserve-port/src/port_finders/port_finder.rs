use ::std::net::IpAddr;
use ::std::net::Ipv4Addr;
use ::std::net::SocketAddr;
use ::std::net::TcpListener;

pub trait PortFinder {
    fn find_port(&mut self) -> Option<u16> {
        self.find_port_for_ip(Ipv4Addr::LOCALHOST.into())
            .map(|(_, socket_addr)| socket_addr.port())
    }

    fn find_port_for_ip(&mut self, ip: IpAddr) -> Option<(TcpListener, SocketAddr)>;
}

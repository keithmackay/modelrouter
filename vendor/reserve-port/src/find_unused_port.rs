use crate::port_finders::OsQueryPortFinder;
use crate::port_finders::PortFinder;

pub fn find_unused_port() -> Option<u16> {
    OsQueryPortFinder::new().find_port()
}

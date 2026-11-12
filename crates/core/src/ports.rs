use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

use anyhow::{Result, bail};

pub type PortMap = BTreeMap<String, u16>;

/// Slot 0 is the developer's main checkout, so workers start at slot 1.
pub fn ports_for_slot(base: &PortMap, slot: u16, stride: u16) -> PortMap {
    base.iter()
        .map(|(k, v)| (k.clone(), v + slot * stride))
        .collect()
}

/// Windows asks the user for firewall access when a new exe listens on a wildcard address, but
/// not when it only binds one, so the wildcard checks are exclusive binds that never listen.
#[cfg(windows)]
pub fn is_free(port: u16) -> bool {
    // Each step drops its socket before the next; a held 127.0.0.1 listener makes the exclusive
    // 0.0.0.0 bind fail, so every port would read busy.
    let steps: [&dyn Fn() -> bool; 4] = [
        &|| TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok(),
        &|| exclusive_bind((Ipv4Addr::UNSPECIFIED, port).into()).is_ok(),
        &|| ipv6_free(TcpListener::bind((Ipv6Addr::LOCALHOST, port))),
        &|| ipv6_free(exclusive_bind((Ipv6Addr::UNSPECIFIED, port).into())),
    ];
    steps.iter().all(|step| step())
}

#[cfg(unix)]
pub fn is_free(port: u16) -> bool {
    [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .iter()
        .all(|ip| TcpListener::bind((*ip, port)).is_ok())
        && ipv6_free(TcpListener::bind((Ipv6Addr::LOCALHOST, port)))
}

/// Other IPv6 bind errors count as free so machines without IPv6 don't read every port as busy.
fn ipv6_free<T>(probe: io::Result<T>) -> bool {
    !matches!(
        probe.map_err(|e| e.kind()),
        Err(io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied)
    )
}

#[cfg(windows)]
fn exclusive_bind(addr: std::net::SocketAddr) -> io::Result<socket2::Socket> {
    use socket2::{Domain, Socket, Type};
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOL_SOCKET, setsockopt};

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    if addr.is_ipv6() {
        // Windows defaults IPv6 sockets to V6ONLY, which wouldn't conflict with a dual-stack holder.
        socket.set_only_v6(false)?;
    }
    let on: i32 = 1;
    // SAFETY: the socket is open for the whole call, and optval points at an i32 of optlen bytes.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const i32).cast(),
            size_of::<i32>() as i32,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&addr.into())?;
    Ok(socket)
}

pub fn allocate(
    base: &PortMap,
    stride: u16,
    max_slots: u16,
    used: &[u16],
    probe: impl Fn(u16) -> bool,
) -> Result<(u16, PortMap)> {
    // Different services' base ports can meet after shifting (3000 + 10×100 = 4000), so a slot
    // must not reuse any port of slot 0 or another worker's slot.
    let taken: std::collections::HashSet<u16> = std::iter::once(0)
        .chain(used.iter().copied())
        .flat_map(|s| ports_for_slot(base, s, stride).into_values())
        .collect();
    for slot in 1..=max_slots {
        if used.contains(&slot) {
            continue;
        }
        let ports = ports_for_slot(base, slot, stride);
        if ports.values().all(|p| !taken.contains(p) && probe(*p)) {
            return Ok((slot, ports));
        }
    }
    bail!("no free port slot (max_slots = {max_slots}); clean up a worker or raise max_slots")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PortMap {
        [("api".to_string(), 4000), ("pg".to_string(), 5432)].into()
    }

    #[test]
    fn skips_used_and_busy_slots() {
        let (slot, ports) = allocate(&base(), 100, 5, &[1], |p| p != 4200).unwrap();
        assert_eq!(slot, 3);
        assert_eq!(ports["api"], 4300);
        assert_eq!(ports["pg"], 5732);
    }

    #[test]
    fn avoids_cross_service_collisions() {
        let base: PortMap = [("api".to_string(), 4000), ("grafana".to_string(), 3900)].into();
        let (slot, _) = allocate(&base, 100, 10, &[], |_| true).unwrap();
        assert_eq!(
            slot, 2,
            "slot 1 puts grafana on 4000, the main checkout's api port"
        );
        let (slot, _) = allocate(&base, 100, 10, &[2], |_| true).unwrap();
        assert_eq!(slot, 4, "slot 3 puts grafana on 4200, worker 2's api port");
    }

    #[test]
    fn exhausts() {
        assert!(allocate(&base(), 100, 2, &[1, 2], |_| true).is_err());
    }

    #[test]
    fn probe_detects_bound_port() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(!is_free(l.local_addr().unwrap().port()));
    }

    #[test]
    fn probe_detects_ipv6_loopback_listener() {
        let Ok(l) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            eprintln!("skipped: no IPv6 loopback");
            return;
        };
        assert!(!is_free(l.local_addr().unwrap().port()));
    }

    #[cfg(windows)]
    #[test]
    fn probe_detects_wildcard_bind_without_listen() {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket
            .bind(&std::net::SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())
            .unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        assert!(!is_free(port));
    }

    #[cfg(windows)]
    #[test]
    fn probe_detects_dual_stack_wildcard_bind() {
        use socket2::{Domain, Socket, Type};
        let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, None) else {
            eprintln!("skipped: no IPv6");
            return;
        };
        let bound = socket.set_only_v6(false).and_then(|()| {
            socket.bind(&std::net::SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into())
        });
        if bound.is_err() {
            eprintln!("skipped: no dual-stack IPv6");
            return;
        }
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        assert!(!is_free(port));
    }

    #[test]
    fn probe_reads_free_port_as_free() {
        // Parallel tests bind ephemeral ports too, so one dropped port may be taken again.
        let any_free = (0..5).any(|_| {
            let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            is_free(port)
        });
        assert!(any_free);
    }
}

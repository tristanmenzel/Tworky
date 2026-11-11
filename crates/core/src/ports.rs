use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};

use anyhow::{Result, bail};

pub type PortMap = BTreeMap<String, u16>;

/// Slot 0 is the developer's main checkout, so workers start at slot 1.
pub fn ports_for_slot(base: &PortMap, slot: u16, stride: u16) -> PortMap {
    base.iter()
        .map(|(k, v)| (k.clone(), v + slot * stride))
        .collect()
}

pub fn is_free(port: u16) -> bool {
    [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .iter()
        .all(|ip| TcpListener::bind((*ip, port)).is_ok())
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
}

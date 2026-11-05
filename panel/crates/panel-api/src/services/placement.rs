//! Placing a new service on a server: scontrol's own placement instead of a
//! Swarm scheduler. A service stays on its server for its whole life; the
//! agent there owns its files, containers, and Nginx vhost.

use crate::db::servers::Server;

/// Why a server cannot take a service, or `None` when it can.
pub fn ineligible_reason(server: &Server, needs_docker: bool) -> Option<String> {
    if server.status != "online" {
        return Some(format!("server {} is {}", server.name, server.status));
    }
    if server.draining {
        return Some(format!("server {} is draining", server.name));
    }
    if needs_docker && !server.docker {
        return Some(format!("server {} has no Docker", server.name));
    }
    if let Some(max) = server.max_services {
        if server.service_count >= i64::from(max) {
            return Some(format!("server {} is full ({max} services)", server.name));
        }
    }
    None
}

/// Picks the eligible server with the most free capacity: the lowest
/// fraction of `max_services` in use (an unbounded server counts by service
/// count against a nominal 100), ties broken by fewer services and then name,
/// so placement is deterministic.
pub fn choose(servers: &[Server], needs_docker: bool) -> Option<&Server> {
    servers
        .iter()
        .filter(|s| ineligible_reason(s, needs_docker).is_none())
        .min_by(|a, b| {
            load(a)
                .total_cmp(&load(b))
                .then(a.service_count.cmp(&b.service_count))
                .then(a.name.cmp(&b.name))
        })
}

fn load(server: &Server) -> f64 {
    let capacity = server.max_services.map_or(100.0, |m| f64::from(m.max(1)));
    server.service_count as f64 / capacity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::servers::Transport;
    use uuid::Uuid;

    fn server(name: &str, services: i64, max: Option<i32>) -> Server {
        Server {
            id: Uuid::new_v4(),
            name: name.into(),
            transport: Transport::Mtls,
            address: format!("{name}:7843"),
            cert_fingerprint: None,
            cert_serial: None,
            cert_expires_at: None,
            previous_fingerprint: None,
            previous_valid_until: None,
            rotation_pending: false,
            draining: false,
            max_services: max,
            status: "online".into(),
            docker: true,
            agent_version: None,
            last_seen_at: None,
            last_error: None,
            service_count: services,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn least_loaded_eligible_server_wins() {
        let servers = vec![
            server("a", 8, Some(10)),
            server("b", 3, Some(10)),
            server("c", 1, Some(2)),
        ];
        assert_eq!(choose(&servers, true).unwrap().name, "b");
    }

    #[test]
    fn offline_draining_full_and_dockerless_servers_are_skipped() {
        let mut offline = server("offline", 0, None);
        offline.status = "offline".into();
        let mut draining = server("draining", 0, None);
        draining.draining = true;
        let full = server("full", 4, Some(4));
        let mut no_docker = server("no-docker", 0, None);
        no_docker.docker = false;
        let servers = vec![offline, draining, full, no_docker.clone()];
        assert!(choose(&servers, true).is_none());
        assert_eq!(choose(&servers, false).unwrap().name, "no-docker");
        assert!(ineligible_reason(&servers[2], true)
            .unwrap()
            .contains("full"));
    }

    #[test]
    fn ties_are_deterministic() {
        let servers = vec![server("b", 0, None), server("a", 0, None)];
        assert_eq!(choose(&servers, true).unwrap().name, "a");
    }
}

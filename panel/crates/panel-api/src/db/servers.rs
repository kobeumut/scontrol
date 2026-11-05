//! Servers (agents) and the panel PKI material. Runtime `sqlx::query*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The panel's own host: always present, reached over the local socket.
pub const LOCAL_SERVER_ID: Uuid = Uuid::nil();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Local,
    Mtls,
    SshSocket,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Local => "local",
            Transport::Mtls => "mtls",
            Transport::SshSocket => "ssh_socket",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "mtls" => Transport::Mtls,
            "ssh_socket" => Transport::SshSocket,
            _ => Transport::Local,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Server {
    pub id: Uuid,
    pub name: String,
    pub transport: Transport,
    pub address: String,
    pub cert_fingerprint: Option<String>,
    pub cert_serial: Option<String>,
    pub cert_expires_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub previous_fingerprint: Option<String>,
    #[serde(skip)]
    pub previous_valid_until: Option<DateTime<Utc>>,
    pub rotation_pending: bool,
    pub draining: bool,
    pub max_services: Option<i32>,
    pub status: String,
    pub docker: bool,
    pub agent_version: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub service_count: i64,
    pub created_at: DateTime<Utc>,
}

impl Server {
    /// Fingerprints an agent may present: the current certificate and, until
    /// the rotation window closes, the previous one.
    pub fn accepted_fingerprints(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut pins: Vec<String> = self.cert_fingerprint.iter().cloned().collect();
        if let (Some(previous), Some(until)) =
            (&self.previous_fingerprint, self.previous_valid_until)
        {
            if until > now {
                pins.push(previous.clone());
            }
        }
        pins
    }

    /// The host part of `address` (`host:port`, IPv6 in brackets).
    pub fn host(&self) -> &str {
        split_host_port(&self.address).map_or("", |(host, _)| host)
    }
}

/// Splits `host:port`; IPv6 hosts are written `[addr]:port`.
pub fn split_host_port(address: &str) -> Option<(&str, u16)> {
    let (host, port) = address.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    let host = match host.strip_prefix('[') {
        Some(bracketed) => {
            let inner = bracketed.strip_suffix(']')?;
            inner.parse::<std::net::Ipv6Addr>().ok()?;
            inner
        }
        None if host.contains(':') => return None,
        None => host,
    };
    (!host.is_empty()).then_some((host, port))
}

#[derive(sqlx::FromRow)]
struct ServerRecord {
    id: Uuid,
    name: String,
    transport: String,
    address: String,
    cert_fingerprint: Option<String>,
    cert_serial: Option<String>,
    cert_expires_at: Option<DateTime<Utc>>,
    previous_fingerprint: Option<String>,
    previous_valid_until: Option<DateTime<Utc>>,
    draining: bool,
    max_services: Option<i32>,
    status: String,
    docker: bool,
    agent_version: Option<String>,
    last_seen_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    service_count: i64,
    created_at: DateTime<Utc>,
}

impl From<ServerRecord> for Server {
    fn from(r: ServerRecord) -> Self {
        let rotation_pending = r
            .previous_valid_until
            .is_some_and(|until| until > Utc::now());
        Server {
            id: r.id,
            name: r.name,
            transport: Transport::parse(&r.transport),
            address: r.address,
            cert_fingerprint: r.cert_fingerprint,
            cert_serial: r.cert_serial,
            cert_expires_at: r.cert_expires_at,
            previous_fingerprint: r.previous_fingerprint,
            previous_valid_until: r.previous_valid_until,
            rotation_pending,
            draining: r.draining,
            max_services: r.max_services,
            status: r.status,
            docker: r.docker,
            agent_version: r.agent_version,
            last_seen_at: r.last_seen_at,
            last_error: r.last_error,
            service_count: r.service_count,
            created_at: r.created_at,
        }
    }
}

const SELECT: &str = "SELECT s.id, s.name, s.transport, s.address, s.cert_fingerprint,
        s.cert_serial, s.cert_expires_at, s.previous_fingerprint, s.previous_valid_until,
        s.draining, s.max_services, s.status, s.docker, s.agent_version, s.last_seen_at,
        s.last_error,
        (SELECT COUNT(*) FROM compose_projects c WHERE c.server_id = s.id) AS service_count,
        s.created_at
     FROM servers s";

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<Server>> {
    let records: Vec<ServerRecord> = sqlx::query_as(&format!(
        "{SELECT} ORDER BY (s.id <> '00000000-0000-0000-0000-000000000000'), s.name"
    ))
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(Server::from).collect())
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Server>> {
    let record: Option<ServerRecord> = sqlx::query_as(&format!("{SELECT} WHERE s.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(record.map(Server::from))
}

pub struct NewServer<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub transport: Transport,
    pub address: &'a str,
    pub max_services: Option<i32>,
}

pub async fn insert(pool: &PgPool, server: NewServer<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO servers (id, name, transport, address, max_services)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(server.id)
    .bind(server.name)
    .bind(server.transport.as_str())
    .bind(server.address)
    .bind(server.max_services)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a newly signed agent certificate. The one it replaces stays
/// accepted for `grace` (a rotation the operator has not installed yet).
pub async fn set_certificate(
    pool: &PgPool,
    id: Uuid,
    cert_pem: &str,
    info: &panel_core::pki::IssuedInfo,
    grace: chrono::Duration,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE servers SET
             previous_fingerprint = cert_fingerprint,
             previous_valid_until = CASE WHEN cert_fingerprint IS NULL THEN NULL
                                         ELSE now() + $6 END,
             cert_pem = $2, cert_fingerprint = $3, cert_serial = $4, cert_expires_at = $5,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(cert_pem)
    .bind(&info.fingerprint)
    .bind(&info.serial_hex)
    .bind(info.not_after)
    .bind(grace)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn certificate_pem(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>("SELECT cert_pem FROM servers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(Option::flatten)
}

/// Ends a rotation once the agent presented its new certificate.
pub async fn finish_rotation(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE servers SET previous_fingerprint = NULL, previous_valid_until = NULL
         WHERE id = $1 AND previous_fingerprint IS NOT NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_settings(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    draining: bool,
    max_services: Option<i32>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE servers SET name = $2, draining = $3, max_services = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(draining)
    .bind(max_services)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_check(
    pool: &PgPool,
    id: Uuid,
    result: Result<(bool, &str), &str>,
) -> sqlx::Result<()> {
    match result {
        Ok((docker, version)) => {
            sqlx::query(
                "UPDATE servers SET status = 'online', docker = $2, agent_version = $3,
                     last_seen_at = now(), last_error = NULL
                 WHERE id = $1",
            )
            .bind(id)
            .bind(docker)
            .bind(version)
            .execute(pool)
            .await?;
        }
        Err(error) => {
            sqlx::query("UPDATE servers SET status = 'offline', last_error = $2 WHERE id = $1")
                .bind(id)
                .bind(error)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM servers WHERE id = $1 AND id <> $2")
        .bind(id)
        .bind(LOCAL_SERVER_ID)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn server_of_service(pool: &PgPool, service_id: Uuid) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT server_id FROM compose_projects WHERE id = $1")
        .bind(service_id)
        .fetch_optional(pool)
        .await
}

pub async fn place_service(pool: &PgPool, service_id: Uuid, server_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE compose_projects SET server_id = $2 WHERE id = $1")
        .bind(service_id)
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// One sealed PKI item (`ca` or `panel_client`).
pub struct PkiRow {
    pub cert_pem: String,
    pub key_sealed: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}

pub async fn pki_material(pool: &PgPool, name: &str) -> sqlx::Result<Option<PkiRow>> {
    let row: Option<(String, Vec<u8>, DateTime<Utc>)> =
        sqlx::query_as("SELECT cert_pem, key_sealed, expires_at FROM pki_material WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(cert_pem, key_sealed, expires_at)| PkiRow {
        cert_pem,
        key_sealed,
        expires_at,
    }))
}

/// Stores `row` under `name`. With `replace = false` an existing row wins
/// (two processes creating the CA at once keep the first) and is returned.
pub async fn store_pki_material(
    pool: &PgPool,
    name: &str,
    row: &PkiRow,
    replace: bool,
) -> sqlx::Result<PkiRow> {
    let conflict = if replace {
        "DO UPDATE SET cert_pem = EXCLUDED.cert_pem, key_sealed = EXCLUDED.key_sealed,
             expires_at = EXCLUDED.expires_at, created_at = now()"
    } else {
        "DO NOTHING"
    };
    sqlx::query(&format!(
        "INSERT INTO pki_material (name, cert_pem, key_sealed, expires_at)
         VALUES ($1, $2, $3, $4) ON CONFLICT (name) {conflict}"
    ))
    .bind(name)
    .bind(&row.cert_pem)
    .bind(&row.key_sealed)
    .bind(row.expires_at)
    .execute(pool)
    .await?;
    pki_material(pool, name)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

#[cfg(test)]
mod tests {
    use super::split_host_port;

    #[test]
    fn host_port_parsing() {
        assert_eq!(
            split_host_port("agent.example.com:7843"),
            Some(("agent.example.com", 7843))
        );
        assert_eq!(split_host_port("10.0.0.2:7843"), Some(("10.0.0.2", 7843)));
        assert_eq!(
            split_host_port("[2001:db8::1]:7843"),
            Some(("2001:db8::1", 7843))
        );
        assert_eq!(split_host_port("2001:db8::1:7843"), None);
        assert_eq!(split_host_port("agent.example.com"), None);
        assert_eq!(split_host_port("agent.example.com:0"), None);
        assert_eq!(split_host_port(":7843"), None);
    }
}

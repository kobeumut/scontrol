//! Managed database services (PostgreSQL, MySQL, MariaDB, MongoDB, Redis).
//!
//! A database service is a Compose project whose source is
//! [`crate::types::ComposeSource::Database`]: the agent renders it into a
//! single-service Compose file from the vetted per-engine template below, so
//! it shares the deploy queue, logs, and lifecycle of every other service.
//!
//! Rules every template follows:
//! - the container port is published on `127.0.0.1` only, never `0.0.0.0`;
//!   outside access goes through an explicit [`DatabaseExposure`] (a host
//!   Nginx `stream` proxy plus a UFW rule), so UFW actually filters it;
//! - data lives in a Docker named volume; there are no host bind mounts;
//! - credentials reach the container through the agent's 0600 `db.env` file
//!   (and, for Redis, a 0400 config file), never through argv;
//! - health checks are exec form and carry no password.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;

/// Compose service name of every database service.
pub const DATABASE_SERVICE_NAME: &str = "db";
/// Named volume holding the database files.
pub const DATABASE_VOLUME_NAME: &str = "data";
/// Loopback host ports the panel allocates for database services. Disjoint
/// from domain bindings (40000–49999) and proxied site apps (30000–39999).
pub const DATABASE_LOOPBACK_PORTS: std::ops::RangeInclusive<u16> = 25000..=29999;
/// Most CIDR entries one exposure may allow.
pub const MAX_EXPOSURE_SOURCES: usize = 16;
/// Length of generated passwords (alphanumeric, ~190 bits).
pub const GENERATED_PASSWORD_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManagedDatabaseEngine {
    Postgres,
    Mysql,
    Mariadb,
    Mongo,
    Redis,
}

impl fmt::Display for ManagedDatabaseEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ManagedDatabaseEngine {
    pub const ALL: [ManagedDatabaseEngine; 5] = [
        ManagedDatabaseEngine::Postgres,
        ManagedDatabaseEngine::Mysql,
        ManagedDatabaseEngine::Mariadb,
        ManagedDatabaseEngine::Mongo,
        ManagedDatabaseEngine::Redis,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Mongo => "mongo",
            Self::Redis => "redis",
        }
    }

    /// Supported image tags, newest first. The first one is the default.
    pub fn versions(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &["18", "17", "16", "15", "14"],
            Self::Mysql => &["8.4", "8.0"],
            Self::Mariadb => &["11.8", "11.4", "10.11", "10.6"],
            // 8.0 builds before 8.0.17 refuse to start on Linux 6.19+ kernels
            // (SERVER-121912); 8.2 is the default.
            Self::Mongo => &["8.2", "8.0", "7.0"],
            Self::Redis => &["8.2", "7.4"],
        }
    }

    pub fn default_version(self) -> &'static str {
        self.versions()[0]
    }

    /// Official Docker Hub image for a supported version.
    pub fn image(self, version: &str) -> String {
        format!("{}:{version}", self.as_str())
    }

    /// Port the server listens on inside the container.
    pub fn container_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Mysql | Self::Mariadb => 3306,
            Self::Mongo => 27017,
            Self::Redis => 6379,
        }
    }

    /// Where the named volume is mounted. PostgreSQL 18 images moved the
    /// volume to `/var/lib/postgresql` (with a versioned `PGDATA` below it)
    /// and refuse to start with a mount at the old `.../data` path.
    pub fn data_path(self, version: &str) -> &'static str {
        match self {
            Self::Postgres => {
                if version.parse::<u32>().is_ok_and(|major| major >= 18) {
                    "/var/lib/postgresql"
                } else {
                    "/var/lib/postgresql/data"
                }
            }
            Self::Mysql | Self::Mariadb => "/var/lib/mysql",
            Self::Mongo => "/data/db",
            Self::Redis => "/data",
        }
    }

    /// Redis has a single server password and no named database or user.
    pub fn has_database_and_user(self) -> bool {
        !matches!(self, Self::Redis)
    }

    /// MySQL and MariaDB keep a separate `root` account.
    pub fn has_root_password(self) -> bool {
        matches!(self, Self::Mysql | Self::Mariadb)
    }
}

fn default_database_name() -> String {
    "app".into()
}

fn default_database_user() -> String {
    "app".into()
}

/// Non-secret settings of a database service; stored in the service's
/// source. The password is not part of it (see [`DatabaseCredentials`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseServiceSpec {
    pub engine: ManagedDatabaseEngine,
    /// One of [`ManagedDatabaseEngine::versions`].
    pub version: String,
    /// Database created on first start (ignored for Redis).
    #[serde(default = "default_database_name")]
    pub database_name: String,
    /// Application user created on first start (Redis uses `default`).
    #[serde(default = "default_database_user")]
    pub username: String,
    /// Memory limit in MiB.
    #[serde(default)]
    pub memory_limit_mb: Option<u32>,
    /// CPU limit in thousandths of a CPU (`500` = half a CPU).
    #[serde(default)]
    pub cpu_limit_millis: Option<u32>,
}

impl DatabaseServiceSpec {
    /// Shared validation for panel-api and the agent. Returns a message
    /// that is safe to show to the user.
    pub fn validate(&self) -> Result<(), String> {
        if !self.engine.versions().contains(&self.version.as_str()) {
            return Err(format!(
                "unsupported {} version; choose one of {}",
                self.engine,
                self.engine.versions().join(", ")
            ));
        }
        if self.engine.has_database_and_user() {
            if !is_valid_database_identifier(&self.database_name) {
                return Err(
                    "database name must be 1..=63 letters, digits, or underscores, starting with a letter"
                        .into(),
                );
            }
            if !is_valid_database_identifier(&self.username) {
                return Err(
                    "username must be 1..=63 letters, digits, or underscores, starting with a letter"
                        .into(),
                );
            }
            // The MySQL and MariaDB images refuse MYSQL_USER=root; the root
            // account is managed separately.
            if self.engine.has_root_password() && self.username.eq_ignore_ascii_case("root") {
                return Err("username cannot be root; the root account is generated".into());
            }
        }
        if self
            .memory_limit_mb
            .is_some_and(|mb| !(64..=1024 * 1024).contains(&mb))
        {
            return Err("memory limit must be 64 MiB..=1 TiB".into());
        }
        if self
            .cpu_limit_millis
            .is_some_and(|m| !(10..=512_000).contains(&m))
        {
            return Err("CPU limit must be 10..=512000 millicpus".into());
        }
        Ok(())
    }

    /// The user clients log in as.
    pub fn login_user(&self) -> &str {
        if self.engine.has_database_and_user() {
            &self.username
        } else {
            "default"
        }
    }

    /// Variables the container needs on first start. Written by the agent to
    /// its 0600 `db.env` file and passed as the last `env_file`, so they win
    /// over user variables with the same key.
    pub fn container_environment(
        &self,
        credentials: &DatabaseCredentials,
    ) -> Vec<(String, String)> {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let password = credentials.password.as_str();
        let root = credentials.root_password.as_deref().unwrap_or(password);
        match self.engine {
            ManagedDatabaseEngine::Postgres => vec![
                pair("POSTGRES_USER", &self.username),
                pair("POSTGRES_PASSWORD", password),
                pair("POSTGRES_DB", &self.database_name),
            ],
            ManagedDatabaseEngine::Mysql => vec![
                pair("MYSQL_DATABASE", &self.database_name),
                pair("MYSQL_USER", &self.username),
                pair("MYSQL_PASSWORD", password),
                pair("MYSQL_ROOT_PASSWORD", root),
            ],
            ManagedDatabaseEngine::Mariadb => vec![
                pair("MARIADB_DATABASE", &self.database_name),
                pair("MARIADB_USER", &self.username),
                pair("MARIADB_PASSWORD", password),
                pair("MARIADB_ROOT_PASSWORD", root),
            ],
            ManagedDatabaseEngine::Mongo => vec![
                pair("MONGO_INITDB_ROOT_USERNAME", &self.username),
                pair("MONGO_INITDB_ROOT_PASSWORD", password),
                pair("MONGO_INITDB_DATABASE", &self.database_name),
            ],
            // `redis-cli` (the health check) reads its password from here.
            ManagedDatabaseEngine::Redis => vec![pair("REDISCLI_AUTH", password)],
        }
    }

    /// Exec-form health check (without the leading `CMD`). None of them
    /// needs the password on argv: `pg_isready` and `mysqladmin ping` only
    /// probe the server, MariaDB's `healthcheck.sh` uses the image's own
    /// health check account, MongoDB answers `ping` without auth, and
    /// `redis-cli` reads `REDISCLI_AUTH`.
    pub fn health_check(&self) -> Vec<String> {
        let args: Vec<&str> = match self.engine {
            ManagedDatabaseEngine::Postgres => vec![
                "pg_isready",
                "-U",
                &self.username,
                "-d",
                &self.database_name,
            ],
            ManagedDatabaseEngine::Mysql => {
                vec!["mysqladmin", "ping", "-h", "127.0.0.1", "--silent"]
            }
            ManagedDatabaseEngine::Mariadb => {
                vec!["healthcheck.sh", "--connect", "--innodb_initialized"]
            }
            ManagedDatabaseEngine::Mongo => vec![
                "mongosh",
                "--quiet",
                "--norc",
                "--eval",
                "db.adminCommand('ping').ok",
            ],
            ManagedDatabaseEngine::Redis => vec!["redis-cli", "-h", "127.0.0.1", "ping"],
        };
        args.into_iter().map(str::to_string).collect()
    }

    /// Redis configuration (holds the password). Mounted read-only into the
    /// container as a Compose secret and passed to `redis-server` by path.
    pub fn redis_config(credentials: &DatabaseCredentials) -> String {
        format!("requirepass {}\nappendonly yes\n", credentials.password)
    }

    /// Connection URI for `host:port`. Generated passwords are alphanumeric,
    /// so nothing needs escaping; user and database names are identifiers.
    pub fn connection_uri(
        &self,
        credentials: &DatabaseCredentials,
        host: &str,
        port: u16,
    ) -> String {
        let user = self.login_user();
        let password = &credentials.password;
        let db = &self.database_name;
        match self.engine {
            ManagedDatabaseEngine::Postgres => {
                format!("postgresql://{user}:{password}@{host}:{port}/{db}")
            }
            ManagedDatabaseEngine::Mysql | ManagedDatabaseEngine::Mariadb => {
                format!("mysql://{user}:{password}@{host}:{port}/{db}")
            }
            ManagedDatabaseEngine::Mongo => {
                format!("mongodb://{user}:{password}@{host}:{port}/{db}?authSource=admin")
            }
            ManagedDatabaseEngine::Redis => format!("redis://{user}:{password}@{host}:{port}/0"),
        }
    }
}

/// Identifiers for database and user names: safe unquoted in every engine.
pub fn is_valid_database_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Generated secrets are alphanumeric only, which keeps them literal in an
/// env file, a Redis config line, and a URI.
pub fn is_valid_generated_secret(value: &str) -> bool {
    (16..=128).contains(&value.len()) && value.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Generated credentials of a database service. panel-api keeps them sealed
/// with `SecretBox`; the queue worker sends them with each deployment of the
/// service (never in a queue row), and the agent writes them to 0600 files
/// and masks them in logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseCredentials {
    pub password: String,
    /// MySQL and MariaDB only.
    #[serde(default)]
    pub root_password: Option<String>,
}

impl fmt::Debug for DatabaseCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseCredentials")
            .field("password", &"********")
            .field(
                "root_password",
                &self.root_password.as_ref().map(|_| "********"),
            )
            .finish()
    }
}

impl DatabaseCredentials {
    pub fn validate(&self, engine: ManagedDatabaseEngine) -> Result<(), String> {
        if !is_valid_generated_secret(&self.password) {
            return Err("invalid database password".into());
        }
        match (&self.root_password, engine.has_root_password()) {
            (Some(root), true) if is_valid_generated_secret(root) => Ok(()),
            (None, false) => Ok(()),
            _ => Err("invalid database root password".into()),
        }
    }

    /// Every secret value, for log masking.
    pub fn secrets(&self) -> Vec<String> {
        let mut all = vec![self.password.clone()];
        all.extend(self.root_password.clone());
        all
    }
}

/// What the agent needs, besides the spec, to deploy a database service.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseDeployment {
    /// Published on `127.0.0.1` only; in [`DATABASE_LOOPBACK_PORTS`].
    pub loopback_port: u16,
    pub credentials: DatabaseCredentials,
}

/// Outside access to a database service: the host Nginx listens on
/// `public_port` (a `stream` proxy to the loopback port) and UFW allows the
/// port, only from `allow_from` when it is not empty. Docker never publishes
/// the database on a public interface, because Docker-published ports bypass
/// UFW.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseExposure {
    pub public_port: u16,
    /// Source addresses or CIDR ranges (`203.0.113.4`, `10.0.0.0/8`,
    /// `2001:db8::/32`). Empty means any address.
    #[serde(default)]
    pub allow_from: Vec<String>,
}

/// Public ports a database may not take: privileged ports, the panel's own
/// HTTP(S) and API ports, and the loopback ranges the panel allocates.
pub fn is_allowed_public_port(port: u16) -> bool {
    port >= 1024
        && port != 8080
        && !DATABASE_LOOPBACK_PORTS.contains(&port)
        && !(30000..=49999).contains(&port)
}

/// Canonical form of an allowed source (`addr` or `addr/prefix`), or `None`
/// when it is not one.
pub fn normalize_source(source: &str) -> Option<String> {
    let (addr, prefix) = match source.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (source, None),
    };
    let addr: IpAddr = addr.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    match prefix {
        None => Some(addr.to_string()),
        Some(p) if !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()) => {
            let p: u8 = p.parse().ok()?;
            (p <= max).then(|| format!("{addr}/{p}"))
        }
        Some(_) => None,
    }
}

impl DatabaseExposure {
    /// Validates and canonicalizes the sources in place.
    pub fn normalize(&mut self) -> Result<(), String> {
        if !is_allowed_public_port(self.public_port) {
            return Err(
                "public port must be 1024..=65535 and outside the panel's reserved ranges (8080, 25000–49999)"
                    .into(),
            );
        }
        if self.allow_from.len() > MAX_EXPOSURE_SOURCES {
            return Err(format!("at most {MAX_EXPOSURE_SOURCES} allowed sources"));
        }
        let mut normalized = Vec::with_capacity(self.allow_from.len());
        for source in &self.allow_from {
            let value = normalize_source(source.trim())
                .ok_or_else(|| format!("invalid source address or CIDR: {source}"))?;
            if !normalized.contains(&value) {
                normalized.push(value);
            }
        }
        self.allow_from = normalized;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(engine: ManagedDatabaseEngine) -> DatabaseServiceSpec {
        DatabaseServiceSpec {
            engine,
            version: engine.default_version().into(),
            database_name: "shop".into(),
            username: "shop_user".into(),
            memory_limit_mb: None,
            cpu_limit_millis: None,
        }
    }

    fn credentials(engine: ManagedDatabaseEngine) -> DatabaseCredentials {
        DatabaseCredentials {
            password: "Pw0123456789abcdefABCDEF".into(),
            root_password: engine
                .has_root_password()
                .then(|| "Root0123456789abcdefABCD".into()),
        }
    }

    #[test]
    fn every_engine_has_a_valid_default_template() {
        for engine in ManagedDatabaseEngine::ALL {
            let spec = spec(engine);
            assert!(spec.validate().is_ok(), "{engine}");
            assert!(credentials(engine).validate(engine).is_ok(), "{engine}");
            assert!(spec.engine.data_path(&spec.version).starts_with('/'));
            assert!(spec.engine.container_port() > 0);
            assert!(crate::types::validate::is_valid_image_reference(
                &engine.image(&spec.version)
            ));
            for version in engine.versions() {
                assert!(crate::types::validate::is_valid_image_reference(
                    &engine.image(version)
                ));
            }
        }
    }

    #[test]
    fn hostile_or_unsupported_specs_are_rejected() {
        let mut s = spec(ManagedDatabaseEngine::Postgres);
        s.version = "latest".into();
        assert!(s.validate().is_err());
        s.version = "17; rm -rf /".into();
        assert!(s.validate().is_err());

        let mut s = spec(ManagedDatabaseEngine::Postgres);
        s.database_name = "app\"; DROP".into();
        assert!(s.validate().is_err());
        s.database_name = "-c".into();
        assert!(s.validate().is_err());

        let mut s = spec(ManagedDatabaseEngine::Mysql);
        s.username = "ROOT".into();
        assert!(s.validate().is_err());

        let mut s = spec(ManagedDatabaseEngine::Redis);
        s.username = "ignored for redis!".into();
        assert!(s.validate().is_ok());
        s.memory_limit_mb = Some(1);
        assert!(s.validate().is_err());
    }

    #[test]
    fn postgres_18_mounts_the_new_volume_path() {
        let e = ManagedDatabaseEngine::Postgres;
        assert_eq!(e.data_path("18"), "/var/lib/postgresql");
        assert_eq!(e.data_path("17"), "/var/lib/postgresql/data");
    }

    #[test]
    fn credentials_never_appear_in_debug_output_or_health_checks() {
        for engine in ManagedDatabaseEngine::ALL {
            let creds = credentials(engine);
            let debug = format!("{creds:?}");
            for secret in creds.secrets() {
                assert!(!debug.contains(&secret));
                assert!(!spec(engine)
                    .health_check()
                    .iter()
                    .any(|a| a.contains(&secret)));
            }
        }
    }

    #[test]
    fn credentials_must_match_the_engine() {
        let pg = ManagedDatabaseEngine::Postgres;
        let my = ManagedDatabaseEngine::Mysql;
        assert!(credentials(my).validate(pg).is_err());
        assert!(credentials(pg).validate(my).is_err());
        let short = DatabaseCredentials {
            password: "short".into(),
            root_password: None,
        };
        assert!(short.validate(pg).is_err());
        let newline = DatabaseCredentials {
            password: "abcdefghijklmnop\nrequirepass x".into(),
            root_password: None,
        };
        assert!(newline.validate(ManagedDatabaseEngine::Redis).is_err());
    }

    #[test]
    fn container_environment_carries_the_generated_secrets() {
        let s = spec(ManagedDatabaseEngine::Mysql);
        let c = credentials(ManagedDatabaseEngine::Mysql);
        let env = s.container_environment(&c);
        assert!(env.contains(&("MYSQL_PASSWORD".into(), c.password.clone())));
        assert!(env.contains(&(
            "MYSQL_ROOT_PASSWORD".into(),
            c.root_password.clone().unwrap()
        )));
        assert!(env.contains(&("MYSQL_USER".into(), "shop_user".into())));
        let redis = spec(ManagedDatabaseEngine::Redis);
        let rc = credentials(ManagedDatabaseEngine::Redis);
        assert_eq!(
            redis.container_environment(&rc),
            vec![("REDISCLI_AUTH".to_string(), rc.password.clone())]
        );
        assert!(DatabaseServiceSpec::redis_config(&rc).starts_with("requirepass "));
    }

    #[test]
    fn connection_uris_per_engine() {
        let pg = spec(ManagedDatabaseEngine::Postgres);
        let c = credentials(ManagedDatabaseEngine::Postgres);
        assert_eq!(
            pg.connection_uri(&c, "127.0.0.1", 25000),
            format!("postgresql://shop_user:{}@127.0.0.1:25000/shop", c.password)
        );
        let mongo = spec(ManagedDatabaseEngine::Mongo);
        assert!(mongo
            .connection_uri(&c, "h", 1)
            .ends_with("/shop?authSource=admin"));
        let redis = spec(ManagedDatabaseEngine::Redis);
        assert!(redis
            .connection_uri(&c, "h", 1)
            .starts_with("redis://default:"));
    }

    #[test]
    fn exposure_ports_and_sources_are_validated() {
        let mut ok = DatabaseExposure {
            public_port: 15432,
            allow_from: vec![
                " 203.0.113.4 ".into(),
                "10.0.0.0/8".into(),
                "2001:db8::/32".into(),
                "10.0.0.0/8".into(),
            ],
        };
        ok.normalize().unwrap();
        assert_eq!(
            ok.allow_from,
            vec!["203.0.113.4", "10.0.0.0/8", "2001:db8::/32"]
        );
        for port in [22, 80, 443, 8080, 25000, 29999, 40000, 45000] {
            let mut bad = DatabaseExposure {
                public_port: port,
                allow_from: vec![],
            };
            assert!(bad.normalize().is_err(), "{port}");
        }
        for source in [
            "any",
            "10.0.0.0/33",
            "10.0.0.0/",
            "1.2.3.4/8/8",
            "::/129",
            "1.2.3.4 ; deny",
            "-1.2.3.4",
        ] {
            let mut bad = DatabaseExposure {
                public_port: 15432,
                allow_from: vec![source.into()],
            };
            assert!(bad.normalize().is_err(), "{source}");
        }
    }
}

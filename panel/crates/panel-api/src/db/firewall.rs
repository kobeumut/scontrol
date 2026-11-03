use panel_core::types::{FirewallProtocol, FirewallRule};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
struct FirewallRecord {
    id: Uuid,
    port: i32,
    protocol: String,
    allow: bool,
    description: String,
}

impl FirewallRecord {
    fn into_rule(self) -> anyhow::Result<FirewallRule> {
        Ok(FirewallRule {
            id: self.id,
            port: self.port as u16,
            protocol: FirewallProtocol::from_str(&self.protocol).map_err(anyhow::Error::msg)?,
            description: self.description,
            allow: self.allow,
        })
    }
}

pub async fn upsert(
    pool: &PgPool,
    port: u16,
    protocol: FirewallProtocol,
    allow: bool,
    description: &str,
) -> anyhow::Result<FirewallRule> {
    let protocol_str = protocol.to_string();
    let record = sqlx::query_as!(
        FirewallRecord,
        r#"
        INSERT INTO firewall_rules (port, protocol, allow, description)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (port, protocol) DO UPDATE SET allow = EXCLUDED.allow, description = EXCLUDED.description
        RETURNING id, port, protocol, allow, description
        "#,
        port as i32,
        protocol_str,
        allow,
        description,
    )
    .fetch_one(pool)
    .await?;
    record.into_rule()
}

pub async fn list_all(pool: &PgPool) -> anyhow::Result<Vec<FirewallRule>> {
    let records = sqlx::query_as!(
        FirewallRecord,
        "SELECT id, port, protocol, allow, description FROM firewall_rules ORDER BY port",
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(|r| r.into_rule()).collect()
}

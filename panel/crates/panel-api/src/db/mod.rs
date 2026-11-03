pub mod audit;
pub mod compose_projects;
pub mod cron;
pub mod databases;
pub mod firewall;
pub mod notifications;
pub mod projects;
pub mod registries;
pub mod sites;
pub mod users;

pub use sqlx::PgPool;

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

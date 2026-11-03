#![forbid(unsafe_code)]

//! panel-cli: user-facing administration CLI. Talks directly to Postgres
//! (same schema as `panel-api`) and to `panel-agent` over its Unix socket,
//! via the shared `panel_core::agent_client::AgentClient`. This lets the CLI
//! work during install (before the HTTP API is even running) and for local
//! server administration without needing a browser session.

mod db;

use clap::{Parser, Subcommand};
use comfy_table::Table;
use panel_core::adapters::{AdapterPort, CreateSiteParams, DeleteSiteParams};
use panel_core::agent_client::AgentClient;
use panel_core::types::Runtime;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "panel-cli", about = "scontrol administration CLI")]
struct Cli {
    /// Path to the panel-agent Unix socket.
    #[arg(long, default_value = "/run/scontrol/agent.sock", global = true)]
    agent_socket: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// User management.
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Site management.
    Site {
        #[command(subcommand)]
        action: SiteAction,
    },
    /// Query agent capabilities (docker/mail/antivirus enabled?).
    Capabilities,
}

#[derive(Subcommand)]
enum UserAction {
    /// Create a panel user (used for the initial admin bootstrap at install time).
    Create {
        #[arg(long)]
        username: String,
        #[arg(long)]
        email: String,
        #[arg(long)]
        password: String,
        #[arg(long)]
        admin: bool,
    },
    /// List panel users.
    List,
}

#[derive(Subcommand)]
enum SiteAction {
    /// Create a new site (`--runtime` is one of: static, php:8.3, node:20:index.js, python:3.12:app.py, proxy:http://127.0.0.1:4000).
    Create {
        #[arg(long)]
        domain: String,
        #[arg(long)]
        owner_username: String,
        #[arg(long)]
        owner_user_id: Uuid,
        #[arg(long)]
        runtime: String,
    },
    /// List sites.
    List,
    /// Delete a site by id.
    Delete { id: Uuid },
}

fn parse_runtime(spec: &str) -> anyhow::Result<Runtime> {
    let mut parts = spec.splitn(3, ':');
    let kind = parts.next().unwrap_or_default();
    Ok(match kind {
        "static" => Runtime::Static,
        "php" => Runtime::Php {
            version: parts.next().unwrap_or("8.3").to_string(),
        },
        "node" => Runtime::NodeJs {
            version: parts.next().unwrap_or("20").to_string(),
            entrypoint: parts.next().unwrap_or("index.js").to_string(),
        },
        "python" => Runtime::Python {
            version: parts.next().unwrap_or("3.12").to_string(),
            entrypoint: parts.next().unwrap_or("app.py").to_string(),
        },
        "proxy" => Runtime::ReverseProxy {
            upstream: parts.collect::<Vec<_>>().join(":"),
        },
        other => {
            anyhow::bail!("unknown runtime kind: {other} (expected static|php|node|python|proxy)")
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let agent = AgentClient::new(&cli.agent_socket);

    match cli.command {
        Commands::Capabilities => {
            let caps = agent
                .capabilities()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("{}", serde_json::to_string_pretty(&caps)?);
        }
        Commands::User { action } => handle_user(action).await?,
        Commands::Site { action } => handle_site(action, &agent).await?,
    }
    Ok(())
}

async fn handle_user(action: UserAction) -> anyhow::Result<()> {
    let pool = db::connect().await?;
    match action {
        UserAction::Create {
            username,
            email,
            password,
            admin,
        } => {
            if !panel_core::types::validate::is_valid_username(&username) {
                anyhow::bail!("invalid username (must match ^[a-z][a-z0-9_-]{{2,31}}$)");
            }
            if password.len() < 12 {
                anyhow::bail!("password must be at least 12 characters");
            }
            let hash =
                panel_core::security::hash_password(&password).map_err(|e| anyhow::anyhow!(e))?;
            let user = db::create_user(&pool, &username, &email, &hash, admin).await?;
            println!(
                "Created user {} ({}), admin={}",
                user.username, user.id, user.is_admin
            );
        }
        UserAction::List => {
            let users = db::list_users(&pool).await?;
            let mut table = Table::new();
            table.set_header(vec!["ID", "Username", "Email", "Admin"]);
            for u in users {
                table.add_row(vec![
                    u.id.to_string(),
                    u.username,
                    u.email,
                    u.is_admin.to_string(),
                ]);
            }
            println!("{table}");
        }
    }
    Ok(())
}

async fn handle_site(action: SiteAction, agent: &AgentClient) -> anyhow::Result<()> {
    let pool = db::connect().await?;
    match action {
        SiteAction::Create {
            domain,
            owner_username,
            owner_user_id,
            runtime,
        } => {
            if !panel_core::types::validate::is_valid_domain(&domain) {
                anyhow::bail!("invalid domain: {domain}");
            }
            let runtime = parse_runtime(&runtime)?;
            let site_id = Uuid::new_v4();
            let document_root = format!("/srv/scontrol/sites/{owner_username}/{domain}/public");

            db::insert_site(
                &pool,
                site_id,
                &domain,
                owner_user_id,
                &owner_username,
                serde_json::to_value(&runtime)?,
                &document_root,
            )
            .await?;

            let result = agent
                .create_site(CreateSiteParams {
                    site_id,
                    domain: domain.clone(),
                    owner_username: owner_username.clone(),
                    runtime,
                })
                .await;

            match result {
                Ok(()) => {
                    db::set_site_status(&pool, site_id, "active").await?;
                    println!("Site {domain} created and active (id={site_id}).");
                }
                Err(e) => {
                    db::delete_site(&pool, site_id).await.ok();
                    anyhow::bail!("agent failed to provision site, rolled back: {e}");
                }
            }
        }
        SiteAction::List => {
            let sites = db::list_sites(&pool).await?;
            let mut table = Table::new();
            table.set_header(vec!["ID", "Domain", "Owner", "Status", "SSL"]);
            for s in sites {
                table.add_row(vec![
                    s.id.to_string(),
                    s.domain,
                    s.owner_username,
                    s.status,
                    s.ssl_enabled.to_string(),
                ]);
            }
            println!("{table}");
        }
        SiteAction::Delete { id } => {
            let (domain, owner_user_id, runtime_json) = db::find_site(&pool, id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("site not found"))?;
            let owner_username = db::find_user_by_id(&pool, owner_user_id)
                .await?
                .unwrap_or_default();
            let runtime: Runtime = serde_json::from_value(runtime_json)?;
            let php_version = match runtime {
                Runtime::Php { version } => Some(version),
                _ => None,
            };

            agent
                .delete_site(DeleteSiteParams {
                    site_id: id,
                    domain: domain.clone(),
                    owner_username,
                    php_version,
                })
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            db::delete_site(&pool, id).await?;
            println!("Site {domain} deleted.");
        }
    }
    Ok(())
}

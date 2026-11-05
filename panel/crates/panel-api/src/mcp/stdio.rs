//! MCP stdio transport: newline-delimited JSON-RPC over stdin/stdout, for
//! local AI tool integration (Claude Desktop, Cursor, OpenCode, Gemini CLI).
//!
//! Launched as `panel-api --mcp-stdio`. Acts as the panel admin identified
//! by `PANEL_MCP_ACTOR` (defaults to the first admin account), since a
//! locally-spawned stdio client is already trusted at the level of whoever
//! can run the binary.

use super::protocol::JsonRpcRequest;
use super::server::{self, McpIdentity};
use crate::state::AppState;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn run(state: AppState) -> anyhow::Result<()> {
    let actor_username = std::env::var("PANEL_MCP_ACTOR").ok();
    let user = match actor_username {
        Some(username) => crate::db::users::find_by_username(&state.db, &username).await?,
        None => sqlx::query_as!(
            crate::db::users::UserRow,
            r#"SELECT id, username, email, password_hash, is_admin, totp_secret, totp_enabled, created_at, updated_at
               FROM users WHERE is_admin = TRUE ORDER BY created_at ASC LIMIT 1"#
        )
        .fetch_optional(&state.db)
        .await?,
    }
    .ok_or_else(|| anyhow::anyhow!("no admin user found; run `panel-cli user create --admin` first"))?;

    let authority = panel_core::rbac::Authority {
        user: if user.is_admin {
            panel_core::rbac::GrantSet::superuser()
        } else {
            panel_core::rbac::GrantSet::new(crate::db::rbac::user_grants(&state.db, user.id).await?)
        },
        key: None,
    };
    let identity = McpIdentity {
        user_id: user.id,
        is_admin: user.is_admin,
        authority,
        api_key_id: None,
    };

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => server::handle_request(&state, identity.clone(), request).await,
            Err(e) => {
                tracing::warn!(error = %e, "invalid MCP request over stdio");
                continue;
            }
        };

        let mut serialized = serde_json::to_string(&response)?;
        serialized.push('\n');
        stdout.write_all(serialized.as_bytes()).await?;
        stdout.flush().await?;
    }
}

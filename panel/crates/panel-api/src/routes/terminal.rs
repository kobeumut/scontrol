//! Web terminal (`GET /api/compose-projects/:id/terminal`, WebSocket).
//!
//! Only principals with scoped `services:terminal` may connect. The session is an interactive
//! `docker exec -it <service container> <sh|bash|ash>` run by the agent on a
//! pseudoterminal. A host shell can not be requested: the request names a
//! project service and one of three shell names, nothing else. Opening and
//! closing a session is written to the audit log (with byte counts; the
//! keystrokes themselves are not recorded, since they may hold passwords).
//!
//! Protocol: binary frames carry keystrokes in and terminal output out;
//! text frames are JSON control messages. In: `{"type":"resize","cols":N,
//! "rows":N}`. Out: `{"type":"exit","code":N}` once the shell ended.

use crate::auth::{perm, ServiceAccess};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use panel_core::adapters::{
    AdapterPort, OpenTerminalParams, TerminalCloseParams, TerminalReadParams, TerminalResizeParams,
    TerminalShell, TerminalWriteParams,
};
use panel_core::types::{validate, ComposeSource};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/:id/terminal", get(connect))
}

#[derive(Deserialize)]
struct TerminalQuery {
    service: String,
    #[serde(default = "default_shell")]
    shell: TerminalShell,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_shell() -> TerminalShell {
    TerminalShell::Sh
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// A browser WebSocket carries the session cookie, so a page on another
/// origin could open one. Refuse any `Origin` that is not this host.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Non-browser clients send no Origin; they still need the cookie.
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin_host = origin.split_once("://").map(|(_, rest)| rest);
    matches!((origin_host, host), (Some(o), Some(h)) if o == h)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

async fn connect(
    access: ServiceAccess<perm::ServicesTerminal>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<TerminalQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    if !same_origin(&headers) {
        return Err(ApiError::Forbidden);
    }
    if !validate::is_valid_service_name(&query.service) {
        return Err(ApiError::BadRequest("invalid service name".into()));
    }
    let project = db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))?;
    if matches!(
        project.source,
        ComposeSource::Static(_) | ComposeSource::Database(_)
    ) {
        return Err(ApiError::BadRequest(
            "this project type has no container terminal".into(),
        ));
    }
    let user = access.principal;
    // Resolve once, before the upgrade; pin every operation to this same host.
    let agent = state.agents.for_service(&state.db, id).await?;
    // Opened before the upgrade, so a missing container is a plain HTTP error.
    let session = agent
        .open_terminal(OpenTerminalParams {
            project_slug: project.slug.clone(),
            service_name: query.service.clone(),
            shell: query.shell,
            cols: query.cols,
            rows: query.rows,
            actor: user.username.clone(),
        })
        .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "terminal_open",
        Some(&project.slug),
        serde_json::json!({
            "project_id": project.id,
            "service": query.service,
            "shell": query.shell.program(),
            "session_id": session.session_id,
        }),
    )
    .await
    .ok();
    let service = query.service;
    let slug = project.slug;
    let session_id = session.session_id;
    Ok(ws.on_upgrade(move |socket| async move {
        let bytes_in = Arc::new(AtomicU64::new(0));
        let bytes_out = Arc::new(AtomicU64::new(0));
        pump(
            agent.clone(),
            socket,
            session_id,
            bytes_in.clone(),
            bytes_out.clone(),
        )
        .await;
        agent
            .close_terminal(TerminalCloseParams { session_id })
            .await
            .ok();
        db::audit::record(
            &state.db,
            Some(user.id),
            user.actor_kind(),
            "terminal_close",
            Some(&slug),
            serde_json::json!({
                "session_id": session_id,
                "service": service,
                "bytes_in": bytes_in.load(Ordering::Relaxed),
                "bytes_out": bytes_out.load(Ordering::Relaxed),
            }),
        )
        .await
        .ok();
    }))
}

async fn pump(
    agent: Arc<dyn AdapterPort>,
    socket: WebSocket,
    session_id: Uuid,
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
) {
    let (mut tx, mut rx) = socket.split();
    let output_agent = agent.clone();
    let output = async {
        loop {
            let read = output_agent
                .read_terminal(TerminalReadParams {
                    session_id,
                    wait_ms: 20_000,
                })
                .await;
            let Ok(chunk) = read else { break };
            if !chunk.data.is_empty() {
                let Ok(bytes) = B64.decode(chunk.data.as_bytes()) else {
                    break;
                };
                bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if tx.send(Message::Binary(bytes)).await.is_err() {
                    break;
                }
            }
            if let Some(code) = chunk.exited {
                let text = serde_json::json!({ "type": "exit", "code": code }).to_string();
                tx.send(Message::Text(text)).await.ok();
                tx.close().await.ok();
                break;
            }
        }
    };
    let input = async {
        while let Some(Ok(message)) = rx.next().await {
            match message {
                Message::Binary(bytes) => {
                    bytes_in.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    let write = agent
                        .write_terminal(TerminalWriteParams {
                            session_id,
                            data: B64.encode(bytes),
                        })
                        .await;
                    if write.is_err() {
                        break;
                    }
                }
                Message::Text(text) => {
                    if let Ok(Control::Resize { cols, rows }) = serde_json::from_str(&text) {
                        agent
                            .resize_terminal(TerminalResizeParams {
                                session_id,
                                cols,
                                rows,
                            })
                            .await
                            .ok();
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = output => {}
        _ = input => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            map.insert(header::ORIGIN, origin.parse().unwrap());
        }
        map
    }

    #[test]
    fn cross_origin_websockets_are_refused() {
        assert!(same_origin(&headers(None, "panel.example.com")));
        assert!(same_origin(&headers(
            Some("https://panel.example.com"),
            "panel.example.com"
        )));
        assert!(!same_origin(&headers(
            Some("https://evil.example.org"),
            "panel.example.com"
        )));
        assert!(!same_origin(&headers(Some("null"), "panel.example.com")));
    }

    #[test]
    fn shell_is_limited_to_the_whitelist() {
        let ok: Result<TerminalQuery, _> = serde_urlencoded_probe("service=web&shell=bash");
        assert!(ok.is_ok());
        assert!(serde_urlencoded_probe("service=web&shell=zsh").is_err());
        assert!(serde_urlencoded_probe("service=web&shell=%2Fbin%2Fsh").is_err());
    }

    fn serde_urlencoded_probe(q: &str) -> Result<TerminalQuery, String> {
        // `shell` deserializes through the same enum the Query extractor uses.
        let mut service = String::new();
        let mut shell = default_shell();
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap();
            match k {
                "service" => service = v.into(),
                "shell" => {
                    shell = serde_json::from_str(&format!("\"{}\"", v.replace("%2F", "/")))
                        .map_err(|e| e.to_string())?
                }
                _ => {}
            }
        }
        Ok(TerminalQuery {
            service,
            shell,
            cols: 80,
            rows: 24,
        })
    }
}

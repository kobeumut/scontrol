//! Servers: the panel's own host and remote agents (`/api/servers`).
//!
//! A remote agent over mutual TLS is enrolled from a CSR it made with
//! `panel-agent remote-init`: the panel signs it for the server's host and
//! returns the certificate and the panel CA to install next to the key. A
//! rotation is the same with a new CSR; the previous certificate stays
//! accepted until the agent presents the new one, or for seven days.

use crate::auth::{perm, Require};
use crate::db;
use crate::db::servers::{NewServer, Server, Transport, LOCAL_SERVER_ID};
use crate::error::{ApiError, ApiResult};
use crate::routes::access::{audit, conflict_on_unique};
use crate::services::agent_pool::is_valid_ssh_socket_path;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use panel_core::pki;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How long a replaced agent certificate stays accepted.
const ROTATION_GRACE_DAYS: i64 = 7;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/ca", get(ca))
        .route("/:id", get(show).put(update).delete(remove))
        .route("/:id/certificate", get(certificate).post(rotate))
        .route("/:id/check", post(check))
}

async fn list(
    _: Require<perm::ServersRead>,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<Server>>> {
    Ok(Json(db::servers::list(&state.db).await?))
}

async fn show(
    _: Require<perm::ServersRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Server>> {
    Ok(Json(find(&state, id).await?))
}

#[derive(Serialize)]
struct CaInfo {
    ca_pem: String,
}

/// The panel CA certificate (public), installed as `ca.pem` on agents.
async fn ca(
    _: Require<perm::ServersRead>,
    State(state): State<AppState>,
) -> ApiResult<Json<CaInfo>> {
    Ok(Json(CaInfo {
        ca_pem: state.agents.pki().ca(&state.db).await?.cert_pem,
    }))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    transport: Transport,
    /// `host:port` for `mtls`; `/run/scontrol/remote/<name>.sock` for
    /// `ssh_socket`.
    address: String,
    /// The agent's certificate signing request (`mtls` only).
    #[serde(default)]
    csr_pem: Option<String>,
    #[serde(default)]
    max_services: Option<i32>,
}

#[derive(Serialize)]
struct Enrolled {
    server: Server,
    /// Install as `agent.pem` (mTLS only).
    certificate_pem: Option<String>,
    /// Install as `ca.pem` (mTLS only).
    ca_pem: Option<String>,
}

fn validate_name(name: &str) -> ApiResult<()> {
    if !panel_core::rbac::is_valid_role_name(name) || name == "local" {
        return Err(ApiError::BadRequest(
            "server names are 2–40 lowercase letters, digits, and dashes".into(),
        ));
    }
    Ok(())
}

fn validate_capacity(max: Option<i32>) -> ApiResult<()> {
    match max {
        Some(n) if !(0..=10_000).contains(&n) => Err(ApiError::BadRequest(
            "max_services must be between 0 and 10000".into(),
        )),
        _ => Ok(()),
    }
}

async fn create(
    Require { principal, .. }: Require<perm::ServersManage>,
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Enrolled>)> {
    validate_name(&body.name)?;
    validate_capacity(body.max_services)?;
    let address = body.address.trim();
    let csr = match body.transport {
        Transport::Local => {
            return Err(ApiError::BadRequest(
                "the local server exists already; add remote servers only".into(),
            ))
        }
        Transport::SshSocket => {
            if !is_valid_ssh_socket_path(address) {
                return Err(ApiError::BadRequest(format!(
                    "the forwarded socket must be {}<name>.sock",
                    crate::services::agent_pool::SSH_SOCKET_DIR
                )));
            }
            None
        }
        Transport::Mtls => {
            let (host, _) = db::servers::split_host_port(address).ok_or_else(|| {
                ApiError::BadRequest("address must be host:port ([v6]:port for IPv6)".into())
            })?;
            if host.parse::<std::net::IpAddr>().is_err()
                && !panel_core::types::validate::is_valid_domain(host)
            {
                return Err(ApiError::BadRequest("invalid host in address".into()));
            }
            Some(
                body.csr_pem
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| {
                        ApiError::BadRequest("paste the CSR from `panel-agent remote-init`".into())
                    })?,
            )
        }
    };
    let id = Uuid::new_v4();
    // Sign before inserting, so a bad CSR leaves no half-enrolled server.
    let signed = match csr {
        Some(csr) => {
            let ca = state.agents.pki().ca(&state.db).await?;
            let host = db::servers::split_host_port(address).map_or("", |(h, _)| h);
            let (cert_pem, info) = pki::sign_agent_csr(&ca, csr, host, id, pki::AGENT_CERT_DAYS)?;
            Some((cert_pem, info, ca.cert_pem))
        }
        None => None,
    };
    db::servers::insert(
        &state.db,
        NewServer {
            id,
            name: &body.name,
            transport: body.transport,
            address,
            max_services: body.max_services,
        },
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a server with this name exists"))?;
    if let Some((cert_pem, info, _)) = &signed {
        db::servers::set_certificate(&state.db, id, cert_pem, info, chrono::Duration::zero())
            .await?;
    }
    audit(
        &state,
        &principal,
        "create_server",
        &body.name,
        serde_json::json!({
            "server_id": id,
            "transport": body.transport,
            "address": address,
            "cert_serial": signed.as_ref().map(|(_, info, _)| info.serial_hex.clone()),
        }),
    )
    .await;
    let server = find(&state, id).await?;
    // Best effort: an SSH tunnel may already be up. A freshly enrolled mTLS
    // agent still needs its certificate installed.
    if server.transport == Transport::SshSocket {
        state.agents.check(&state.db, &server).await.ok();
    }
    let server = find(&state, id).await?;
    let (certificate_pem, ca_pem) = match signed {
        Some((cert, _, ca)) => (Some(cert), Some(ca)),
        None => (None, None),
    };
    Ok((
        StatusCode::CREATED,
        Json(Enrolled {
            server,
            certificate_pem,
            ca_pem,
        }),
    ))
}

#[derive(Serialize)]
struct CertificateInfo {
    certificate_pem: String,
    ca_pem: String,
}

async fn certificate(
    _: Require<perm::ServersRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CertificateInfo>> {
    let server = find(&state, id).await?;
    let certificate_pem = db::servers::certificate_pem(&state.db, server.id)
        .await?
        .ok_or_else(|| ApiError::NotFound("this server has no certificate".into()))?;
    Ok(Json(CertificateInfo {
        certificate_pem,
        ca_pem: state.agents.pki().ca(&state.db).await?.cert_pem,
    }))
}

#[derive(Deserialize)]
struct RotateBody {
    csr_pem: String,
}

/// Signs a new CSR (from `panel-agent remote-init --force`).
async fn rotate(
    Require { principal, .. }: Require<perm::ServersManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<RotateBody>,
) -> ApiResult<Json<CertificateInfo>> {
    let server = find(&state, id).await?;
    if server.transport != Transport::Mtls {
        return Err(ApiError::BadRequest(
            "only mTLS servers have certificates".into(),
        ));
    }
    let ca = state.agents.pki().ca(&state.db).await?;
    let (certificate_pem, info) = pki::sign_agent_csr(
        &ca,
        &body.csr_pem,
        server.host(),
        server.id,
        pki::AGENT_CERT_DAYS,
    )?;
    db::servers::set_certificate(
        &state.db,
        id,
        &certificate_pem,
        &info,
        chrono::Duration::days(ROTATION_GRACE_DAYS),
    )
    .await?;
    audit(
        &state,
        &principal,
        "rotate_server_certificate",
        &server.name,
        serde_json::json!({ "server_id": id, "cert_serial": info.serial_hex }),
    )
    .await;
    Ok(Json(CertificateInfo {
        certificate_pem,
        ca_pem: ca.cert_pem,
    }))
}

#[derive(Serialize)]
struct CheckResult {
    ok: bool,
    message: Option<String>,
    server: Server,
}

async fn check(
    _: Require<perm::ServersRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CheckResult>> {
    let server = find(&state, id).await?;
    if server.id == LOCAL_SERVER_ID {
        let result = state.agent.capabilities().await;
        let server = find(&state, id).await?;
        return Ok(Json(CheckResult {
            ok: result.is_ok(),
            message: result.err().map(|e| e.message().to_string()),
            server,
        }));
    }
    let result = state.agents.check(&state.db, &server).await;
    let server = find(&state, id).await?;
    Ok(Json(CheckResult {
        ok: result.is_ok(),
        message: result.err().map(|e| match e {
            ApiError::Core(core) => core.message().to_string(),
            other => other.to_string(),
        }),
        server,
    }))
}

#[derive(Deserialize)]
struct UpdateBody {
    name: String,
    draining: bool,
    #[serde(default)]
    max_services: Option<i32>,
}

async fn update(
    Require { principal, .. }: Require<perm::ServersManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<Server>> {
    let server = find(&state, id).await?;
    if server.id == LOCAL_SERVER_ID {
        if body.name != server.name {
            return Err(ApiError::BadRequest(
                "the local server cannot be renamed".into(),
            ));
        }
    } else {
        validate_name(&body.name)?;
    }
    validate_capacity(body.max_services)?;
    db::servers::update_settings(&state.db, id, &body.name, body.draining, body.max_services)
        .await
        .map_err(|e| conflict_on_unique(e, "a server with this name exists"))?;
    audit(
        &state,
        &principal,
        "update_server",
        &body.name,
        serde_json::json!({ "server_id": id, "draining": body.draining, "max_services": body.max_services }),
    )
    .await;
    Ok(Json(find(&state, id).await?))
}

async fn remove(
    Require { principal, .. }: Require<perm::ServersManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let server = find(&state, id).await?;
    if server.id == LOCAL_SERVER_ID {
        return Err(ApiError::BadRequest(
            "the local server cannot be removed".into(),
        ));
    }
    if server.service_count > 0 {
        return Err(ApiError::Conflict(
            "delete this server's services first".into(),
        ));
    }
    db::servers::delete(&state.db, id)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                ApiError::Conflict("delete this server's services first".into())
            }
            _ => e.into(),
        })?;
    audit(
        &state,
        &principal,
        "delete_server",
        &server.name,
        serde_json::json!({ "server_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<Server> {
    db::servers::find(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("server not found".into()))
}

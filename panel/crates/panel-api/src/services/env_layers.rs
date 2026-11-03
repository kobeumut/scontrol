//! Layered environment variables: project → environment → service.
//!
//! Every layer is stored sealed with [`SecretBox`], bound to its scope and
//! key, so a sealed value copied to another row or key fails to open. The
//! API only ever reports key names and which layer wins. At deploy time the
//! queue worker opens the layers of one service, merges them (the most
//! specific layer wins), and hands the result to the agent through
//! `write_compose_environment`; overridden inherited values go along only
//! so the agent can mask them in logs.

use crate::db;
use crate::db::projects::{EnvScope, SealedVariable};
use crate::error::{ApiError, ApiResult};
use crate::services::secret_box::SecretBox;
use panel_core::types::{
    describe_env_layers, merge_env_layers, validate, ComposeProject, EnvKeyInfo, EnvLayer,
    EnvLayerKeys, MAX_ENV_VARS_PER_LAYER,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Most bytes one value may hold (same limit the agent enforces).
const MAX_VALUE_LEN: usize = 16 * 1024;

/// One variable of a layer update. `value: None` keeps the stored value of
/// an existing key, so a client can edit one variable without re-entering
/// the others (it never sees them).
#[derive(Debug, Clone, Deserialize)]
pub struct VariableInput {
    pub key: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// What the API shows of a service's variables: key names per layer and,
/// per key, the winning layer and the layers it overrides.
#[derive(Debug, Serialize)]
pub struct ServiceVariables {
    /// False for a service that predates layering: its own variables are
    /// unknown to the panel and nothing is inherited until it is saved.
    pub layered: bool,
    pub layers: Vec<EnvLayerKeys>,
    pub variables: Vec<EnvKeyInfo>,
}

fn aad(scope: EnvScope, key: &str) -> Vec<u8> {
    format!(
        "scontrol/env-var/v1:{}:{}:{key}",
        scope.layer().as_str(),
        scope.id()
    )
    .into_bytes()
}

/// Checks keys and values. Error messages name keys only, never values.
pub fn validate_pairs(pairs: &[(String, String)]) -> ApiResult<()> {
    if pairs.len() > MAX_ENV_VARS_PER_LAYER {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_ENV_VARS_PER_LAYER} variables per layer"
        )));
    }
    let mut seen = HashSet::new();
    for (key, value) in pairs {
        if !validate::is_valid_env_key(key) {
            return Err(ApiError::BadRequest("invalid project environment".into()));
        }
        if value.contains('\0') || value.contains('\n') || value.len() > MAX_VALUE_LEN {
            return Err(ApiError::BadRequest(format!(
                "invalid value for variable {key}"
            )));
        }
        if !seen.insert(key.as_str()) {
            return Err(ApiError::BadRequest(format!("duplicate variable {key}")));
        }
    }
    Ok(())
}

fn seal(secrets: &SecretBox, scope: EnvScope, pairs: &[(String, String)]) -> Vec<SealedVariable> {
    pairs
        .iter()
        .map(|(key, value)| SealedVariable {
            key: key.clone(),
            value_sealed: secrets.seal(value.as_bytes(), &aad(scope, key)),
        })
        .collect()
}

fn open(
    secrets: &SecretBox,
    scope: EnvScope,
    sealed: Vec<SealedVariable>,
) -> anyhow::Result<Vec<(String, String)>> {
    sealed
        .into_iter()
        .map(|v| {
            let plain = secrets.open(&v.value_sealed, &aad(scope, &v.key))?;
            Ok((v.key, String::from_utf8(plain)?))
        })
        .collect()
}

/// Replaces a layer with `pairs` (all values given).
pub async fn replace(
    pool: &PgPool,
    secrets: &SecretBox,
    scope: EnvScope,
    pairs: &[(String, String)],
) -> ApiResult<()> {
    validate_pairs(pairs)?;
    db::projects::replace_layer(pool, scope, &seal(secrets, scope, pairs)).await?;
    Ok(())
}

/// Replaces a layer with `inputs`, keeping the stored value wherever an
/// input omits it. Keys absent from `inputs` are removed. Returns the keys.
pub async fn update(
    pool: &PgPool,
    secrets: &SecretBox,
    scope: EnvScope,
    inputs: Vec<VariableInput>,
) -> ApiResult<Vec<String>> {
    if inputs.len() > MAX_ENV_VARS_PER_LAYER {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_ENV_VARS_PER_LAYER} variables per layer"
        )));
    }
    let mut stored: HashMap<String, Vec<u8>> = db::projects::layer_variables(pool, scope)
        .await?
        .into_iter()
        .map(|v| (v.key, v.value_sealed))
        .collect();
    let mut sealed = Vec::with_capacity(inputs.len());
    let mut fresh: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for input in inputs {
        if !validate::is_valid_env_key(&input.key) {
            return Err(ApiError::BadRequest("invalid variable name".into()));
        }
        if !seen.insert(input.key.clone()) {
            return Err(ApiError::BadRequest(format!(
                "duplicate variable {}",
                input.key
            )));
        }
        match input.value {
            Some(value) => fresh.push((input.key, value)),
            None => {
                // Re-binding is unnecessary: the key and scope are unchanged.
                let value_sealed = stored.remove(&input.key).ok_or_else(|| {
                    ApiError::BadRequest(format!("variable {} has no stored value", input.key))
                })?;
                sealed.push(SealedVariable {
                    key: input.key,
                    value_sealed,
                });
            }
        }
    }
    validate_pairs(&fresh)?;
    sealed.extend(seal(secrets, scope, &fresh));
    sealed.sort_by(|a, b| a.key.cmp(&b.key));
    db::projects::replace_layer(pool, scope, &sealed).await?;
    Ok(sealed.into_iter().map(|v| v.key).collect())
}

/// Key names of one layer.
pub async fn keys(pool: &PgPool, scope: EnvScope) -> ApiResult<Vec<String>> {
    Ok(db::projects::layer_variables(pool, scope)
        .await?
        .into_iter()
        .map(|v| v.key)
        .collect())
}

async fn scopes_of(pool: &PgPool, service_id: Uuid) -> ApiResult<[EnvScope; 3]> {
    let (environment_id, project_id) = db::projects::service_scopes(pool, service_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))?;
    Ok([
        EnvScope::Project(project_id),
        EnvScope::Environment(environment_id),
        EnvScope::Service(service_id),
    ])
}

/// Key names of a service's three layers and how they combine.
pub async fn service_variables(
    pool: &PgPool,
    service: &ComposeProject,
) -> ApiResult<ServiceVariables> {
    let mut layers = Vec::with_capacity(3);
    for scope in scopes_of(pool, service.id).await? {
        layers.push(EnvLayerKeys {
            layer: scope.layer(),
            keys: keys(pool, scope).await?,
        });
    }
    let variables = describe_env_layers(&layers);
    Ok(ServiceVariables {
        layered: service.env_layered,
        layers,
        variables,
    })
}

/// Key names of an environment and its project, combined.
pub async fn environment_variables(
    pool: &PgPool,
    project_id: Uuid,
    environment_id: Uuid,
) -> ApiResult<(Vec<EnvLayerKeys>, Vec<EnvKeyInfo>)> {
    let layers = vec![
        EnvLayerKeys {
            layer: EnvLayer::Project,
            keys: keys(pool, EnvScope::Project(project_id)).await?,
        },
        EnvLayerKeys {
            layer: EnvLayer::Environment,
            keys: keys(pool, EnvScope::Environment(environment_id)).await?,
        },
    ];
    let variables = describe_env_layers(&layers);
    Ok((layers, variables))
}

/// The merged environment of a service and the overridden inherited values
/// (for log masking). Plaintext lives only in memory, on its way to the
/// agent.
pub async fn merged_for_service(
    pool: &PgPool,
    secrets: &SecretBox,
    service_id: Uuid,
) -> ApiResult<(Vec<(String, String)>, Vec<String>)> {
    let mut layers = Vec::with_capacity(3);
    for scope in scopes_of(pool, service_id).await? {
        let sealed = db::projects::layer_variables(pool, scope).await?;
        layers.push((scope.layer(), open(secrets, scope, sealed)?));
    }
    Ok(merge_env_layers(&layers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_pool, user};
    use panel_core::types::ComposeSource;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn service(pool: &PgPool) -> ComposeProject {
        let owner = user(pool, true).await;
        db::compose_projects::create(
            pool,
            Uuid::new_v4(),
            "Layered",
            &format!("p{}", &Uuid::new_v4().simple().to_string()[..12]),
            owner.id,
            None,
            &ComposeSource::Raw {
                compose_yaml: "services:\n  web:\n    image: nginx\n".into(),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn layers_merge_with_service_over_environment_over_project() {
        let pool = test_pool().await;
        let secrets = SecretBox::derive(&[7u8; 32]);
        let svc = service(&pool).await;
        let [project, environment, own] = scopes_of(&pool, svc.id).await.unwrap();
        replace(
            &pool,
            &secrets,
            project,
            &pairs(&[("DB_URL", "proj-db-111"), ("REGION", "eu-111")]),
        )
        .await
        .unwrap();
        replace(
            &pool,
            &secrets,
            environment,
            &pairs(&[("DB_URL", "env-db-222"), ("LOG", "debug-222")]),
        )
        .await
        .unwrap();
        replace(&pool, &secrets, own, &pairs(&[("DB_URL", "svc-db-333")]))
            .await
            .unwrap();

        let (merged, masked) = merged_for_service(&pool, &secrets, svc.id).await.unwrap();
        let merged: HashMap<_, _> = merged.into_iter().collect();
        assert_eq!(merged["DB_URL"], "svc-db-333");
        assert_eq!(merged["LOG"], "debug-222");
        assert_eq!(merged["REGION"], "eu-111");
        assert_eq!(merged.len(), 3);
        assert!(masked.contains(&"proj-db-111".to_string()));
        assert!(masked.contains(&"env-db-222".to_string()));

        let view = service_variables(&pool, &svc).await.unwrap();
        let db_url = view.variables.iter().find(|v| v.key == "DB_URL").unwrap();
        assert_eq!(db_url.source, EnvLayer::Service);
        assert_eq!(
            db_url.overrides,
            vec![EnvLayer::Project, EnvLayer::Environment]
        );
        let text = serde_json::to_string(&view).unwrap();
        for value in [
            "proj-db-111",
            "eu-111",
            "env-db-222",
            "debug-222",
            "svc-db-333",
        ] {
            assert!(!text.contains(value), "value leaked into {text}");
        }

        // Stored rows hold ciphertext only.
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT to_jsonb(v)::text FROM env_variables v
             WHERE project_id = $1 OR environment_id = $2 OR service_id = $3",
        )
        .bind(project.id())
        .bind(environment.id())
        .bind(own.id())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert!(!row.contains("-111") && !row.contains("-222") && !row.contains("-333"));
        }
    }

    #[tokio::test]
    async fn update_keeps_omitted_values_and_rejects_unknown_ones() {
        let pool = test_pool().await;
        let secrets = SecretBox::derive(&[7u8; 32]);
        let svc = service(&pool).await;
        let scope = EnvScope::Service(svc.id);
        replace(
            &pool,
            &secrets,
            scope,
            &pairs(&[("A", "one"), ("B", "two")]),
        )
        .await
        .unwrap();
        let keys = update(
            &pool,
            &secrets,
            scope,
            vec![
                VariableInput {
                    key: "A".into(),
                    value: None,
                },
                VariableInput {
                    key: "C".into(),
                    value: Some("three".into()),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(keys, vec!["A", "C"]);
        let (merged, _) = merged_for_service(&pool, &secrets, svc.id).await.unwrap();
        assert_eq!(merged, pairs(&[("A", "one"), ("C", "three")]));

        let unknown = update(
            &pool,
            &secrets,
            scope,
            vec![VariableInput {
                key: "NOPE".into(),
                value: None,
            }],
        )
        .await;
        assert!(matches!(unknown, Err(ApiError::BadRequest(_))));
        let duplicate = replace(&pool, &secrets, scope, &pairs(&[("A", "1"), ("A", "2")])).await;
        assert!(matches!(duplicate, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn sealed_values_are_bound_to_their_scope_and_key() {
        let pool = test_pool().await;
        let secrets = SecretBox::derive(&[7u8; 32]);
        let svc = service(&pool).await;
        let sealed = seal(&secrets, EnvScope::Service(svc.id), &pairs(&[("A", "x")]));
        let moved = vec![SealedVariable {
            key: "A".into(),
            value_sealed: sealed[0].value_sealed.clone(),
        }];
        assert!(open(&secrets, EnvScope::Project(svc.id), moved).is_err());
        let renamed = vec![SealedVariable {
            key: "B".into(),
            value_sealed: sealed[0].value_sealed.clone(),
        }];
        assert!(open(&secrets, EnvScope::Service(svc.id), renamed).is_err());
    }
}

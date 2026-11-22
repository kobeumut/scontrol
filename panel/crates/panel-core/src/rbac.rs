//! Role-based access control: the permission catalog, scopes, built-in
//! roles, and the pure checks panel-api's extractors run.
//!
//! Every check is fail-closed: a permission is held only through an explicit
//! grant whose scope covers the target. A principal with no grant, an unknown
//! permission string, or a grant at a narrower scope is refused. Nothing is
//! paywalled; every permission can be granted to anyone.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// One `resource:action` pair. The string form is what the database and the
/// API carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    ProjectsRead,
    ProjectsCreate,
    ProjectsUpdate,
    ProjectsDelete,
    ServicesRead,
    ServicesCreate,
    ServicesUpdate,
    ServicesDelete,
    ServicesDeploy,
    ServicesTerminal,
    TemplatesInstall,
    VariablesRead,
    VariablesUpdate,
    ServersRead,
    ServersManage,
    AccessManage,
    SystemAdmin,
}

impl Permission {
    pub const ALL: [Permission; 17] = [
        Permission::ProjectsRead,
        Permission::ProjectsCreate,
        Permission::ProjectsUpdate,
        Permission::ProjectsDelete,
        Permission::ServicesRead,
        Permission::ServicesCreate,
        Permission::ServicesUpdate,
        Permission::ServicesDelete,
        Permission::ServicesDeploy,
        Permission::ServicesTerminal,
        Permission::TemplatesInstall,
        Permission::VariablesRead,
        Permission::VariablesUpdate,
        Permission::ServersRead,
        Permission::ServersManage,
        Permission::AccessManage,
        Permission::SystemAdmin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Permission::ProjectsRead => "projects:read",
            Permission::ProjectsCreate => "projects:create",
            Permission::ProjectsUpdate => "projects:update",
            Permission::ProjectsDelete => "projects:delete",
            Permission::ServicesRead => "services:read",
            Permission::ServicesCreate => "services:create",
            Permission::ServicesUpdate => "services:update",
            Permission::ServicesDelete => "services:delete",
            Permission::ServicesDeploy => "services:deploy",
            Permission::ServicesTerminal => "services:terminal",
            Permission::TemplatesInstall => "templates:install",
            Permission::VariablesRead => "variables:read",
            Permission::VariablesUpdate => "variables:update",
            Permission::ServersRead => "servers:read",
            Permission::ServersManage => "servers:manage",
            Permission::AccessManage => "access:manage",
            Permission::SystemAdmin => "system:admin",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Permission::ProjectsRead => "See projects and their environments",
            Permission::ProjectsCreate => "Create projects",
            Permission::ProjectsUpdate => {
                "Rename projects, add environments, edit shared variables"
            }
            Permission::ProjectsDelete => "Delete projects and environments",
            Permission::ServicesRead => "See services, deployments, logs, and domains",
            Permission::ServicesCreate => "Create services",
            Permission::ServicesUpdate => "Change domains, Git access, registries, and webhooks",
            Permission::ServicesDelete => "Delete services",
            Permission::ServicesDeploy => "Deploy, stop, cancel, and roll back services",
            Permission::ServicesTerminal => "Open an interactive shell inside a service container",
            Permission::TemplatesInstall => "Install vetted templates in an environment",
            Permission::VariablesRead => "See variable names (values are never returned)",
            Permission::VariablesUpdate => "Write service variables",
            Permission::ServersRead => "See servers and their status",
            Permission::ServersManage => "Add, rotate, drain, and remove servers",
            Permission::AccessManage => "Manage roles and role bindings",
            Permission::SystemAdmin => "Server-wide settings (registries, backups, alerts, ...)",
        }
    }

    /// Whether a project or environment binding can carry this permission.
    /// The rest exist only at global scope; a narrower grant of one of them
    /// grants nothing.
    pub fn is_scopable(self) -> bool {
        matches!(
            self,
            Permission::ProjectsRead
                | Permission::ProjectsUpdate
                | Permission::ProjectsDelete
                | Permission::ServicesRead
                | Permission::ServicesCreate
                | Permission::ServicesUpdate
                | Permission::ServicesDelete
                | Permission::ServicesDeploy
                | Permission::ServicesTerminal
                | Permission::TemplatesInstall
                | Permission::VariablesRead
                | Permission::VariablesUpdate
        )
    }

    /// Whether an API key may carry this permission. Role and binding
    /// management and server-wide administration need an interactive session
    /// (and its 2FA).
    pub fn allowed_for_api_keys(self) -> bool {
        !matches!(self, Permission::AccessManage | Permission::SystemAdmin)
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Permission::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unknown permission '{s}'"))
    }
}

impl Serialize for Permission {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Where a grant applies. A project scope covers the project and all of its
/// environments; an environment scope covers that environment only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    Global,
    Project {
        project_id: Uuid,
    },
    Environment {
        project_id: Uuid,
        environment_id: Uuid,
    },
}

/// What a request touches, resolved from its path (a service resolves to
/// its environment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Global,
    Project(Uuid),
    Environment { project: Uuid, environment: Uuid },
}

impl Scope {
    pub fn covers(&self, target: Target) -> bool {
        match (self, target) {
            (Scope::Global, _) => true,
            (Scope::Project { project_id }, Target::Project(p)) => *project_id == p,
            (Scope::Project { project_id }, Target::Environment { project, .. }) => {
                *project_id == project
            }
            (
                Scope::Environment {
                    project_id,
                    environment_id,
                },
                Target::Environment {
                    project,
                    environment,
                },
            ) => *project_id == project && *environment_id == environment,
            _ => false,
        }
    }

    fn touches_project(&self, project: Uuid) -> bool {
        match self {
            Scope::Global => true,
            Scope::Project { project_id } | Scope::Environment { project_id, .. } => {
                *project_id == project
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Grant {
    pub permission: Permission,
    pub scope: Scope,
}

impl Grant {
    fn allows(&self, permission: Permission, target: Target) -> bool {
        self.permission == permission
            && self.scope.covers(target)
            && (permission.is_scopable() || self.scope == Scope::Global)
    }
}

/// The grants one party holds. `superuser` is the installation admin
/// (`users.is_admin`), who holds every permission everywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantSet {
    pub superuser: bool,
    pub grants: Vec<Grant>,
}

impl GrantSet {
    pub fn superuser() -> Self {
        Self {
            superuser: true,
            grants: Vec::new(),
        }
    }

    pub fn new(grants: Vec<Grant>) -> Self {
        Self {
            superuser: false,
            grants,
        }
    }

    pub fn allows(&self, permission: Permission, target: Target) -> bool {
        self.superuser || self.grants.iter().any(|g| g.allows(permission, target))
    }

    /// Whether the permission is held anywhere inside `project` (the project
    /// itself or one of its environments). Used to list projects a user can
    /// reach only through an environment binding.
    pub fn allows_within(&self, permission: Permission, project: Uuid) -> bool {
        self.superuser
            || self.grants.iter().any(|g| {
                g.permission == permission
                    && g.scope.touches_project(project)
                    && (permission.is_scopable() || g.scope == Scope::Global)
            })
    }
}

/// The authority behind one request: the user's grants, narrowed by an API
/// key's own grants when the request carries one. A key never exceeds its
/// owner, and removing the owner's role disables the key's matching scopes
/// at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub user: GrantSet,
    pub key: Option<GrantSet>,
}

impl Authority {
    pub fn allows(&self, permission: Permission, target: Target) -> bool {
        self.user.allows(permission, target)
            && self
                .key
                .as_ref()
                .is_none_or(|key| key.allows(permission, target))
    }

    pub fn allows_within(&self, permission: Permission, project: Uuid) -> bool {
        self.user.allows_within(permission, project)
            && self
                .key
                .as_ref()
                .is_none_or(|key| key.allows_within(permission, project))
    }
}

/// A role shipped with the panel. Built-in roles cannot be edited or
/// deleted, so a binding to one always means the same thing.
pub struct BuiltinRole {
    pub name: &'static str,
    pub description: &'static str,
    pub permissions: &'static [Permission],
}

pub const BUILTIN_ROLES: &[BuiltinRole] = &[
    BuiltinRole {
        name: "admin",
        description: "Every permission. Bind globally for a full administrator.",
        permissions: &Permission::ALL,
    },
    BuiltinRole {
        name: "project-admin",
        description: "Full control of the projects, environments, and services in scope",
        permissions: &[
            Permission::ServicesTerminal,
            Permission::TemplatesInstall,
            Permission::ProjectsRead,
            Permission::ProjectsUpdate,
            Permission::ProjectsDelete,
            Permission::ServicesRead,
            Permission::ServicesCreate,
            Permission::ServicesUpdate,
            Permission::ServicesDelete,
            Permission::ServicesDeploy,
            Permission::VariablesRead,
            Permission::VariablesUpdate,
        ],
    },
    BuiltinRole {
        name: "developer",
        description: "Create, configure, and deploy services; edit variables",
        permissions: &[
            Permission::ProjectsRead,
            Permission::ServicesRead,
            Permission::ServicesCreate,
            Permission::ServicesUpdate,
            Permission::ServicesDeploy,
            Permission::VariablesRead,
            Permission::VariablesUpdate,
        ],
    },
    BuiltinRole {
        name: "deployer",
        description: "Trigger deployments and rollbacks (for CI)",
        permissions: &[
            Permission::ProjectsRead,
            Permission::ServicesRead,
            Permission::ServicesDeploy,
        ],
    },
    BuiltinRole {
        name: "viewer",
        description: "Read-only access; variable names but never values",
        permissions: &[
            Permission::ProjectsRead,
            Permission::ServicesRead,
            Permission::VariablesRead,
        ],
    },
];

/// Most grants one API key may carry.
pub const MAX_KEY_GRANTS: usize = 64;

/// Checks the grants requested for a new API key: at least one, a bounded
/// number, only key-eligible permissions, and global-only permissions only at
/// global scope (a narrower grant of one would silently grant nothing).
pub fn validate_key_grants(grants: &[Grant]) -> Result<(), String> {
    if grants.is_empty() {
        return Err("an API key needs at least one permission".into());
    }
    if grants.len() > MAX_KEY_GRANTS {
        return Err(format!(
            "an API key holds at most {MAX_KEY_GRANTS} permissions"
        ));
    }
    for grant in grants {
        if !grant.permission.allowed_for_api_keys() {
            return Err(format!(
                "{} cannot be granted to an API key",
                grant.permission
            ));
        }
        if !grant.permission.is_scopable() && grant.scope != Scope::Global {
            return Err(format!("{} exists only at global scope", grant.permission));
        }
    }
    Ok(())
}

/// Role names: lowercase letters, digits, and dashes, 2–40 characters.
pub fn is_valid_role_name(name: &str) -> bool {
    (2..=40).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid, Uuid) {
        (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4())
    }

    #[test]
    fn permission_strings_round_trip() {
        for permission in Permission::ALL {
            assert_eq!(permission.as_str().parse::<Permission>(), Ok(permission));
        }
        assert!("services:*".parse::<Permission>().is_err());
        assert!("".parse::<Permission>().is_err());
    }

    #[test]
    fn no_grants_means_no_access() {
        let set = GrantSet::default();
        for permission in Permission::ALL {
            assert!(!set.allows(permission, Target::Global));
        }
    }

    #[test]
    fn superuser_holds_everything() {
        let (p, e, _) = ids();
        let set = GrantSet::superuser();
        assert!(set.allows(Permission::SystemAdmin, Target::Global));
        assert!(set.allows(
            Permission::ServicesDeploy,
            Target::Environment {
                project: p,
                environment: e
            }
        ));
    }

    #[test]
    fn project_scope_covers_its_environments_only() {
        let (p, e, other) = ids();
        let set = GrantSet::new(vec![Grant {
            permission: Permission::ServicesDeploy,
            scope: Scope::Project { project_id: p },
        }]);
        assert!(set.allows(Permission::ServicesDeploy, Target::Project(p)));
        assert!(set.allows(
            Permission::ServicesDeploy,
            Target::Environment {
                project: p,
                environment: e
            }
        ));
        assert!(!set.allows(Permission::ServicesDeploy, Target::Project(other)));
        assert!(!set.allows(Permission::ServicesDeploy, Target::Global));
        assert!(!set.allows(Permission::ServicesDelete, Target::Project(p)));
    }

    #[test]
    fn environment_scope_does_not_reach_siblings_or_the_project() {
        let (p, e, sibling) = ids();
        let set = GrantSet::new(vec![Grant {
            permission: Permission::ServicesRead,
            scope: Scope::Environment {
                project_id: p,
                environment_id: e,
            },
        }]);
        assert!(set.allows(
            Permission::ServicesRead,
            Target::Environment {
                project: p,
                environment: e
            }
        ));
        assert!(!set.allows(
            Permission::ServicesRead,
            Target::Environment {
                project: p,
                environment: sibling
            }
        ));
        assert!(!set.allows(Permission::ServicesRead, Target::Project(p)));
        assert!(set.allows_within(Permission::ServicesRead, p));
        assert!(!set.allows_within(Permission::ServicesRead, sibling));
    }

    #[test]
    fn global_only_permissions_need_global_grants() {
        let (p, _, _) = ids();
        let set = GrantSet::new(vec![Grant {
            permission: Permission::SystemAdmin,
            scope: Scope::Project { project_id: p },
        }]);
        assert!(!set.allows(Permission::SystemAdmin, Target::Global));
        assert!(!set.allows(Permission::SystemAdmin, Target::Project(p)));
        assert!(!set.allows_within(Permission::SystemAdmin, p));
    }

    #[test]
    fn api_keys_never_exceed_their_owner() {
        let (p, e, _) = ids();
        let target = Target::Environment {
            project: p,
            environment: e,
        };
        let owner = GrantSet::new(vec![Grant {
            permission: Permission::ServicesRead,
            scope: Scope::Global,
        }]);
        // The key asks for more than its owner holds.
        let key = GrantSet::new(vec![
            Grant {
                permission: Permission::ServicesRead,
                scope: Scope::Project { project_id: p },
            },
            Grant {
                permission: Permission::ServicesDeploy,
                scope: Scope::Global,
            },
        ]);
        let authority = Authority {
            user: owner,
            key: Some(key),
        };
        assert!(authority.allows(Permission::ServicesRead, target));
        assert!(!authority.allows(Permission::ServicesDeploy, target));
        // The owner reads globally, the key only inside `p`.
        assert!(!authority.allows(Permission::ServicesRead, Target::Project(Uuid::new_v4())));

        // A superuser's key is still limited to the key's grants.
        let authority = Authority {
            user: GrantSet::superuser(),
            key: Some(GrantSet::new(vec![Grant {
                permission: Permission::ServicesDeploy,
                scope: Scope::Project { project_id: p },
            }])),
        };
        assert!(authority.allows(Permission::ServicesDeploy, target));
        assert!(!authority.allows(Permission::SystemAdmin, Target::Global));
        assert!(!authority.allows(Permission::ServicesDelete, target));
    }

    #[test]
    fn key_grants_are_validated() {
        let (p, _, _) = ids();
        assert!(validate_key_grants(&[]).is_err());
        assert!(validate_key_grants(&[Grant {
            permission: Permission::AccessManage,
            scope: Scope::Global,
        }])
        .is_err());
        assert!(validate_key_grants(&[Grant {
            permission: Permission::ServersManage,
            scope: Scope::Project { project_id: p },
        }])
        .is_err());
        assert!(validate_key_grants(&[Grant {
            permission: Permission::ServicesDeploy,
            scope: Scope::Project { project_id: p },
        }])
        .is_ok());
    }

    #[test]
    fn builtin_roles_have_valid_names_and_scopable_permissions() {
        for role in BUILTIN_ROLES {
            assert!(is_valid_role_name(role.name), "{}", role.name);
            if role.name != "admin" {
                assert!(role.permissions.iter().all(|p| p.is_scopable()));
            }
        }
        assert!(!is_valid_role_name("Admin"));
        assert!(!is_valid_role_name("-x"));
        assert!(!is_valid_role_name("a"));
    }

    #[test]
    fn scope_json_is_tagged() {
        let p = Uuid::nil();
        let json = serde_json::to_value(Scope::Project { project_id: p }).unwrap();
        assert_eq!(json["kind"], "project");
        let grant: Grant = serde_json::from_value(serde_json::json!({
            "permission": "services:deploy",
            "scope": { "kind": "global" }
        }))
        .unwrap();
        assert_eq!(grant.permission, Permission::ServicesDeploy);
        assert!(serde_json::from_value::<Grant>(serde_json::json!({
            "permission": "services:everything",
            "scope": { "kind": "global" }
        }))
        .is_err());
    }
}

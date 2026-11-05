//! The vetted template catalogue: popular applications shipped with the
//! panel as TOML metadata plus a Compose file.
//!
//! A template's Compose file never contains secrets. It references
//! `${NAME}` variables that [`generate_variables`] fills with random values
//! (or user input); the result goes to the project's write-only environment,
//! which Compose interpolates from `--env-file`. The Compose text is
//! deployed through the same validator as any user-supplied Compose file.

use crate::types::validate;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Where a template's variable value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generator {
    /// Random alphanumeric string of `length` characters.
    Password,
    /// Random lowercase hex string of `length` characters.
    Hex,
    /// Random standard base64 of `length` random bytes.
    Base64,
    /// Random version-4 UUID.
    Uuid,
    /// Supplied by the user at install time (`default` when omitted).
    Input,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateVariable {
    pub name: String,
    #[serde(default)]
    pub label: String,
    pub generator: Generator,
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedDomain {
    pub service: String,
    pub port: u16,
}

/// Parsed `<id>.toml`. `compose` names the Compose file of the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub compose: String,
    #[serde(default)]
    pub suggested_domain: Option<SuggestedDomain>,
    #[serde(default)]
    pub variables: Vec<TemplateVariable>,
}

#[derive(Debug, Clone)]
pub struct Template {
    pub meta: TemplateMeta,
    pub compose_yaml: String,
}

const MAX_GENERATED_LENGTH: usize = 128;

impl Template {
    /// Parses and validates a template. Every `${NAME}` the Compose file
    /// uses must be declared, and every declared variable must be used.
    pub fn parse(meta_toml: &str, compose_yaml: &str) -> Result<Template, String> {
        let meta: TemplateMeta =
            toml::from_str(meta_toml).map_err(|e| format!("invalid template metadata: {e}"))?;
        if !validate::is_valid_project_slug(&meta.id) {
            return Err(format!("invalid template id: {}", meta.id));
        }
        if meta.name.trim().is_empty() || meta.description.trim().is_empty() {
            return Err(format!("template {} needs a name and description", meta.id));
        }
        if compose_yaml.trim().is_empty() || compose_yaml.len() > 512 * 1024 {
            return Err(format!("template {} has an invalid Compose file", meta.id));
        }
        if let Some(domain) = &meta.suggested_domain {
            if !validate::is_valid_service_name(&domain.service) || domain.port == 0 {
                return Err(format!(
                    "template {} has an invalid suggested domain",
                    meta.id
                ));
            }
        }
        let mut declared = BTreeSet::new();
        for variable in &meta.variables {
            if !validate::is_valid_env_key(&variable.name) {
                return Err(format!("invalid variable name: {}", variable.name));
            }
            if !declared.insert(variable.name.as_str()) {
                return Err(format!("duplicate variable: {}", variable.name));
            }
            match variable.generator {
                Generator::Password | Generator::Hex | Generator::Base64 => {
                    let length = variable.length.unwrap_or(0);
                    if !(8..=MAX_GENERATED_LENGTH).contains(&length) {
                        return Err(format!(
                            "variable {} needs a length of 8..={MAX_GENERATED_LENGTH}",
                            variable.name
                        ));
                    }
                }
                Generator::Uuid | Generator::Input => {}
            }
        }
        let used = referenced_variables(compose_yaml)?;
        for name in &used {
            if !declared.contains(name.as_str()) {
                return Err(format!("Compose file uses undeclared variable {name}"));
            }
        }
        for name in &declared {
            if !used.contains(*name) {
                return Err(format!("variable {name} is never used"));
            }
        }
        Ok(Template {
            meta,
            compose_yaml: compose_yaml.to_string(),
        })
    }
}

/// Names referenced as `${NAME}` (plain form only; `$$` escapes a dollar).
/// Other `${...}` forms, such as defaults, are rejected so templates stay
/// predictable.
pub fn referenced_variables(compose: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let bytes = compose.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            match bytes.get(i + 1) {
                Some(b'$') => i += 2,
                Some(b'{') => {
                    let end = compose[i + 2..]
                        .find('}')
                        .ok_or("unterminated ${ in Compose file")?;
                    let name = &compose[i + 2..i + 2 + end];
                    if !validate::is_valid_env_key(name) {
                        return Err(format!("unsupported variable reference: ${{{name}}}"));
                    }
                    names.insert(name.to_string());
                    i += end + 3;
                }
                _ => return Err("bare `$` in Compose file; write `$$` or `${NAME}`".into()),
            }
        } else {
            i += 1;
        }
    }
    Ok(names)
}

const ALPHANUMERIC: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; n];
    OsRng.fill_bytes(&mut buffer);
    buffer
}

/// Uniform alphanumeric string (rejection sampling avoids modulo bias).
pub fn generate_password(length: usize) -> String {
    let limit = 256 - (256 % ALPHANUMERIC.len());
    let mut out = String::with_capacity(length);
    while out.len() < length {
        for byte in random_bytes(length * 2) {
            if (byte as usize) < limit && out.len() < length {
                out.push(ALPHANUMERIC[byte as usize % ALPHANUMERIC.len()] as char);
            }
        }
    }
    out
}

pub fn generate_hex(length: usize) -> String {
    let mut out = String::with_capacity(length + 1);
    for byte in random_bytes(length.div_ceil(2)) {
        out.push_str(&format!("{byte:02x}"));
    }
    out.truncate(length);
    out
}

pub fn generate_base64(byte_length: usize) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let data = random_bytes(byte_length);
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn generate_uuid() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Produces the environment for one installation: generated values for
/// generator variables and validated `inputs` (or defaults) for `input`
/// ones. Inputs naming no input variable are rejected, so a caller cannot
/// override a generated secret with a known value.
pub fn generate_variables(
    template: &Template,
    inputs: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    for key in inputs.keys() {
        let ok = template
            .meta
            .variables
            .iter()
            .any(|v| &v.name == key && v.generator == Generator::Input);
        if !ok {
            return Err(format!("{key} is not an input variable of this template"));
        }
    }
    let mut env = Vec::new();
    for variable in &template.meta.variables {
        let length = variable.length.unwrap_or(0);
        let value = match variable.generator {
            Generator::Password => generate_password(length),
            Generator::Hex => generate_hex(length),
            Generator::Base64 => generate_base64(length),
            Generator::Uuid => generate_uuid(),
            Generator::Input => {
                let value = inputs
                    .get(&variable.name)
                    .cloned()
                    .or_else(|| variable.default.clone())
                    .ok_or_else(|| format!("{} is required", variable.name))?;
                if value.is_empty() || value.len() > 1024 || value.chars().any(|c| c.is_control()) {
                    return Err(format!("invalid value for {}", variable.name));
                }
                value
            }
        };
        env.push((variable.name.clone(), value));
    }
    Ok(env)
}

macro_rules! entry {
    ($id:literal) => {
        (
            include_str!(concat!("../templates/", $id, ".toml")),
            include_str!(concat!("../templates/", $id, ".yml")),
        )
    };
}

const CATALOGUE: &[(&str, &str)] = &[
    entry!("wordpress"),
    entry!("ghost"),
    entry!("directus"),
    entry!("plausible"),
    entry!("uptime-kuma"),
];

/// The templates shipped with the panel. Every entry is validated by tests,
/// so a malformed one cannot ship.
pub fn catalogue() -> Vec<Template> {
    CATALOGUE
        .iter()
        .map(|(meta, compose)| Template::parse(meta, compose).expect("shipped template is valid"))
        .collect()
}

pub fn find(id: &str) -> Option<Template> {
    catalogue().into_iter().find(|t| t.meta.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"
id = "demo"
name = "Demo"
description = "d"
category = "x"
compose = "demo.yml"
[[variables]]
name = "PW"
generator = "password"
length = 20
[[variables]]
name = "HOST"
generator = "input"
default = "localhost"
"#;
    const YML: &str =
        "services:\n  a:\n    image: x\n    environment:\n      P: ${PW}\n      H: ${HOST}\n";

    #[test]
    fn shipped_catalogue_parses() {
        let all = catalogue();
        assert!(all.len() >= 5);
        let ids: BTreeSet<_> = all.iter().map(|t| t.meta.id.clone()).collect();
        assert_eq!(ids.len(), all.len());
    }

    #[test]
    fn generators_have_expected_shape() {
        let p = generate_password(40);
        assert_eq!(p.len(), 40);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(p, generate_password(40));
        let h = generate_hex(33);
        assert_eq!(h.len(), 33);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(generate_base64(32).len(), 44);
        assert_eq!(generate_base64(5).len(), 8);
        let u = generate_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }

    #[test]
    fn variables_are_generated_and_inputs_validated() {
        let t = Template::parse(META, YML).unwrap();
        let env = generate_variables(&t, &BTreeMap::new()).unwrap();
        assert_eq!(env[0].1.len(), 20);
        assert_eq!(env[1], ("HOST".into(), "localhost".into()));
        let mut inputs = BTreeMap::new();
        inputs.insert("HOST".to_string(), "blog.example.com".to_string());
        assert_eq!(
            generate_variables(&t, &inputs).unwrap()[1].1,
            "blog.example.com"
        );
        // A generated secret cannot be overridden through inputs.
        inputs.insert("PW".to_string(), "known".to_string());
        assert!(generate_variables(&t, &inputs).is_err());
        let mut bad = BTreeMap::new();
        bad.insert("HOST".to_string(), "a\nb".to_string());
        assert!(generate_variables(&t, &bad).is_err());
    }

    #[test]
    fn parser_rejects_inconsistent_templates() {
        assert!(Template::parse(META, "services:\n  a:\n    image: ${NOPE}\n").is_err());
        assert!(Template::parse(META, "services:\n  a:\n    image: ${PW}\n").is_err()); // HOST unused
        assert!(Template::parse(META, "services:\n  a:\n    image: ${PW:-x}\n").is_err());
        assert!(Template::parse(META, "x: $PW\n").is_err());
        let short = META.replace("length = 20", "length = 2");
        assert!(Template::parse(&short, YML).is_err());
        assert!(Template::parse("id = \"A b\"", YML).is_err());
        assert_eq!(referenced_variables("a: $$X ${Y}").unwrap().len(), 1);
    }
}

//! spin.toml v2 subset: source, route, allowed_outbound_hosts, key_value_stores, variables.
use serde::Deserialize;
use std::collections::BTreeMap as Map;

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Source {
    Local(String),
    #[allow(dead_code)] Remote { url: String, digest: String },
    #[allow(dead_code)] Registry { registry: String, package: String, version: String },
}
#[derive(Deserialize, Default)]
pub struct Component {
    pub source: Option<Source>,
    #[serde(default)] pub allowed_outbound_hosts: Vec<String>,
    #[serde(default)] pub key_value_stores: Vec<String>,
    #[serde(default)] pub variables: Map<String, String>,
    #[serde(default)] pub environment: Map<String, String>,
}
#[derive(Deserialize)]
pub struct Var { pub default: Option<String>, #[serde(default)] pub required: bool }
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Spec { Ref(String), Inline(Box<Component>) }
#[derive(Deserialize)]
pub struct HttpTrigger { pub route: toml::Value, pub component: Spec }
#[derive(Deserialize)]
pub struct Manifest {
    #[serde(default)] pub variables: Map<String, Var>,
    #[serde(default)] pub trigger: Map<String, Vec<toml::Table>>,
    #[serde(default)] pub component: Map<String, Component>,
}

impl Manifest {
    /// `{{ var }}` expansion; value = env SPIN_VARIABLE_<NAME> or the manifest default.
    pub fn expand(&self, tpl: &str) -> wasmtime::Result<String> {
        let (mut out, mut rest) = (String::new(), tpl);
        while let Some((pre, tail)) = rest.split_once("{{") {
            let (name, after) = tail.split_once("}}").ok_or_else(|| wasmtime::format_err!("unclosed {{{{ in {tpl}"))?;
            let name = name.trim();
            let v = std::env::var(format!("SPIN_VARIABLE_{}", name.to_uppercase())).ok()
                .or_else(|| self.variables.get(name)?.default.clone())
                .ok_or_else(|| wasmtime::format_err!("variable `{name}` is required"))?;
            out += pre; out += &v; rest = after;
        }
        Ok(out + rest)
    }
}

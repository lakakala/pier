use crate::{
    AgentRecord, Binding, Controller, ResolvedBinding,
    api::{ApiResult, bad, conflict, missing},
    catalog::{Blueprint, variable_name},
};
use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
};
use pier_protocol::Variables;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const NAMESPACE: &str = "global_variables";

/// Strings remain literal, including strings containing template expressions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum BindingValue {
    Literal(String),
    Reference { r#ref: String },
}
impl BindingValue {
    pub(crate) fn reference(&self) -> Option<&str> {
        match self {
            Self::Literal(_) => None,
            Self::Reference { r#ref } => Some(r#ref),
        }
    }
}
impl From<&str> for BindingValue {
    fn from(value: &str) -> Self {
        Self::Literal(value.into())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Variable {
    name: String,
    value: String,
}
#[derive(Serialize)]
struct Reference {
    agent_id: String,
    agent_name: String,
    blueprint: String,
    variable: String,
}

impl Controller {
    // Call under mutation_lock so reference validation and snapshot capture are
    // atomic with variable changes, deletion and binding updates.
    pub(crate) fn resolve_binding(
        &self,
        binding: &Binding,
        blueprint: &Blueprint,
    ) -> Result<ResolvedBinding> {
        let mut values = Variables::new();
        for (name, source) in &binding.variables {
            ensure!(
                blueprint.variables.contains_key(name),
                "unknown blueprint variable: {name}"
            );
            let value = match source {
                BindingValue::Literal(value) => value.clone(),
                BindingValue::Reference { r#ref } => {
                    ensure!(
                        variable_name(r#ref),
                        "invalid global variable reference for {name}"
                    );
                    self.store
                        .get::<Variable>(NAMESPACE, r#ref)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "blueprint variable {name} references missing global variable: {}",
                                r#ref
                            )
                        })?
                        .value
                }
            };
            values.insert(name.clone(), value);
        }
        Ok(ResolvedBinding {
            blueprint: binding.blueprint.clone(),
            variables: blueprint.resolve(&values)?,
        })
    }
}

fn references(state: &Controller) -> Result<BTreeMap<String, Vec<Reference>>> {
    let mut result: BTreeMap<String, Vec<Reference>> = BTreeMap::new();
    for agent in state.store.list::<AgentRecord>("agents")? {
        for binding in state.bindings(&agent.id)?.values() {
            for (name, value) in &binding.variables {
                if let Some(reference) = value.reference() {
                    result.entry(reference.into()).or_default().push(Reference {
                        agent_id: agent.id.clone(),
                        agent_name: agent.name.clone(),
                        blueprint: binding.blueprint.clone(),
                        variable: name.clone(),
                    });
                }
            }
        }
    }
    Ok(result)
}
fn public_variable(variable: Variable, references: Vec<Reference>) -> Value {
    json!({"name":variable.name,"value":variable.value,"references":references})
}
pub(crate) async fn list(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    let mut references = references(&state)?;
    let variables = state
        .store
        .list::<Variable>(NAMESPACE)?
        .into_iter()
        .map(|variable| {
            let refs = references.remove(&variable.name).unwrap_or_default();
            public_variable(variable, refs)
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"variables":variables})))
}
pub(crate) async fn create(
    State(state): State<Arc<Controller>>,
    Json(variable): Json<Variable>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    if !variable_name(&variable.name) {
        return Err(bad("invalid or reserved global variable name"));
    }
    if state
        .store
        .get::<Variable>(NAMESPACE, &variable.name)?
        .is_some()
    {
        return Err(conflict("global variable already exists"));
    }
    state.store.put(NAMESPACE, &variable.name, &variable)?;
    Ok(Json(public_variable(variable, Vec::new())))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    value: String,
}
pub(crate) async fn update(
    State(state): State<Arc<Controller>>,
    Path(name): Path<String>,
    Json(update): Json<Update>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    let mut variable = state
        .store
        .get::<Variable>(NAMESPACE, &name)?
        .ok_or_else(missing)?;
    variable.value = update.value;
    state.store.put(NAMESPACE, &name, &variable)?;
    let refs = references(&state)?.remove(&name).unwrap_or_default();
    Ok(Json(public_variable(variable, refs)))
}
pub(crate) async fn delete(
    State(state): State<Arc<Controller>>,
    Path(name): Path<String>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    if state.store.get::<Variable>(NAMESPACE, &name)?.is_none() {
        return Err(missing());
    }
    if references(&state)?.contains_key(&name) {
        return Err(conflict(
            "global variable is referenced; remove its binding references before deleting",
        ));
    }
    state.store.delete(NAMESPACE, &name)?;
    Ok(Json(json!({"removed":true})))
}

#[cfg(test)]
mod tests;

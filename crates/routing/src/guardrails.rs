use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use phoebis_guardrails::CompiledGuardrail;
use phoebis_store::{AttachmentSubject, GuardrailAttachment, RegistryLoad};

/// The compiled guardrails of one load: each key's list in evaluation order and the name of every row, attached or
/// not, enabled or not, so a count over a guardrail since detached still reads its name.
#[derive(Clone, Debug, Default)]
pub struct CompiledGuardrails {
    by_key: HashMap<String, Arc<[Arc<CompiledGuardrail>]>>,
    names: HashMap<String, String>,
}

impl CompiledGuardrails {
    /// Each key's list is its pool's attachments in position order, then its own, a guardrail on both once at its
    /// pool position. A disabled row and one that does not compile run for no key.
    pub fn build(load: &RegistryLoad) -> CompiledGuardrails {
        let mut names = HashMap::with_capacity(load.guardrails.len());
        let mut compiled: HashMap<&str, Arc<CompiledGuardrail>> = HashMap::new();
        for row in &load.guardrails {
            names.insert(row.id.clone(), row.name.clone());
            if !row.enabled {
                continue;
            }
            match CompiledGuardrail::compile(
                &row.id,
                &row.name,
                row.mode,
                row.action,
                &row.conditions,
            ) {
                Ok(guardrail) => {
                    compiled.insert(&row.id, Arc::new(guardrail));
                }
                // The admin route refuses such a row, but one written around it must not stop the registry
                // from loading.
                Err(error) => tracing::warn!(
                    guardrail = %row.id,
                    %error,
                    "the guardrail does not compile, so no key runs it"
                ),
            }
        }

        let mut by_pool: HashMap<&str, Vec<&GuardrailAttachment>> = HashMap::new();
        let mut by_own_key: HashMap<&str, Vec<&GuardrailAttachment>> = HashMap::new();
        for attachment in &load.guardrail_attachments {
            let lists = match &attachment.subject {
                AttachmentSubject::Key(id) => by_own_key.entry(id),
                AttachmentSubject::Pool(id) => by_pool.entry(id),
            };
            lists.or_default().push(attachment);
        }
        for list in by_pool.values_mut().chain(by_own_key.values_mut()) {
            list.sort_by_key(|attachment| attachment.position);
        }

        let mut by_key = HashMap::new();
        for key in &load.keys {
            let attached = by_pool
                .get(key.pool_id.as_str())
                .into_iter()
                .chain(by_own_key.get(key.id.as_str()))
                .flatten();
            let mut seen = HashSet::new();
            let list: Vec<Arc<CompiledGuardrail>> = attached
                .filter(|attachment| seen.insert(attachment.guardrail_id.as_str()))
                .filter_map(|attachment| compiled.get(attachment.guardrail_id.as_str()))
                .cloned()
                .collect();
            if !list.is_empty() {
                by_key.insert(key.id.clone(), Arc::from(list));
            }
        }
        CompiledGuardrails { by_key, names }
    }

    /// An empty slice for a key the build gave no entry.
    pub fn for_key(&self, key_id: &str) -> &[Arc<CompiledGuardrail>] {
        self.by_key.get(key_id).map(|list| &list[..]).unwrap_or(&[])
    }

    pub fn name_of(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }
}

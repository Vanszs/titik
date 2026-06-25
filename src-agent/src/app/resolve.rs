//! Per-role route resolution: the single chokepoint that turns a [`ModelRole`]
//! into one concrete [`Resolved`] route (model id + endpoint + key + wire type +
//! optional upstream pin + effort).
//!
//! The runtime has four model-driven roles — Main (interactive chat), Awareness
//! (project-doc summary), Safeguard (the safety classifier), and Compactor
//! (`/compact`, which rides Main today). Each is assigned a model via the global
//! catalogue (`config.models`) or a per-session override (`settings.session_models`);
//! that model points at a provider connection (`config.providers`) by uuid, which
//! carries the endpoint + key + wire type. [`resolve_role`] walks that chain and
//! produces the route the call site hands to the client.
//!
//! ## Resolution order
//!
//! 1. Find the model assigned to `role`: session overrides first
//!    (`settings.session_models`), then the global catalogue (`config.models`).
//! 2. Resolve that model's provider by `provider_uuid` against `config.providers`.
//!    A hit produces the [`Resolved`] route directly.
//! 3. If no model is assigned, OR the assigned model's `provider_uuid` does not
//!    resolve to a known provider, fall through to the per-role LEGACY fallback —
//!    the old per-field `settings.*` behaviour, so an empty/old config never
//!    bricks chat (Main/Compactor/Awareness) and the safeguard fails CLOSED.
//!
//! ## Fallback table (when step 2 finds no provider)
//!
//! | role      | fallback                                                            |
//! |-----------|---------------------------------------------------------------------|
//! | Main      | legacy `settings.model` / `api_key` @ `DEFAULT_BASE_URL`            |
//! | Compactor | resolve Main (compactor rides Main; no config slot of its own)      |
//! | Awareness | `awareness_inherit` → resolve Main; else legacy `awareness_model`  |
//! | Safeguard | legacy `classifier_model` if set; else `None` (FAIL-CLOSED)        |
//!
//! ## Foot-gun (do not regress)
//!
//! `awareness_inherit` is ONLY consulted in the Awareness *fallback*. An Awareness
//! model that is explicitly assigned (found in step 1 and whose provider resolves
//! in step 2) ALWAYS wins over `awareness_inherit` — inherit is the behaviour when
//! nothing is assigned, never an override of an assignment.

use crate::config::DEFAULT_BASE_URL;
use crate::model::app_config::{ApiType, AppConfig, ModelEntry, ModelRole};
use crate::model::settings::Settings;
use crate::service::openrouter::Conn;

/// One fully-resolved route for a runtime role: everything a client call needs to
/// reach the right model on the right provider, with no further config lookups.
///
/// `route` is the OpenRouter upstream-provider pin (`None` = automatic routing).
/// `effort` carries the reasoning-effort token and is only ever non-empty for the
/// Main role (the only role that exposes effort today); every other role resolves
/// it to `String::new()`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub model_id: String,
    pub endpoint: String,
    pub api_key: String,
    // Carried by the resolver but not consumed on the request path YET: every
    // method still emits the OpenAI-compatible wire body. The Anthropic body
    // builder will branch on this (forward-compat; flagged as a known gap).
    #[allow(dead_code)]
    pub api_type: ApiType,
    pub route: Option<String>,
    // Consumed only by the MAIN streaming path, which still reads the client's
    // baked `self.effort` this stage (the chat path is refactored next stage).
    // Non-empty only for the Main role; every other role resolves it to "".
    #[allow(dead_code)]
    pub effort: String,
}

impl Resolved {
    /// Borrow this route's `endpoint` + `api_key` as a [`Conn`] for a secondary
    /// client call (the connection part of the route — "where + auth").
    pub fn conn(&self) -> Conn<'_> {
        Conn {
            endpoint: &self.endpoint,
            api_key: &self.api_key,
        }
    }

    /// The OpenRouter upstream-provider pin as a slug (`""` = automatic routing),
    /// the form the client's `provider_routing_for` expects.
    pub fn provider(&self) -> &str {
        self.route.as_deref().unwrap_or("")
    }
}

/// Build a [`Resolved`] from an assigned [`ModelEntry`] by resolving its provider
/// against `config.providers`. Returns `None` when the entry's `provider_uuid`
/// does not match any known provider (a dangling reference) — the caller treats
/// that the same as "no assignment" and falls through to the legacy fallback.
///
/// `effort` is taken from `settings.effort` only for the Main role; every other
/// role gets an empty effort.
fn from_entry(config: &AppConfig, settings: &Settings, entry: &ModelEntry, role: ModelRole) -> Option<Resolved> {
    let provider = config
        .providers
        .iter()
        .find(|p| p.uuid == entry.provider_uuid)?;
    let effort = if role == ModelRole::Main {
        settings.effort.clone()
    } else {
        String::new()
    };
    Some(Resolved {
        model_id: entry.model_id.clone(),
        endpoint: provider.endpoint.clone(),
        api_key: provider.api_key.clone(),
        api_type: provider.api_type,
        route: entry.route.clone(),
        effort,
    })
}

/// The universal Main soft-fallback: a [`Resolved`] built from the OLD per-field
/// `settings` (api_key + model + provider + effort @ [`DEFAULT_BASE_URL`], the
/// OpenAI-compatible wire). Keeps chat alive when `config` is empty/old or an
/// assigned Main provider is missing, exactly preserving today's behaviour.
fn legacy_main(settings: &Settings) -> Resolved {
    Resolved {
        model_id: settings.model.clone(),
        endpoint: DEFAULT_BASE_URL.to_string(),
        api_key: settings.api_key.clone(),
        api_type: ApiType::OpenAiCompatible,
        route: None,
        effort: settings.effort.clone(),
    }
}

/// Per-role fallback used when no model is assigned to `role`, or the assigned
/// model's provider is dangling. Encodes the table in this module's docs:
/// Main/Compactor soft-fall-back to the legacy Main route; Awareness inherits Main
/// (when `awareness_inherit`) else uses the legacy awareness fields; Safeguard
/// uses the legacy classifier field if set, else fails CLOSED (`None`).
fn legacy_fallback(settings: &Settings, role: ModelRole) -> Option<Resolved> {
    match role {
        // Chat is the product — Main must never go dark.
        ModelRole::Main => Some(legacy_main(settings)),
        // Compactor has never had its own config; it rides Main.
        ModelRole::Compactor => Some(legacy_main(settings)),
        ModelRole::Awareness => {
            if settings.awareness_inherit {
                // Inherit the Main route — only in the no-assignment fallback.
                Some(legacy_main(settings))
            } else {
                // Legacy dedicated awareness fields @ DEFAULT_BASE_URL, no effort.
                Some(Resolved {
                    model_id: settings.awareness_model.clone(),
                    endpoint: DEFAULT_BASE_URL.to_string(),
                    api_key: settings.api_key.clone(),
                    api_type: ApiType::OpenAiCompatible,
                    route: None,
                    effort: String::new(),
                })
            }
        }
        ModelRole::Safeguard => {
            // FAIL-CLOSED: only the legacy classifier model rescues it; an empty
            // field yields `None`, which the harness caller degrades to a human
            // prompt (TAC) / advisory toast (PC) rather than silently allowing.
            if settings.classifier_model.is_empty() {
                None
            } else {
                Some(Resolved {
                    model_id: settings.classifier_model.clone(),
                    endpoint: DEFAULT_BASE_URL.to_string(),
                    api_key: settings.api_key.clone(),
                    api_type: ApiType::OpenAiCompatible,
                    route: None,
                    effort: String::new(),
                })
            }
        }
    }
}

/// Resolve the concrete route for `role`.
///
/// Session overrides (`settings.session_models`) win over the global catalogue
/// (`config.models`); the chosen model's provider is resolved by uuid against
/// `config.providers`. A successful resolution returns the assigned route. When no
/// model is assigned, or the assigned model's provider is dangling, the per-role
/// legacy fallback applies (see [`legacy_fallback`]). Returns `None` only for an
/// unresolved Safeguard (fail-closed); every other role always resolves to
/// `Some`.
pub fn resolve_role(config: &AppConfig, settings: &Settings, role: ModelRole) -> Option<Resolved> {
    // 1. Pick the assigned model: per-session overrides first, then the global
    //    catalogue. (`role` is Copy; both finds compare against `Some(role)`.)
    let assigned = settings
        .session_models
        .iter()
        .find(|e| e.role == Some(role))
        .or_else(|| config.models.iter().find(|e| e.role == Some(role)));

    // 2. If a model is assigned AND its provider resolves, that route wins —
    //    including an explicitly-assigned Awareness model, which beats
    //    `awareness_inherit` (the inherit branch lives in the fallback only).
    if let Some(entry) = assigned {
        if let Some(resolved) = from_entry(config, settings, entry, role) {
            return Some(resolved);
        }
        // Assigned but the provider_uuid is dangling → fall through to legacy.
    }

    // 3. No assignment, or a dangling provider → per-role legacy fallback.
    legacy_fallback(settings, role)
}

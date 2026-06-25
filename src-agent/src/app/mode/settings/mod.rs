//! Settings-mode types: the field schema, category layout, path-list picker,
//! and the main [`SettingsState`] draft holder.
//!
//! Adding a new category or field to [`SETTING_CATEGORIES`] is sufficient — the
//! view and input handler iterate over it generically.

mod picker;
mod state;

pub use picker::PICKER_MAX;
pub use state::SettingsState;

/// The persisted catalogue enums double as the UI draft enums — the variants are
/// identical and re-using them avoids a second enum + conversion glue. The
/// inherent `impl` blocks below (label/short_label/full_label/toggle/ALL) attach
/// the UI-only helpers to these re-exported in-crate types.
///
/// `ModelRole`: role slot a model is assigned to in the agent runtime. A model
/// may hold SEVERAL roles, but each role is globally exclusive — assigning a role
/// to one model steals it from any other model that currently holds it. An empty
/// role list means unassigned.
///
/// `ApiType`: the wire protocol type of an API provider endpoint.
pub use crate::model::app_config::{ApiType, ModelRole};

/// A single navigable field within the add/edit-model modal.
///
/// The concrete field SET is computed at runtime by
/// [`SettingsState::model_modal_fields`] because two fields are conditional:
/// `Route` appears only for an OpenRouter provider with a model selected, and
/// `Role` appears only in EDIT mode. The modal's `field` index addresses into
/// that computed `Vec<ModelField>`, so there are no hardcoded layout indices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelField {
    Name,
    Provider,
    Model,
    Route,
    Role,
    Save,
    SaveSession,
    Cancel,
}

impl ModelRole {
    pub fn label(&self) -> &'static str {
        match self {
            ModelRole::Main      => "main",
            ModelRole::Awareness => "awareness",
            ModelRole::Safeguard => "safeguard",
            ModelRole::Compactor => "compactor",
        }
    }

    pub const ALL: [ModelRole; 4] = [
        ModelRole::Main,
        ModelRole::Awareness,
        ModelRole::Safeguard,
        ModelRole::Compactor,
    ];
}

impl ApiType {
    /// Short label used in the providers table column.
    pub fn short_label(self) -> &'static str {
        match self {
            ApiType::OpenAiCompatible   => "OpenAI",
            ApiType::AnthropicCompatible => "Anthropic",
        }
    }

    /// Full human-readable label for the api type. The Anthropic variant is tagged
    /// "(not wired)" because native Anthropic is deferred (the type persists but no
    /// role routes to it yet — see [`ApiType::is_routable`]). Kept for
    /// forward-compat; the UI Type field was removed (new providers are always
    /// `OpenAiCompatible`).
    #[allow(dead_code)]
    pub fn full_label(self) -> &'static str {
        match self {
            ApiType::OpenAiCompatible   => "OpenAI compatible",
            ApiType::AnthropicCompatible => "Anthropic compatible (not wired)",
        }
    }

    /// Flip between the two variants. Kept for forward-compat; not called from
    /// the UI since the Type field was removed.
    #[allow(dead_code)]
    pub fn toggle(self) -> Self {
        match self {
            ApiType::OpenAiCompatible   => ApiType::AnthropicCompatible,
            ApiType::AnthropicCompatible => ApiType::OpenAiCompatible,
        }
    }
}

/// Mint a fresh random UUID (v4) as a `String`. Used when CREATING a new
/// provider/model draft in the UI so its identity is stable across the edit
/// session (before the first config save) and matches the persisted
/// [`crate::model::app_config`] uuid scheme.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// One API provider entry, mirrored to/from a persisted
/// [`crate::model::app_config::ProviderConn`]. `uuid` carries the persisted
/// identity so a reorder/delete/edit round-trips without losing the
/// model→provider linkage.
#[derive(Clone, Debug)]
pub struct ProviderDraft {
    /// Persisted identity (matches the `ProviderConn` uuid). Minted on create.
    pub uuid: String,
    pub name: String,
    pub endpoint: String,
    pub api_type: ApiType,
    pub api_key: String,
}

/// State for the "Add API provider" modal overlay.
#[derive(Clone, Debug)]
pub struct ProviderModal {
    pub name: String,
    pub endpoint: String,
    pub api_type: ApiType,
    pub api_key: String,
    /// Active field: 0=name, 1=endpoint, 2=api_key, 3=Save button, 4=Cancel button.
    pub field: usize,
}

impl ProviderModal {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            endpoint: String::new(),
            api_type: ApiType::OpenAiCompatible,
            api_key: String::new(),
            field: 0,
        }
    }
}

/// One model entry stored in-memory (stub only, not persisted). Maps a custom
/// display name to a concrete `model_id`, served by the API provider at
/// `provider_idx` in [`SettingsState::providers`].
#[derive(Clone, Debug)]
pub struct ModelDraft {
    /// Persisted identity (matches the `ModelEntry` uuid). Minted on create;
    /// preserved on edit so a saved model keeps its catalogue identity.
    pub uuid: String,
    /// Custom display name shown in the models table.
    pub name: String,
    /// Concrete model identifier, e.g. `"openai/gpt-4o-mini"`.
    pub model_id: String,
    /// Index into [`SettingsState::providers`] — which API provider serves it.
    pub provider_idx: usize,
    /// Role slots assigned to this model; empty = unassigned. A model may hold
    /// several roles (each role is globally unique across models).
    pub roles: Vec<ModelRole>,
    /// Pinned upstream provider for OpenRouter routing: the chosen endpoint's
    /// provider name. `None` = Auto (let OpenRouter route). Only meaningful for
    /// OpenRouter-served models; ignored for other providers.
    pub route: Option<String>,
    /// `true` = saved for this session only (not persisted globally);
    /// `false` = global scope. Stub flag — persistence is not yet implemented.
    #[allow(dead_code)]
    pub session_only: bool,
}

/// State for the "Add / Edit model" modal overlay.
///
/// When the chosen provider is OpenRouter and the Model field is focused, the
/// modal hosts a live omnisearch over the cached model catalogue (`query` +
/// `result_sel`). Selecting/opening an OpenRouter model arms the `endpoints*`
/// fields: a background fetch loads the model's provider endpoints, which the
/// view renders as a read-only providers list (price + uptime per provider).
///
/// The field layout is computed (never hardcoded): `field` indexes into the
/// `Vec<ModelField>` returned by [`SettingsState::model_modal_fields`]. The base
/// run is `Name, Provider, Model`; a `Route` field is inserted (OpenRouter +
/// model selected) and a `Role` field is inserted (EDIT mode); `Save, Cancel`
/// always close the list. Resolve the focused field with
/// [`SettingsState::mm_current_field`] instead of comparing raw indices.
#[derive(Clone, Debug)]
pub struct ModelModal {
    /// `Some(i)` = editing `models[i]`; `None` = adding a new entry.
    pub editing_idx: Option<usize>,
    /// Persisted identity carried through the modal: minted fresh in
    /// [`Self::new_add`], cloned from the edited draft on edit-open, and written
    /// back onto the saved [`ModelDraft`]. Empty falls back to a freshly minted
    /// uuid on save.
    pub uuid: String,
    /// Draft custom display name.
    pub name: String,
    /// Index into [`SettingsState::providers`] (which provider serves the model).
    pub provider_idx: usize,
    /// Draft concrete model id.
    pub model_id: String,
    /// Active field index (see layout comment above).
    pub field: usize,
    /// Draft role assignments; empty = unassigned. A model may hold several
    /// roles. Only editable in EDIT mode, via the Role chip multi-select.
    pub roles: Vec<ModelRole>,
    /// Which role chip the multi-select cursor sits on (`0..ModelRole::ALL.len()`).
    /// Used by the Role field to highlight + toggle the focused chip.
    pub role_cursor: usize,
    /// Model omnisearch query (used when provider is OpenRouter and the Model
    /// field is focused).
    pub query: String,
    /// Highlighted row in the omnisearch results list.
    pub result_sel: usize,
    /// Pinned upstream provider for OpenRouter routing: the chosen endpoint's
    /// provider name. `None` = Auto (let OpenRouter route). Mirrors
    /// [`ModelDraft::route`]; committed into the draft on save.
    pub route: Option<String>,
    /// Cursor into the Route options list (0 = Auto; 1..=N index `endpoints`).
    pub route_sel: usize,
    // --- endpoints area: per-model provider list (display only) ---
    /// Fetched per-model provider endpoints. `None` until a fetch resolves;
    /// `Some(vec)` once loaded (an empty vec means "no providers found", also
    /// used to resolve a failed fetch). Rendered by `draw_model_modal`.
    pub endpoints: Option<Vec<crate::dto::openrouter::ModelEndpoint>>,
    /// `true` while the endpoints fetch is in flight (the view shows "loading
    /// providers…"); cleared when the fetch resolves.
    pub endpoints_loading: bool,
    /// The model id the in-flight / cached `endpoints` belong to. Used as a
    /// stale-guard in the drain so a rapid re-selection can't show a previous
    /// model's providers.
    pub endpoints_for: Option<String>,
}

impl ModelModal {
    /// Blank ADD-mode modal targeting provider `provider_idx`. Mints a fresh
    /// uuid so a model added in this session has a stable identity before save.
    pub fn new_add(provider_idx: usize) -> Self {
        Self {
            editing_idx: None,
            uuid: new_uuid(),
            name: String::new(),
            provider_idx,
            model_id: String::new(),
            field: 0,
            roles: Vec::new(),
            role_cursor: 0,
            query: String::new(),
            result_sel: 0,
            route: None,
            route_sel: 0,
            endpoints: None,
            endpoints_loading: false,
            endpoints_for: None,
        }
    }

    /// `true` when this modal is in EDIT mode.
    pub fn is_edit(&self) -> bool {
        self.editing_idx.is_some()
    }
}

/// Filter the cached model catalogue by a (case-insensitive) substring match on
/// the model `id` or its human-readable `name`, returning up to 50 catalogue
/// indices. Used to drive the modal's model omnisearch.
pub fn filter_models(cache: &[crate::dto::openrouter::ModelInfo], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    cache
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.id.to_lowercase().contains(&q)
                || m.name
                    .as_deref()
                    .map(|n| n.to_lowercase().contains(&q))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .take(50)
        .collect()
}

/// A single editable/toggleable field within a settings category.
#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(dead_code)]
pub enum SettingField {
    ApiKey,
    Model,
    Provider,
    Theme,
    Accent,
    Name,
    Workdir,
    /// Toggle: whether the project-awareness summary is generated/injected.
    AwarenessEnabled,
    /// Toggle: awareness model source — inherit the session model or use the
    /// dedicated awareness model/provider.
    AwarenessSource,
    /// Text: dedicated awareness model (ignored when the source is "inherit").
    AwarenessModel,
    /// Text: dedicated awareness provider (ignored when the source is "inherit").
    AwarenessProvider,
    /// Toggle: master switch for the safety harness ("Pass B").
    ClassifierEnabled,
    /// Text: model used for the safety classifier.
    ClassifierModel,
    /// Text: provider slug (strict-pinned) for the safety classifier.
    ClassifierProvider,
    /// Text: extra allowed folders (comma-separated) for the workspace check.
    AllowedFolders,
    /// Toggle: master kill-switch for the short-send token saver.
    ShortSendEnabled,
    /// Toggle: cache-warmth-adaptive summarization. On only for models with a
    /// sliding/refreshing prompt cache (e.g. Anthropic).
    SlidingCache,
}

impl SettingField {
    /// Human-readable label shown in the detail pane.
    pub fn label(self) -> &'static str {
        match self {
            SettingField::ApiKey            => "API key",
            SettingField::Model             => "Model",
            SettingField::Provider          => "Provider",
            SettingField::Theme             => "Theme",
            SettingField::Accent            => "Accent",
            SettingField::Name              => "Session name",
            SettingField::Workdir           => "Workdir",
            SettingField::AwarenessEnabled  => "Awareness",
            SettingField::AwarenessSource   => "Model source",
            SettingField::AwarenessModel    => "Aware model",
            SettingField::AwarenessProvider => "Aware provider",
            SettingField::ClassifierEnabled  => "Harness",
            SettingField::ClassifierModel    => "Class. model",
            SettingField::ClassifierProvider => "Class. provider",
            SettingField::AllowedFolders     => "Allowed dirs",
            SettingField::ShortSendEnabled   => "Short-send",
            SettingField::SlidingCache       => "Sliding cache",
        }
    }
}

/// A named group of related settings fields shown in the sidebar.
pub struct SettingCategory {
    pub name: &'static str,
    pub group: &'static str,
    pub fields: &'static [SettingField],
}

/// All settings categories in sidebar display order.
///
/// Adding a new category or field here is sufficient — the view and input
/// handler iterate over this slice generically.
pub const SETTING_CATEGORIES: &[SettingCategory] = &[
    SettingCategory {
        name: "Appearance",
        group: "general",
        fields: &[SettingField::Theme, SettingField::Accent],
    },
    SettingCategory {
        name: "Session",
        group: "general",
        fields: &[SettingField::Name, SettingField::Workdir, SettingField::ShortSendEnabled, SettingField::SlidingCache],
    },
    SettingCategory {
        name: "API Providers",
        group: "models",
        fields: &[],
    },
    SettingCategory {
        name: "Models Select",
        group: "models",
        fields: &[],
    },
];

//! Settings-mode types: the field schema, category layout, path-list picker,
//! and the main [`SettingsState`] draft holder.
//!
//! Adding a new category or field to [`SETTING_CATEGORIES`] is sufficient — the
//! view and input handler iterate over it generically.

mod picker;
mod state;

pub use picker::PICKER_MAX;
pub use state::SettingsState;

/// A single editable/toggleable field within a settings category.
#[derive(Clone, Copy, PartialEq, Debug)]
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
    pub fields: &'static [SettingField],
}

/// All settings categories in sidebar display order.
///
/// Adding a new category or field here is sufficient — the view and input
/// handler iterate over this slice generically.
pub const SETTING_CATEGORIES: &[SettingCategory] = &[
    SettingCategory {
        name: "Connection",
        fields: &[SettingField::ApiKey, SettingField::Model, SettingField::Provider],
    },
    SettingCategory {
        name: "Appearance",
        fields: &[SettingField::Theme, SettingField::Accent],
    },
    SettingCategory {
        name: "Session",
        fields: &[SettingField::Name, SettingField::Workdir, SettingField::ShortSendEnabled, SettingField::SlidingCache],
    },
    SettingCategory {
        name: "Awareness",
        fields: &[
            SettingField::AwarenessEnabled,
            SettingField::AwarenessSource,
            SettingField::AwarenessModel,
            SettingField::AwarenessProvider,
        ],
    },
    SettingCategory {
        name: "Harness",
        fields: &[
            SettingField::ClassifierEnabled,
            SettingField::ClassifierModel,
            SettingField::ClassifierProvider,
            SettingField::AllowedFolders,
        ],
    },
];

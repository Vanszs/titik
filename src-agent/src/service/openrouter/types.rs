//! Shared types used across the openrouter submodules.

/// A resolved provider connection for a single request: the `endpoint` (base
/// URL) + `api_key` that used to be baked onto the client.
///
/// A cheap borrow (two `&str`, `Copy`), built fresh at the call site from the
/// role's resolved route. EVERY request method — the interactive chat
/// (`stream_complete`) and `/compact` (`complete`) included — takes its
/// endpoint+key through this value, so a role on a DIFFERENT provider/key Just
/// Works with no client rebuild (auth + URL are already pure string
/// interpolation on a header-less client; nothing is baked onto `self`).
#[derive(Clone, Copy, Debug)]
pub struct Conn<'a> {
    /// Base URL, e.g. `https://openrouter.ai/api/v1`. Was `self.base_url`.
    pub endpoint: &'a str,
    /// Bearer token for this connection. Was `self.api_key`.
    pub api_key: &'a str,
}

/// Derived reasoning capability for a single model, used to build the `/effort`
/// menu conditionally.
///
/// - `supported`: the model exposes any reasoning control at all.
/// - `mandatory`: reasoning can't be turned off (no "off" option offered).
/// - `efforts`: discrete effort tokens the model accepts, in the order the API
///   reported them; empty means on/off-only (or unreported).
pub struct EffortCaps {
    pub supported: bool,
    pub mandatory: bool,
    pub efforts: Vec<String>,
}

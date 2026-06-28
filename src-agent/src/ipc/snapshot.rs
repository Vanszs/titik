//! Pure render-state PROJECTION + DIFF for the daemon stage-4 streaming layer.
//!
//! Two pure functions, no runtime handles, no terminal, no channels:
//!
//! - [`build_snapshot`] reads the live [`AppState`] and copies out a frozen
//!   [`StateSnapshot`] (the [`super::proto`] projection): one [`SessionSnapshot`]
//!   per session + the foreground id + the [`GlobalSnapshot`]. It is the SINGLE
//!   source of truth for "what the client should render", so a client can never
//!   diverge from the daemon — it only ever renders this projection.
//! - [`diff`] compares a freshly-built snapshot against the previously-sent one and
//!   yields the minimal set of [`StateDelta`]s for the high-frequency per-tick
//!   changes, OR signals (`needs_full`) that a STRUCTURAL change happened that is
//!   not worth diffing incrementally (session added/removed, history changed,
//!   tokens/approval/subagents shifted) — in which case the caller resends a full
//!   [`StateSnapshot`] instead. Correctness-first (daemon stage 4): when in doubt,
//!   ask for a full snapshot; a full snapshot is ALWAYS a valid update.
//!
//! Keeping this PURE (a function of `&AppState`, not a method that also drives the
//! socket) is deliberate: the daemon loop owns the channels + the monotonic seq and
//! merely calls these, and a future local-TUI consumer could call the exact same
//! builder, so the wire projection can never drift from a second hand-rolled copy.

use crate::app::mode::agents::{
    AgentEditField, AgentScope, AgentSubMode, AgentsState, ModelPickerState, ToolPickerState,
};
use crate::app::mode::editor::TextEditorState;
use crate::app::mode::settings::{ModelDraft, ModelModal, PathPicker, PickerMode, ProviderDraft};
use crate::app::mode::{
    CookingEntry, EffortPickerState, HistoryEntry, HubPane, KeyInputForm, LoadingState, Mode,
    PickerState, RewindState, SessionHub, SettingsState, UsageMetric, UsageNavState, UsageView,
    WarmStatus,
};
use crate::app::state::AppState;
use crate::app::subagent::SubAgentStatus;
use crate::model::app_config::{ApiType, ModelRole, ThemeMode};
use crate::model::store::SessionMeta;

use super::proto::{
    AgentModelPickerSnapshot, AgentsSnapshot, CatalogueModelSnapshot, CatalogueProviderSnapshot,
    CookingEntrySnapshot, EffortSnapshot, GlobalSnapshot, HistoryEntrySnapshot, KeyInputSnapshot,
    LoadingSnapshot, ModeSnapshot, ModelDraftSnapshot, ModelEndpointWire, ModelModalSnapshot,
    PathPickerSnapshot, PendingSubagentSnapshot, PickerSnapshot, ProviderDraftSnapshot,
    ProviderModalSnapshot, RewindEntrySnapshot, RewindSnapshot, RolePickerSnapshot,
    SessionHubSnapshot, SessionMetaSnapshot, SessionSnapshot, SettingsSnapshot, StateDelta,
    StateSnapshot, SubAgentSnapshot, TextEditorSnapshot, ToolPickerSnapshot, UsageSnapshot,
    WarmStatusWire,
};

/// Build a complete, frozen [`StateSnapshot`] from the live [`AppState`].
///
/// Pure projection: it only COPIES display state out of `state` (it never mutates
/// it, holds no channel, and resolves every session by its stable UUID). Sent on
/// attach and whenever [`diff`] reports a structural change.
pub fn build_snapshot(state: &AppState) -> StateSnapshot {
    let sessions: Vec<SessionSnapshot> = state
        .rest
        .sessions
        .iter()
        .map(session_snapshot)
        .collect();

    // Foreground id by stable UUID (never the index — see proto critique #2). The
    // index is always valid, but the client only ever speaks UUIDs, so project the
    // id; `None` only if there are somehow no sessions (there is always >=1 today).
    let foreground_id = state
        .rest
        .sessions
        .get(state.rest.foreground)
        .map(|s| s.id.clone());

    StateSnapshot {
        foreground_id,
        sessions,
        global: global_snapshot(state),
    }
}

/// Project ONE live [`crate::app::state::SessionRuntime`] into a [`SessionSnapshot`].
fn session_snapshot(rt: &crate::app::state::SessionRuntime) -> SessionSnapshot {
    // Committed conversation messages for render. Carry the FULL slice (including
    // System) — it is the source of truth; the client applies the same display
    // filter the local view does (`role != System`). Empty when no session is open.
    let messages = rt
        .session
        .as_ref()
        .map(|s| s.conversation.messages().to_vec())
        .unwrap_or_default();
    let name = rt
        .session
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_default();

    SessionSnapshot {
        id: rt.id.clone(),
        name,
        // The session's effective cwd (live `cd` override, else configured
        // workdir) as a display string. Empty when there is no session.
        cwd: rt
            .session
            .as_ref()
            .map(|_| rt.effective_cwd().display().to_string())
            .unwrap_or_default(),
        messages,
        streaming: rt.streaming.clone(),
        stream_reasoning: rt.stream_reasoning.clone(),
        tokens_in: rt.tokens_in,
        tokens_out: rt.tokens_out,
        cost: rt.cost,
        tokens_cached: rt.tokens_cached,
        waiting: rt.waiting,
        awaiting_approval: rt.awaiting_approval,
        approval_reason: rt.approval_reason.clone(),
        // The pending tool-call round + its cursor: the approval overlay (Chat
        // mode, when `awaiting_approval`) renders `pending_tool_calls[tool_idx]`,
        // so both must cross. Carried even when not awaiting (cheap, small) so a
        // snapshot taken just before the park already has them.
        pending_tool_calls: rt.pending_tool_calls.clone(),
        tool_idx: rt.tool_idx,
        // `is_working()` is the render-relevant busy signal (stream / wait / parked
        // lane / running sub-agent) — mirror it at snapshot time, never the raw
        // `waiting` alone, so the client's "● working" dot matches the daemon.
        working: rt.is_working(),
        finished_unseen: rt.finished_unseen,
        subagents: rt.subagents.iter().map(subagent_snapshot).collect(),
        // Queued-but-not-started delegations (over the concurrency cap), in FIFO
        // order, so the client's `$` panel lists the same "pending" rows.
        pending_subagents: rt
            .pending_subagents
            .iter()
            .map(pending_subagent_snapshot)
            .collect(),
    }
}

/// Project one live [`crate::app::subagent::SubAgent`] into its plain-data
/// projection (no `rx`, no `AbortHandle`).
///
/// Carries the full transcript + structured messages (not just a tail): the `$`
/// panel + the full-screen viewer both read them, so a remote render must have the
/// same data a local one does. The transcript is short-lived display text, so the
/// full copy is cheap; the diff only ships it on a structural change (any sub-agent
/// field moving forces a full snapshot — see [`diff`]).
fn subagent_snapshot(sa: &crate::app::subagent::SubAgent) -> SubAgentSnapshot {
    // Render the lifecycle enum down to the short string the proto documents, so
    // the client need not mirror `SubAgentStatus`.
    let status = match &sa.status {
        SubAgentStatus::Running => "running".to_string(),
        SubAgentStatus::Done(_) => "done".to_string(),
        SubAgentStatus::Killed => "killed".to_string(),
        SubAgentStatus::Error(e) => format!("error: {e}"),
    };

    SubAgentSnapshot {
        id: sa.id,
        name: sa.agent_name.clone(),
        label: sa.label.clone(),
        status,
        // Progress proxy: accumulated transcript line count.
        steps: sa.transcript.len(),
        transcript: sa.transcript.clone(),
        messages: sa.messages.clone(),
    }
}

/// Project one queued [`crate::app::subagent::PendingSubagent`] into its plain-data
/// render projection (id + agent + prompt — the only fields the panel's pending row
/// shows; the turn-bookkeeping `tool_call_id` is daemon-internal and not projected).
fn pending_subagent_snapshot(p: &crate::app::subagent::PendingSubagent) -> PendingSubagentSnapshot {
    PendingSubagentSnapshot {
        id: p.id,
        agent_name: p.agent_name.clone(),
        prompt: p.prompt.clone(),
    }
}

/// Project the NON-session global UI state into a [`GlobalSnapshot`].
fn global_snapshot(state: &AppState) -> GlobalSnapshot {
    GlobalSnapshot {
        input: state.rest.input.clone(),
        cursor: state.rest.cursor,
        scroll: state.rest.scroll,
        follow: state.rest.follow,
        status: state.rest.status.clone(),
        // `work_since` is a daemon-local `Instant` (the comet's clock); project it
        // as elapsed-ms so the client re-anchors its own clock and the shimmer
        // continues from the same phase instead of restarting at 0 each snapshot.
        work_elapsed_ms: state
            .rest
            .work_since
            .map(|since| since.elapsed().as_millis() as u64),
        // Global theme + accent from the live `AppConfig`. `view::draw` builds the
        // outer palette (`theme::palette(&state.rest.config)`) for EVERY mode before
        // rendering, so these must cross or the client frames every screen at the
        // default Dark/green palette. Theme rides as a wire token (reusing the same
        // helper the Settings draft uses); accent is an opaque palette key, verbatim.
        theme: theme_token(&state.rest.config.theme).to_string(),
        accent: state.rest.config.accent.clone(),
        mode: mode_snapshot(state),
        // Project the toast as (kind, text); the TTL `Instant` is daemon-local and
        // never crosses the wire (the client re-derives its own dismissal timer).
        toast: state.rest.toast.as_ref().map(|(msg, _until, kind)| {
            let kind = match kind {
                crate::app::state::ToastKind::Error => "error".to_string(),
                crate::app::state::ToastKind::Info => "info".to_string(),
            };
            (kind, msg.clone())
        }),
        // The on-demand model catalogue + the endpoint it was fetched for. Both
        // feed the Settings model-modal omnisearch AND the KeyInput step-1 search;
        // a remote client has no fetch path of its own, so without these its
        // omnisearch dropdown would never populate. `ModelInfo` is serde-clean.
        models_cache: state.rest.models_cache.clone(),
        models_cache_endpoint: state.rest.models_cache_endpoint.clone(),
        // Full-screen sub-agent viewer + `$` panel state. These are rendered FROM
        // Chat mode (the chat renderer takes the viewer branch when `agent_viewer`
        // is set, and the input controller floats the `$` panel), so they ride on
        // the global projection — the client mirrors them onto `rest.*` so the
        // unmodified chat draw reproduces both, off the per-session `subagents` it
        // also reconstructs.
        agent_viewer: state.rest.agent_viewer,
        agent_viewer_scroll: state.rest.agent_viewer_scroll,
        agent_viewer_follow: state.rest.agent_viewer_follow,
        subagents_open: state.rest.subagents_open,
        subagent_sel: state.rest.subagent_sel,
        // Staged composer attachments (path-paste / clipboard-image / @-picker),
        // ingested daemon-side — projected so the client's shadow composer mirrors
        // the daemon's exactly. `Attachment` is serde-clean.
        pending_attachments: state.rest.pending_attachments.clone(),
        // The `@`-file palette, precomputed daemon-side so the thin client (whose
        // reconstructed `dir_cache` is empty) can still render the dropdown. Only
        // computed when the composer's last token is an `@partial`; `None`
        // otherwise so the projection never lingers into an unrelated frame. Uses
        // the SAME `search(partial, FILE_PAL_MAX)` the local file-palette view calls.
        file_palette: file_palette_matches(state),
    }
}

/// The maximum `@`-file palette rows, kept in lockstep with the view's `MAX_VIS`
/// and the chat controller's `FILE_PAL_MAX` (both 10) so the projected list is the
/// same length the local renderer would compute.
const FILE_PAL_MAX: usize = 10;

/// Precompute the `@`-file palette matches the way the local view does, for the
/// thin client (whose reconstructed `dir_cache` is empty).
///
/// Returns `None` unless the composer's last whitespace-delimited token is an
/// `@partial` (the only time the file palette shows). When it is, runs the SAME
/// `dir_cache.search(partial, FILE_PAL_MAX)` the local `render_file_palette` calls
/// against the foreground session's live index, so the client renders an identical
/// dropdown. `Some(vec![])` means an `@partial` that matched nothing (still shows
/// no rows on the client, matching the daemon).
fn file_palette_matches(state: &AppState) -> Option<Vec<String>> {
    let partial = crate::controller::input::file_ref_partial(&state.rest.input)?;
    let matches = state
        .rest
        .fg()
        .dir_cache
        .read()
        .map(|c| c.search(partial, FILE_PAL_MAX))
        .unwrap_or_default();
    Some(matches)
}

/// Project the (large, non-serde) [`Mode`] into the pure-data [`ModeSnapshot`].
///
/// 1:1 with the `Mode` variants. Stage 1 filled `Chat` + `QuitConfirm`; stage 2
/// filled the CORE interactive modes (`KeyInput`, `SessionHub`, `Loading`,
/// `Settings`); stage 3 fills the SECONDARY views (`Usage`, `MessageRewind`) and the
/// last stubs (`Effort`, `SessionPicker`, `Agents`) — so EVERY variant now carries
/// its render-relevant payload and nothing falls back to a blank Chat render.
///
/// Takes the whole `state` (not just the mode) because the `Usage` projection
/// pre-computes the dashboard's ledger data scoped to the foreground session (see
/// [`usage_snapshot`]).
fn mode_snapshot(state: &AppState) -> ModeSnapshot {
    match &state.mode {
        Mode::KeyInput(f) => ModeSnapshot::KeyInput(key_input_snapshot(f)),
        Mode::SessionPicker(p) => ModeSnapshot::SessionPicker(picker_snapshot(p)),
        Mode::SessionHub(h) => ModeSnapshot::SessionHub(session_hub_snapshot(h)),
        Mode::Chat => ModeSnapshot::Chat,
        Mode::Loading(s) => ModeSnapshot::Loading(loading_snapshot(s)),
        Mode::Settings(s) => ModeSnapshot::Settings(Box::new(settings_snapshot(s))),
        Mode::Agents(a) => ModeSnapshot::Agents(Box::new(agents_snapshot(a, state))),
        Mode::Effort(e) => ModeSnapshot::Effort(effort_snapshot(e)),
        Mode::Usage(nav) => ModeSnapshot::Usage(Box::new(usage_snapshot(nav, state))),
        Mode::MessageRewind(rw) => ModeSnapshot::MessageRewind(rewind_snapshot(rw)),
        Mode::QuitConfirm(s) => ModeSnapshot::QuitConfirm {
            working: s.working,
            total: s.total,
        },
    }
}

/// Project the first-run wizard form ([`KeyInputForm`]) into its wire snapshot.
fn key_input_snapshot(f: &KeyInputForm) -> KeyInputSnapshot {
    KeyInputSnapshot {
        step: f.step,
        field: f.field,
        endpoint: f.endpoint.clone(),
        api_key: f.api_key.clone(),
        model: f.model.clone(),
        query: f.query.clone(),
        result_sel: f.result_sel,
        first_run: f.first_run,
        from_picker: f.from_picker,
    }
}

/// Project the loading splash ([`LoadingState`]) into its wire snapshot. The live
/// `started` `Instant` becomes elapsed-ms (re-anchored on the client).
fn loading_snapshot(s: &LoadingState) -> LoadingSnapshot {
    LoadingSnapshot {
        elapsed_ms: s.started.elapsed().as_millis() as u64,
        frame: s.frame,
        workspace: warm_status_wire(&s.workspace),
        awareness: warm_status_wire(&s.awareness),
    }
}

/// Project one [`WarmStatus`] into its serde mirror.
fn warm_status_wire(s: &WarmStatus) -> WarmStatusWire {
    match s {
        WarmStatus::Pending => WarmStatusWire::Pending,
        WarmStatus::Running => WarmStatusWire::Running,
        WarmStatus::Done(d) => WarmStatusWire::Done(d.clone()),
        WarmStatus::Skipped => WarmStatusWire::Skipped,
        WarmStatus::Failed => WarmStatusWire::Failed,
    }
}

/// Project the two-pane session hub ([`SessionHub`]) into its wire snapshot.
fn session_hub_snapshot(h: &SessionHub) -> SessionHubSnapshot {
    SessionHubSnapshot {
        cooking: h.cooking.iter().map(cooking_entry_snapshot).collect(),
        history: h.history.iter().map(history_entry_snapshot).collect(),
        focus_cooking: matches!(h.focus, HubPane::Cooking),
        cooking_selected: h.cooking_selected,
        history_selected: h.history_selected,
    }
}

/// Project one COOKING row. The live `idx` is a daemon-side sessions index (used
/// on Enter, forwarded as a key) and is not rendered, so it is dropped.
fn cooking_entry_snapshot(e: &CookingEntry) -> CookingEntrySnapshot {
    CookingEntrySnapshot {
        name: e.name.clone(),
        working: e.working,
        is_foreground: e.is_foreground,
    }
}

/// Project one HISTORY row. The live `path` is the daemon-side load target (used on
/// Enter) and not rendered; the `SystemTime` becomes seconds-since-epoch.
fn history_entry_snapshot(e: &HistoryEntry) -> HistoryEntrySnapshot {
    let last_active_secs = e
        .last_active
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    HistoryEntrySnapshot {
        name: e.name.clone(),
        last_active_secs,
    }
}

/// Project the `/settings` dashboard ([`SettingsState`]) into its wire snapshot —
/// the biggest mode projection. Every draft + list + modal + picker the view reads
/// is copied so the client can rebuild a real `SettingsState` and render it through
/// the unmodified settings view.
fn settings_snapshot(st: &SettingsState) -> SettingsSnapshot {
    SettingsSnapshot {
        cat: st.cat,
        field: st.field,
        in_detail: st.in_detail,
        editing: st.editing,
        api_key: st.api_key.clone(),
        model: st.model.clone(),
        provider: st.provider.clone(),
        name: st.name.clone(),
        theme: theme_token(&st.theme).to_string(),
        accent: st.accent.clone(),
        workdir: st.workdir.clone(),
        awareness_enabled: st.awareness_enabled,
        awareness_inherit: st.awareness_inherit,
        awareness_model: st.awareness_model.clone(),
        awareness_provider: st.awareness_provider.clone(),
        classifier_enabled: st.classifier_enabled,
        classifier_model: st.classifier_model.clone(),
        classifier_provider: st.classifier_provider.clone(),
        allowed_folders: st.allowed_folders.clone(),
        short_send_enabled: st.short_send_enabled,
        sliding_cache: st.sliding_cache,
        internet_mode: st.internet_mode.as_str().to_string(),
        cwd: st.cwd.display().to_string(),
        list_editing: st.list_editing,
        list_sel: st.list_sel,
        picker: st.picker.as_ref().map(path_picker_snapshot),
        providers: st.providers.iter().map(provider_draft_snapshot).collect(),
        prov_sel: st.prov_sel,
        prov_delete_armed: st.prov_delete_armed,
        prov_modal: st.prov_modal.as_ref().map(|m| ProviderModalSnapshot {
            name: m.name.clone(),
            endpoint: m.endpoint.clone(),
            api_type: api_type_token(m.api_type).to_string(),
            api_key: m.api_key.clone(),
            field: m.field,
        }),
        models: st.models.iter().map(model_draft_snapshot).collect(),
        model_sel: st.model_sel,
        model_delete_armed: st.model_delete_armed,
        model_modal: st.model_modal.as_ref().map(model_modal_snapshot),
    }
}

/// Project one [`ProviderDraft`] row.
fn provider_draft_snapshot(p: &ProviderDraft) -> ProviderDraftSnapshot {
    ProviderDraftSnapshot {
        uuid: p.uuid.clone(),
        name: p.name.clone(),
        endpoint: p.endpoint.clone(),
        api_type: api_type_token(p.api_type).to_string(),
        api_key: p.api_key.clone(),
    }
}

/// Project one [`ModelDraft`] row (roles → wire tokens).
fn model_draft_snapshot(m: &ModelDraft) -> ModelDraftSnapshot {
    ModelDraftSnapshot {
        uuid: m.uuid.clone(),
        name: m.name.clone(),
        model_id: m.model_id.clone(),
        provider_idx: m.provider_idx,
        roles: m.roles.iter().map(|r| role_token(*r).to_string()).collect(),
        route: m.route.clone(),
        session_only: m.session_only,
    }
}

/// Project the add/edit-model modal ([`ModelModal`]) — endpoints ride as the serde
/// mirror, the role picker as its snapshot.
fn model_modal_snapshot(m: &ModelModal) -> ModelModalSnapshot {
    ModelModalSnapshot {
        editing_idx: m.editing_idx,
        uuid: m.uuid.clone(),
        name: m.name.clone(),
        provider_idx: m.provider_idx,
        model_id: m.model_id.clone(),
        field: m.field,
        roles: m.roles.iter().map(|r| role_token(*r).to_string()).collect(),
        role_picker: m.role_picker.as_ref().map(|rp| RolePickerSnapshot {
            checked: rp.checked.clone(),
            cursor: rp.cursor,
        }),
        query: m.query.clone(),
        result_sel: m.result_sel,
        route: m.route.clone(),
        route_sel: m.route_sel,
        endpoints: m.endpoints.as_ref().map(|eps| {
            eps.iter()
                .map(|ep| ModelEndpointWire {
                    name: ep.name.clone(),
                    provider_name: ep.provider_name.clone(),
                    price_prompt: ep.pricing.as_ref().and_then(|p| p.prompt.clone()),
                    price_completion: ep.pricing.as_ref().and_then(|p| p.completion.clone()),
                    uptime_last_30m: ep.uptime_last_30m,
                })
                .collect()
        }),
        endpoints_loading: m.endpoints_loading,
        endpoints_for: m.endpoints_for.clone(),
    }
}

/// Project the FS directory picker overlay ([`PathPicker`]). The matches are the
/// daemon's already-computed `read_dir` results, carried verbatim so the client
/// renders the same list without touching any filesystem.
fn path_picker_snapshot(p: &PathPicker) -> PathPickerSnapshot {
    PathPickerSnapshot {
        query: p.query.clone(),
        matches: p.matches.clone(),
        sel: p.sel,
        replace_idx: match p.mode {
            PickerMode::Add => None,
            PickerMode::Replace(i) => Some(i),
        },
    }
}

// ─── stage 3: secondary full-screen view projections ─────────────────────────

/// Project the `/usage` dashboard ([`UsageNavState`]) + its pre-fetched ledger data
/// into the wire snapshot.
///
/// The dashboard's numbers come from the global sqlite ledger, which the thin
/// client cannot read. So the daemon runs the SAME collection the local renderer
/// would (`view::usage::collect_usage_data`, scoped to the active view/range + the
/// foreground session) and ships the result; the client seeds `rest.usage_data` from
/// it and renders the unmodified dashboard with no DB. The nav state rides as wire
/// tokens.
fn usage_snapshot(nav: &UsageNavState, state: &AppState) -> UsageSnapshot {
    UsageSnapshot {
        view: usage_view_token(nav.view).to_string(),
        range: nav.range.label().to_string(),
        metric: usage_metric_token(nav.metric).to_string(),
        // Collect the dashboard's data daemon-side via the SAME helper the local
        // renderer uses (one source of truth), so the client renders identically.
        data: crate::view::usage::collect_usage_data(nav, &state.rest),
    }
}

/// Wire token for a [`UsageView`].
fn usage_view_token(v: UsageView) -> &'static str {
    match v {
        UsageView::Global => "global",
        UsageView::Session => "session",
    }
}

/// Wire token for a [`UsageMetric`].
fn usage_metric_token(m: UsageMetric) -> &'static str {
    match m {
        UsageMetric::Cost => "cost",
        UsageMetric::Tokens => "tokens",
    }
}

/// Project the message-rewind picker ([`RewindState`]) into its wire snapshot — the
/// newest-first user-message entries + the cursor.
fn rewind_snapshot(rw: &RewindState) -> RewindSnapshot {
    RewindSnapshot {
        entries: rw
            .entries
            .iter()
            .map(|e| RewindEntrySnapshot {
                vec_index: e.vec_index,
                content: e.content.clone(),
            })
            .collect(),
        selected: rw.selected,
    }
}

/// Project the `/effort` reasoning-effort picker ([`EffortPickerState`]) into its
/// wire snapshot (all plain data the overlay reads).
fn effort_snapshot(e: &EffortPickerState) -> EffortSnapshot {
    EffortSnapshot {
        options: e.options.clone(),
        selected: e.selected,
        note: e.note.clone(),
    }
}

/// Project the `--resume` session picker ([`PickerState`]) into its wire snapshot.
/// The full metadata list + the live query + the filtered subset + the cursor cross
/// so the client renders the SAME rows (it re-filters nothing of its own).
fn picker_snapshot(p: &PickerState) -> PickerSnapshot {
    PickerSnapshot {
        query: p.query.clone(),
        all: p.all.iter().map(session_meta_snapshot).collect(),
        filtered_idx: p.filtered_idx.clone(),
        selected: p.selected,
    }
}

/// Project one [`SessionMeta`] row. The `PathBuf` (daemon-side load target) is
/// dropped (not rendered); the `SystemTime` becomes seconds-since-epoch.
fn session_meta_snapshot(m: &SessionMeta) -> SessionMetaSnapshot {
    let modified_secs = m
        .modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SessionMetaSnapshot {
        id: m.id.clone(),
        name: m.name.clone(),
        modified_secs,
        message_count: m.message_count,
        locked: m.locked,
    }
}

/// Project the `/agents` dashboard ([`AgentsState`]) into its wire snapshot.
///
/// Carries the agent list (`AgentDef` is serde-clean), the working drafts plus the
/// sub-mode and field cursor (as wire tokens), the three overlays, and a KEYLESS
/// model/provider catalogue (drawn from the live `AppConfig` and the foreground
/// session's session models) so the client resolves the model label without any API
/// key crossing the wire.
fn agents_snapshot(a: &AgentsState, state: &AppState) -> AgentsSnapshot {
    let config = &state.rest.config;

    // Keyless registered-model catalogue: the session overrides FIRST (the same
    // order the model picker lists), then the global catalogue. The label resolver
    // (`view::agents::model_display`) checks session models then global; folding both
    // into one keyless list lets the client resolve either source's uuid.
    let mut catalogue_models: Vec<CatalogueModelSnapshot> = Vec::new();
    if let Some(session) = state.rest.fg().session.as_ref() {
        for e in &session.settings.session_models {
            catalogue_models.push(CatalogueModelSnapshot {
                uuid: e.uuid.clone(),
                name: e.name.clone(),
                model_id: e.model_id.clone(),
                provider_uuid: e.provider_uuid.clone(),
            });
        }
    }
    for e in &config.models {
        catalogue_models.push(CatalogueModelSnapshot {
            uuid: e.uuid.clone(),
            name: e.name.clone(),
            model_id: e.model_id.clone(),
            provider_uuid: e.provider_uuid.clone(),
        });
    }

    // Keyless provider catalogue (uuid + display name + endpoint only — NEVER the
    // api key). The reconstructed `AppConfig` carries no keys, which is correct: the
    // client only resolves labels, never makes a request.
    let catalogue_providers: Vec<CatalogueProviderSnapshot> = config
        .providers
        .iter()
        .map(|p| CatalogueProviderSnapshot {
            uuid: p.uuid.clone(),
            name: p.name.clone(),
            endpoint: p.endpoint.clone(),
        })
        .collect();

    AgentsSnapshot {
        agents: a.agents.clone(),
        list_sel: a.list_sel,
        in_detail: a.in_detail,
        mode: agent_submode_token(a.mode).to_string(),
        field: agent_field_token(a.field).to_string(),
        editing: a.editing,
        create_scope: agent_scope_token(a.create_scope).to_string(),
        draft_name: a.draft_name.clone(),
        draft_description: a.draft_description.clone(),
        draft_conditions: a.draft_conditions.clone(),
        draft_model_uuid: a.draft_model_uuid.clone(),
        draft_model_legacy: a.draft_model_legacy.clone(),
        draft_tools: a.draft_tools.clone(),
        draft_body: a.draft_body.clone(),
        tool_picker: a.tool_picker.as_ref().map(tool_picker_snapshot),
        model_picker: a.model_picker.as_ref().map(agent_model_picker_snapshot),
        editor: a
            .editor
            .as_ref()
            .map(|(field, ed)| (agent_field_token(*field).to_string(), text_editor_snapshot(ed))),
        editor_clear_confirm: a.editor_clear_confirm,
        catalogue_models,
        catalogue_providers,
    }
}

/// Project the `/agents` tool multi-select picker ([`ToolPickerState`]).
fn tool_picker_snapshot(p: &ToolPickerState) -> ToolPickerSnapshot {
    ToolPickerSnapshot {
        options: p.options.clone(),
        checked: p.checked.clone(),
        cursor: p.cursor,
        filter: p.filter.clone(),
    }
}

/// Project the `/agents` single-select model picker ([`ModelPickerState`]).
fn agent_model_picker_snapshot(p: &ModelPickerState) -> AgentModelPickerSnapshot {
    AgentModelPickerSnapshot {
        options: p.options.clone(),
        cursor: p.cursor,
    }
}

/// Project the full-screen nano editor ([`TextEditorState`]). The render-published
/// `wrap_w` cell is re-seeded on the client, so only the buffer + cursor + scroll
/// cross.
fn text_editor_snapshot(ed: &TextEditorState) -> TextEditorSnapshot {
    TextEditorSnapshot {
        lines: ed.lines.clone(),
        row: ed.row,
        col: ed.col,
        scroll: ed.scroll,
    }
}

/// Wire token for an [`AgentSubMode`].
fn agent_submode_token(m: AgentSubMode) -> &'static str {
    match m {
        AgentSubMode::Browse => "browse",
        AgentSubMode::Edit => "edit",
        AgentSubMode::Create => "create",
        AgentSubMode::DeleteConfirm => "delete_confirm",
    }
}

/// Wire token for an [`AgentEditField`].
fn agent_field_token(f: AgentEditField) -> &'static str {
    match f {
        AgentEditField::Name => "name",
        AgentEditField::Description => "description",
        AgentEditField::Conditions => "conditions",
        AgentEditField::Model => "model",
        AgentEditField::Tools => "tools",
        AgentEditField::Body => "prompt",
    }
}

/// Wire token for an [`AgentScope`].
fn agent_scope_token(s: AgentScope) -> &'static str {
    match s {
        AgentScope::Session => "session",
        AgentScope::Global => "global",
    }
}

/// Wire token for a [`ThemeMode`].
fn theme_token(t: &ThemeMode) -> &'static str {
    match t {
        ThemeMode::Dark => "dark",
        ThemeMode::Light => "light",
    }
}

/// Wire token for an [`ApiType`].
fn api_type_token(t: ApiType) -> &'static str {
    match t {
        ApiType::OpenAiCompatible => "openai",
        ApiType::AnthropicCompatible => "anthropic",
    }
}

/// Wire token for a [`ModelRole`].
fn role_token(r: ModelRole) -> &'static str {
    match r {
        ModelRole::Main => "main",
        ModelRole::Awareness => "awareness",
        ModelRole::Safeguard => "safeguard",
        ModelRole::Compactor => "compactor",
    }
}

/// The outcome of diffing the current snapshot against the previously-sent one.
///
/// Either a set of fine-grained [`StateDelta`]s to fan out (each becomes one
/// seq-tagged `Delta` frame), or `needs_full` — a STRUCTURAL change the daemon
/// answers with a fresh full `Snapshot` frame instead (and the `deltas` are then
/// ignored). The two are mutually exclusive by construction: the moment a
/// structural change is detected, diffing stops and `needs_full` is set.
#[derive(Debug, Default, PartialEq)]
pub struct DiffResult {
    /// When true, the incremental `deltas` are INSUFFICIENT (a session was
    /// added/removed, the id set changed, or a hard-to-diff field moved); the
    /// caller must resend a full [`StateSnapshot`] instead.
    pub needs_full: bool,
    /// Fine-grained updates to fan out, in emission order. Empty + `!needs_full`
    /// means nothing changed this tick (no frame is emitted).
    pub deltas: Vec<StateDelta>,
}

impl DiffResult {
    /// The "resend a full snapshot" outcome (deltas are irrelevant then).
    fn full() -> Self {
        Self {
            needs_full: true,
            deltas: Vec::new(),
        }
    }
}

/// Diff `prev` -> `next` into the minimal [`StateDelta`]s, or request a full
/// snapshot for a structural change.
///
/// High-frequency per-tick changes are diffed incrementally (streamed token /
/// reasoning appends, working / finished-unseen flips, the global + per-session
/// status line, the foreground switch, the toast). Anything STRUCTURAL or awkward
/// to fold incrementally — the session list changing, a session's committed
/// history / token counters / approval state / sub-agent set moving — short-
/// circuits to `needs_full` so the client rebuilds from a fresh snapshot. This is
/// the correctness-first stance the stage calls for: a full snapshot is always a
/// valid update, so when in doubt we send one rather than risk a wrong shadow.
pub fn diff(prev: &StateSnapshot, next: &StateSnapshot) -> DiffResult {
    // --- structural: the mode VARIANT or its payload changed ---
    // `ModeSnapshot` is now a pure-data projection (not a bare tag), so this `!=`
    // fires on BOTH a variant switch (Chat -> QuitConfirm) AND a payload change
    // within a variant (e.g. QuitConfirm's busy/total counts moving). Neither is
    // carried by any incremental delta, and an idle session has no other structural
    // change to coincidentally trigger a full snapshot, so without this the client
    // shadow stays in the old mode/payload (e.g. never enters QuitConfirm, so its
    // key-interception branch never fires; or shows a stale overlay header). A full
    // snapshot rebuilds the screen and is always a valid update, so force one the
    // instant the mode projection moves. (The per-tick `work_elapsed_ms` is
    // deliberately NOT diffed — see the note where the global fields are compared.)
    if prev.global.mode != next.global.mode {
        return DiffResult::full();
    }

    // --- structural: theme / accent palette changed ---
    // The daemon builds the outer palette via `theme::palette(&state.rest.config)` BEFORE
    // dispatching to any mode renderer, so without this the client stays in the default
    // palette (Dark/green) until the next structural change forces a full resync. A full
    // snapshot ensures the client's `rest.config` palette stays in sync with the daemon's.
    if prev.global.theme != next.global.theme || prev.global.accent != next.global.accent {
        return DiffResult::full();
    }

    // --- structural: the sub-agent viewer / `$` panel state changed ---
    // These global flags (the full-screen viewer's open-index + scroll + follow, and
    // the `$` panel's open-state + selection) are rendered FROM Chat mode, so a change
    // doesn't move `global.mode` and no incremental delta carries them. They flip only
    // on discrete user actions (open/scroll the viewer, open/navigate the panel), so a
    // full snapshot on a change is cheap-correct — without it the client's viewer/panel
    // would lag until the next structural change. (The viewer's CONTENT updates already
    // force a full snapshot via the per-session `subagents` comparison below.)
    if prev.global.agent_viewer != next.global.agent_viewer
        || prev.global.agent_viewer_scroll != next.global.agent_viewer_scroll
        || prev.global.agent_viewer_follow != next.global.agent_viewer_follow
        || prev.global.subagents_open != next.global.subagents_open
        || prev.global.subagent_sel != next.global.subagent_sel
    {
        return DiffResult::full();
    }

    // --- structural: staged composer attachments / `@`-file palette changed ---
    // Neither rides an incremental delta. `pending_attachments` flips only on a
    // discrete attach/submit/clear; `file_palette` changes as an `@token` is typed
    // (the match set narrows) — both infrequent relative to streaming, so a full
    // snapshot on a change is cheap-correct. Without this the client's `[Image #N]`
    // card data lags and (crucially) its `@` dropdown — which renders ONLY from the
    // projected `file_palette` on a thin client — never updates as the user types
    // the partial. (The `[Image #N]` marker TEXT still rides `input` via InputChanged,
    // but the palette + attachment records need the snapshot.)
    if prev.global.pending_attachments != next.global.pending_attachments
        || prev.global.file_palette != next.global.file_palette
    {
        return DiffResult::full();
    }

    // --- structural: the on-demand model catalogue changed ---
    // The omnisearch cache (and the endpoint it was fetched for) feeds the Settings
    // model modal + the KeyInput search dropdowns. It changes only when a fetch
    // lands (infrequent) and no incremental delta carries it, so a change forces a
    // full snapshot — the screen that reads it (Settings/KeyInput) then re-renders
    // with the populated dropdown instead of a stale `searching models…`.
    if prev.global.models_cache != next.global.models_cache
        || prev.global.models_cache_endpoint != next.global.models_cache_endpoint
    {
        return DiffResult::full();
    }

    // --- structural: the session SET (count or id order) changed ---
    // A different length or a reordered/replaced id list can't be expressed by the
    // per-session deltas (which address sessions by id and assume the set is
    // stable), so rebuild wholesale. SessionAdded exists in the vocabulary, but a
    // full snapshot is simpler AND always correct for any list change.
    if prev.sessions.len() != next.sessions.len()
        || prev
            .sessions
            .iter()
            .zip(next.sessions.iter())
            .any(|(a, b)| a.id != b.id)
    {
        return DiffResult::full();
    }

    let mut deltas: Vec<StateDelta> = Vec::new();

    // --- per-session, id-keyed (the set is identical + in the same order here) ---
    for (p, n) in prev.sessions.iter().zip(next.sessions.iter()) {
        // Any of these moving is either hard to fold incrementally or rare enough
        // that a full resync is the honest, cheap-correct answer.
        let structural = p.messages != n.messages
            || p.tokens_in != n.tokens_in
            || p.tokens_out != n.tokens_out
            || p.tokens_cached != n.tokens_cached
            || p.cost != n.cost
            || p.awaiting_approval != n.awaiting_approval
            || p.approval_reason != n.approval_reason
            // The pending tool-call set / cursor moving changes what the approval
            // overlay draws; no incremental delta carries it, so resync wholesale.
            || p.pending_tool_calls != n.pending_tool_calls
            || p.tool_idx != n.tool_idx
            || p.name != n.name
            // A `cd` (the effective cwd moving) has no incremental delta, so a
            // change forces a full resync — the client rebuilds with the new cwd.
            || p.cwd != n.cwd
            || p.subagents != n.subagents
            || p.pending_subagents != n.pending_subagents;
        if structural {
            return DiffResult::full();
        }

        // Streaming content: only a pure APPEND is expressible as TokenAppended.
        match append_suffix(p.streaming.as_deref(), n.streaming.as_deref()) {
            AppendDiff::Same => {}
            AppendDiff::Appended(text) => deltas.push(StateDelta::TokenAppended {
                session_id: n.id.clone(),
                text,
            }),
            // Buffer reset / diverged / cleared (turn boundary) — not a suffix
            // append; a full snapshot keeps the shadow exact.
            AppendDiff::Reset => return DiffResult::full(),
        }

        // Reasoning content: same pure-append rule on the parallel buffer.
        match append_suffix(
            Some(p.stream_reasoning.as_str()),
            Some(n.stream_reasoning.as_str()),
        ) {
            AppendDiff::Same => {}
            AppendDiff::Appended(text) => deltas.push(StateDelta::ReasoningAppended {
                session_id: n.id.clone(),
                text,
            }),
            AppendDiff::Reset => return DiffResult::full(),
        }

        // Working / finished-unseen flags (the sticky marker rides here).
        if p.working != n.working || p.finished_unseen != n.finished_unseen {
            deltas.push(StateDelta::SessionStatusChanged {
                session_id: n.id.clone(),
                working: n.working,
                finished_unseen: n.finished_unseen,
            });
        }
    }

    // --- global status line ---
    if prev.global.status != next.global.status {
        deltas.push(StateDelta::StatusChanged {
            session_id: None,
            text: next.global.status.clone(),
        });
    }

    // --- transcript scroll + follow (global view state) ---
    // A daemon-side scroll (forwarded PageUp/Home/End, or new content re-pinning
    // follow) moves these every-so-often; carry an incremental delta so a controller
    // client's rendered scroll tracks the daemon between full snapshots instead of
    // freezing until the next structural change. Both fields ride together since
    // they move together (a scroll up clears follow; reaching bottom re-sets it).
    if prev.global.scroll != next.global.scroll || prev.global.follow != next.global.follow {
        deltas.push(StateDelta::ScrollChanged {
            scroll: next.global.scroll,
            follow: next.global.follow,
        });
    }

    // NOTE: `global.work_elapsed_ms` is intentionally NOT diffed. It is the comet's
    // clock and ticks up every render while a session works — diffing it would force
    // a delta (or worse, a full snapshot) on EVERY tick. The client re-anchors its
    // own `work_since` clock from each full snapshot and lets it tick locally in
    // between, so the comet stays smooth without per-tick wire traffic (same stance
    // as the toast TTL `Instant`, which is also not carried).

    // --- shared composer (text + caret) ---
    // The composer is NOT append-only (mid-string insert/delete, arrow-key caret
    // moves), so unlike the streaming buffers it can't be expressed as a suffix
    // append — carry the whole input string. A caret-only move (text unchanged,
    // cursor changed) still emits so the rendered caret tracks the daemon. Without
    // this the controller client's composer stays blank while the user types, until
    // the next structural change forces a full snapshot.
    if prev.global.input != next.global.input || prev.global.cursor != next.global.cursor {
        deltas.push(StateDelta::InputChanged {
            text: next.global.input.clone(),
            cursor: next.global.cursor,
        });
    }

    // --- foreground switch (by stable id) ---
    if prev.foreground_id != next.foreground_id {
        if let Some(id) = next.foreground_id.clone() {
            deltas.push(StateDelta::ForegroundChanged { session_id: id });
        } else {
            // Foreground became "none" — unusual (there is always >=1 session
            // today); resync rather than invent a delta the vocabulary lacks.
            return DiffResult::full();
        }
    }

    // --- toast (kind, text) ---
    // A new or changed toast emits a Toast delta. A toast CLEARING has no dedicated
    // delta in this stage's vocabulary; it is purely cosmetic (the client's own TTL
    // would dismiss it anyway), so a clear is intentionally NOT forced to a full
    // resync — favouring cheap per-tick deltas over a snapshot for a fading toast.
    if prev.global.toast != next.global.toast {
        if let Some((kind, text)) = next.global.toast.clone() {
            deltas.push(StateDelta::Toast { kind, text });
        }
    }

    DiffResult {
        needs_full: false,
        deltas,
    }
}

/// Result of comparing an old vs new append-only string buffer.
enum AppendDiff {
    /// Unchanged.
    Same,
    /// `next` is `prev` plus this non-empty suffix.
    Appended(String),
    /// `next` is NOT an extension of `prev` (shrunk, cleared, or diverged) — the
    /// buffer was reset at a turn boundary; the caller must resync.
    Reset,
}

/// Classify `prev` -> `next` for an APPEND-ONLY buffer (the streaming content /
/// reasoning buffers only ever grow within a turn, then reset between turns).
///
/// `None` and `Some("")` are treated alike (both "no buffer"): a stream that goes
/// `None` -> `Some("")` -> `Some("hi")` yields `Same` then `Appended("hi")`, and a
/// commit that drops `Some("...")` -> `None` yields `Reset` so the shadow re-syncs.
fn append_suffix(prev: Option<&str>, next: Option<&str>) -> AppendDiff {
    let p = prev.unwrap_or("");
    let n = next.unwrap_or("");
    if p == n {
        AppendDiff::Same
    } else if let Some(rest) = n.strip_prefix(p) {
        // Pure extension of the previous buffer (covers the empty-prefix start).
        AppendDiff::Appended(rest.to_string())
    } else {
        // Shrunk or diverged: a turn boundary reset the buffer.
        AppendDiff::Reset
    }
}

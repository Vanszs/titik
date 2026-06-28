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

use crate::app::mode::settings::{ModelDraft, ModelModal, PathPicker, PickerMode, ProviderDraft};
use crate::app::mode::{
    CookingEntry, HistoryEntry, HubPane, KeyInputForm, LoadingState, Mode, SessionHub,
    SettingsState, WarmStatus,
};
use crate::app::state::AppState;
use crate::app::subagent::SubAgentStatus;
use crate::model::app_config::{ApiType, ModelRole, ThemeMode};

use super::proto::{
    CookingEntrySnapshot, GlobalSnapshot, HistoryEntrySnapshot, KeyInputSnapshot, LoadingSnapshot,
    ModeSnapshot, ModelDraftSnapshot, ModelEndpointWire, ModelModalSnapshot, PathPickerSnapshot,
    PendingSubagentSnapshot, ProviderDraftSnapshot, ProviderModalSnapshot, RolePickerSnapshot,
    SessionHubSnapshot, SessionSnapshot, SettingsSnapshot, StateDelta, StateSnapshot,
    SubAgentSnapshot, WarmStatusWire,
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
        mode: mode_snapshot(&state.mode),
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
    }
}

/// Project the (large, non-serde) [`Mode`] into the pure-data [`ModeSnapshot`].
///
/// 1:1 with the `Mode` variants. Stage 1 filled `Chat` + `QuitConfirm`; stage 2
/// fills the CORE interactive modes — `KeyInput`, `SessionHub`, `Loading`,
/// `Settings` — copying each live mode's render-relevant fields into its payload
/// projection. The remaining variants stay STUBS carrying only "this screen is
/// active" (the client falls back to a safe Chat render for them).
fn mode_snapshot(mode: &Mode) -> ModeSnapshot {
    match mode {
        Mode::KeyInput(f) => ModeSnapshot::KeyInput(key_input_snapshot(f)),
        Mode::SessionPicker(_) => ModeSnapshot::SessionPicker,
        Mode::SessionHub(h) => ModeSnapshot::SessionHub(session_hub_snapshot(h)),
        Mode::Chat => ModeSnapshot::Chat,
        Mode::Loading(s) => ModeSnapshot::Loading(loading_snapshot(s)),
        Mode::Settings(s) => ModeSnapshot::Settings(Box::new(settings_snapshot(s))),
        Mode::Agents(_) => ModeSnapshot::Agents,
        Mode::Effort(_) => ModeSnapshot::Effort,
        Mode::Usage(_) => ModeSnapshot::Usage,
        Mode::MessageRewind(_) => ModeSnapshot::MessageRewind,
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

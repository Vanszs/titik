//! IPC layer: the wire protocol the koma-daemon and its thin TUI client speak.
//!
//! The end-state architecture is ALWAYS-CLIENT/SERVER: a headless `koma-daemon`
//! owns the agent runtime + session locks, and the TUI is a thin attach/detach
//! client over a unix socket (`~/.koma/daemon.sock`) using length-prefixed JSON
//! frames. This module is STAGE 1 of that split: it defines ONLY the message
//! vocabulary ([`proto`]) — the request/response/snapshot/delta types — with no
//! transport and no callers yet. The socket server, framing, and snapshot/delta
//! emission land in later stages.
//!
//! See [`proto`] for the protocol types and the critique fixes (stable session
//! UUIDs, monotonic seq, frame-size cap) that are designed into them from the
//! start to prevent silent stream corruption later.
//!
//! STAGE 2 adds the transport primitives — [`frame`] (the shared length-prefixed
//! codec), [`server`] (bind = liveness oracle), and [`client`] (connect + frame
//! helpers) — plus a [`selftest`] that round-trips a real frame end-to-end. The
//! daemon/client loop wiring that consumes them is still a later stage; the
//! transport is additive and does not touch the TUI path.

pub mod client;
pub mod conn;
pub mod frame;
pub mod proto;
pub mod selftest;
pub mod server;
pub mod snapshot;

#[cfg(test)]
mod roundtrip_tests {
    //! Serde round-trip coverage for the wire protocol.
    //!
    //! Asserts that at least one of each [`proto::ClientRequest`] variant and each
    //! [`proto::DaemonFrame`] event kind (Snapshot / Delta / Ack / Error) survives
    //! a `serde_json` encode->decode unchanged. This both proves the types are
    //! wire-stable and gives the otherwise-dead scaffolding a real use.

    use super::proto::*;

    /// Encode -> decode through JSON and assert structural equality.
    fn roundtrip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, &back, "round-trip mismatch for {value:?}");
    }

    fn sample_session_snapshot() -> SessionSnapshot {
        use crate::dto::chat::{ChatMessage, Role};
        SessionSnapshot {
            id: "11111111-1111-4111-8111-111111111111".to_string(),
            name: "demo".to_string(),
            messages: vec![ChatMessage::new(Role::User, "hi")],
            streaming: Some("partial".to_string()),
            stream_reasoning: "thinking".to_string(),
            tokens_in: 100,
            tokens_out: 42,
            cost: 0.0012,
            tokens_cached: 16,
            waiting: true,
            awaiting_approval: false,
            approval_reason: None,
            working: true,
            finished_unseen: false,
            subagents: vec![SubAgentSnapshot {
                id: 1,
                name: "explorer".to_string(),
                label: "scan the repo".to_string(),
                status: "running".to_string(),
                steps: 3,
                transcript: vec!["scanned src/".to_string()],
                messages: vec![ChatMessage::new(Role::User, "scan")],
            }],
            pending_subagents: vec![PendingSubagentSnapshot {
                id: 2,
                agent_name: "reviewer".to_string(),
                prompt: "review the diff".to_string(),
            }],
        }
    }

    fn sample_global_snapshot() -> GlobalSnapshot {
        GlobalSnapshot {
            input: "type here".to_string(),
            cursor: 4,
            scroll: 0,
            follow: true,
            status: "ready".to_string(),
            work_elapsed_ms: Some(1500),
            mode: ModeSnapshot::QuitConfirm {
                working: 1,
                total: 2,
            },
            toast: Some(("info".to_string(), "saved".to_string())),
        }
    }

    fn sample_snapshot() -> StateSnapshot {
        StateSnapshot {
            foreground_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            sessions: vec![sample_session_snapshot()],
            global: sample_global_snapshot(),
        }
    }

    #[test]
    fn client_request_variants_roundtrip() {
        let variants = vec![
            ClientRequest::Attach {
                foreground_id: Some("abc".to_string()),
            },
            ClientRequest::Detach,
            ClientRequest::ListSessions,
            ClientRequest::Resync,
            ClientRequest::SwitchForeground {
                session_id: "abc".to_string(),
            },
            ClientRequest::SubmitInput {
                text: "hello world".to_string(),
            },
            ClientRequest::SendKey(KeyWire {
                code: KeyCodeWire::Char('x'),
                mods: key_mods::CONTROL,
            }),
            ClientRequest::ApproveTool { approve: true },
            ClientRequest::NewSession {
                name: Some("scratch".to_string()),
                working_dir: Some("/tmp/x".to_string()),
            },
            ClientRequest::QuitSession {
                session_id: "abc".to_string(),
            },
            ClientRequest::QuitDaemon,
        ];
        for v in &variants {
            roundtrip(v);
        }
    }

    #[test]
    fn daemon_frame_event_kinds_roundtrip() {
        let frames = vec![
            DaemonFrame {
                seq: 1,
                event: DaemonEvent::Snapshot(sample_snapshot()),
            },
            DaemonFrame {
                seq: 2,
                event: DaemonEvent::Delta(StateDelta::TokenAppended {
                    session_id: "abc".to_string(),
                    text: "tok".to_string(),
                }),
            },
            DaemonFrame {
                seq: 3,
                event: DaemonEvent::Ack,
            },
            DaemonFrame {
                seq: 4,
                event: DaemonEvent::Error("boom".to_string()),
            },
        ];
        for f in &frames {
            roundtrip(f);
        }
    }

    #[test]
    fn state_delta_variants_roundtrip() {
        let deltas = vec![
            StateDelta::TokenAppended {
                session_id: "s".to_string(),
                text: "t".to_string(),
            },
            StateDelta::ReasoningAppended {
                session_id: "s".to_string(),
                text: "r".to_string(),
            },
            StateDelta::StatusChanged {
                session_id: Some("s".to_string()),
                text: "working".to_string(),
            },
            StateDelta::StatusChanged {
                session_id: None,
                text: "global".to_string(),
            },
            StateDelta::InputChanged {
                text: "hi".to_string(),
                cursor: 2,
            },
            StateDelta::ScrollChanged {
                scroll: 7,
                follow: false,
            },
            StateDelta::SessionStatusChanged {
                session_id: "s".to_string(),
                working: false,
                finished_unseen: true,
            },
            StateDelta::ForegroundChanged {
                session_id: "s".to_string(),
            },
            StateDelta::SessionAdded(sample_session_snapshot()),
            StateDelta::Toast {
                kind: "error".to_string(),
                text: "nope".to_string(),
            },
        ];
        for d in &deltas {
            roundtrip(d);
        }
    }

    #[test]
    fn keywire_roundtrips_through_crossterm() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // A mapped key with modifiers survives KeyEvent -> KeyWire -> JSON ->
        // KeyWire -> KeyEvent exactly.
        let ev = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        let wire = KeyWire::from(ev);
        let json = serde_json::to_string(&wire).expect("serialize");
        let back: KeyWire = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(wire, back);
        let rebuilt = back.to_key_event();
        assert_eq!(rebuilt.code, KeyCode::Char('a'));
        assert!(rebuilt.modifiers.contains(KeyModifiers::CONTROL));
        assert!(rebuilt.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn stream_event_wire_projects_and_skips() {
        use crate::service::StreamEvent;
        // A turn-relevant event projects and round-trips.
        let done = StreamEventWire::from_event(&StreamEvent::Done).expect("Done projects");
        roundtrip(&done);
        let usage = StreamEventWire::from_event(&StreamEvent::Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            cached_tokens: 2,
            cost: 0.5,
        })
        .expect("Usage projects");
        roundtrip(&usage);
        // A client-local UI event is intentionally NOT transferable.
        assert!(
            StreamEventWire::from_event(&StreamEvent::EndpointsError {
                model_id: "m".to_string(),
                error: "x".to_string(),
            })
            .is_none(),
            "endpoint events must not cross the wire"
        );
    }

    #[test]
    fn max_frame_bytes_is_64_mib() {
        assert_eq!(MAX_FRAME_BYTES, 64 * 1024 * 1024);
    }
}

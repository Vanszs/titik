//! App – top-level module wiring.
//!
//! Exposes the sub-modules that together own the application's lifecycle:
//!
//! - [`awareness`] – project-doc summarisation for the self-awareness block
//! - [`mode`] – [`Mode`] enum and associated per-mode state types
//! - [`runtime`] – event loop, terminal setup/teardown, and the main `run` function that ties controller + view together
//! - [`state`] – [`AppState`] (mode + rest) and [`AppStateRest`] (shared fields used across all modes: messages, input, client, …)
//!
//! [`run`] is re-exported at this level so callers only need `app::run(opts)`.

pub mod awareness;
pub mod mode;
pub mod runtime;
pub mod state;

pub use runtime::run;

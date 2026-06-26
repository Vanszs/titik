//! Sandboxed filesystem tools: list / read / write / delete.
//!
//! Every path argument is resolved through [`super::resolve`], which pins it
//! inside the session workspace — a tool can never read or write outside it.
//! These structs implement [`Tool`] and are advertised to the model via
//! [`super::all_tools`]; the agentic loop dispatches the model's requested calls
//! through [`Tool::run`].

mod helpers;
pub mod dirlist;
pub mod read;
pub mod write;
pub mod edit;
pub mod delete;

pub use dirlist::DirList;
pub use read::Read;
pub use write::Write;
pub use edit::Edit;
pub use delete::Delete;

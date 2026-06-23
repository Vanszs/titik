//! App-wide compile-time constants.
//!
//! These values are used across the codebase for defaults and identity strings.
//! None of them are user-configurable at runtime — they represent sensible
//! starting points that the user can override via the credentials form.

/// Base URL of the OpenRouter API endpoint.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Model identifier sent to OpenRouter when the user hasn't specified one.
pub const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";

/// How many most-recent messages to keep when compacting the conversation.
///
/// `/compact` preserves the system prompt plus the last `DEFAULT_PRESERVE_N`
/// turns so the model retains recent context while the token count shrinks.
pub const DEFAULT_PRESERVE_N: usize = 6;

/// Value sent as the `HTTP-Referer` header with every OpenRouter request.
///
/// OpenRouter uses this to attribute usage to the originating project.
pub const HTTP_REFERER: &str = "https://github.com/simple-coders";

/// Human-readable application name (displayed in the TUI title bar).
pub const APP_TITLE: &str = "simple-coders-agent";

/// Name of the hidden directory created in the user's home folder to store
/// session files and configuration.
pub const APP_DIR_NAME: &str = ".simple-coder";

//! The `task` tool: delegate work to a background sub-agent.
//!
//! This tool is advertised to the MAIN model so it can hand a self-contained
//! task to a named agent. It is NEVER dispatched through [`Tool::run`]: the
//! runtime (`app::runtime::stream::process_tools`) intercepts a `task` call
//! BEFORE the generic classify/dispatch path, spawns the sub-agent in the
//! background, and returns immediately so the main turn continues. The `run`
//! impl exists only to satisfy the [`Tool`] trait and must never be reached.

use anyhow::Result;
use serde_json::{json, Value};
use super::{Tool, ToolCtx};

/// Delegate a task to a named sub-agent that runs in the background.
pub struct Task;
impl Tool for Task {
    fn name(&self) -> &'static str { "task" }

    fn description(&self) -> &'static str {
        "Delegate a self-contained task to a named specialist sub-agent that runs \
         autonomously to completion and returns its FULL report as this tool's result \
         for you to read and react to. You MAY call this tool MULTIPLE times in a \
         single turn to run several sub-agents IN PARALLEL (up to 5 at once) — each \
         returns its own report. Use it to offload exploration, research, or mechanical \
         edits. The `agent` argument must be one of the sub-agents listed in your \
         system prompt."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Name of the sub-agent to delegate to (must be one listed under # Sub-agents in your system prompt)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full task instruction for the sub-agent."
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    fn run(&self, _ctx: &ToolCtx, _args: &Value) -> Result<String> {
        // Intercepted by the runtime before dispatch; never actually called.
        Ok("error: task tool must be handled by the runtime".into())
    }
}

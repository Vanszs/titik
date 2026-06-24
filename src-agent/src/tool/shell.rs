//! Shell tool: run arbitrary commands in the workspace.
//!
//! `bash` is RISKY — it requires user approval in Normal mode. Its safety
//! relies entirely on the approval gate, not path-sandboxing (unlike the
//! filesystem tools). Output is captured (stdout + stderr) and capped at the
//! last 8 000 characters so verbose build output doesn't flood the context.

use anyhow::Result;
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use super::{Tool, ToolCtx};

/// Run a shell command in the workspace directory.
pub struct Bash;
impl Tool for Bash {
    fn name(&self) -> &'static str { "bash" }
    fn description(&self) -> &'static str {
        "Run a shell command in the workspace. Use for git, cargo, build, and tests. Output is captured (stdout+stderr)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run (passed to sh -c)."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Timeout in milliseconds (default 120000)."
                }
            },
            "required": ["command"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let command = args.get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required string argument 'command'"))?;

        let timeout_ms: u64 = args.get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120_000);

        // Spawn the child, capturing stdout + stderr.
        let child = match Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Ok(format!("error: failed to spawn command: {e}")),
        };

        // Wait with timeout using a helper thread + channel.
        let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
        // `child` is moved into the thread; we get the result back via the channel.
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });

        let timeout = std::time::Duration::from_millis(timeout_ms);
        let output = match rx.recv_timeout(timeout) {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Ok(format!("error: command failed: {e}")),
            Err(_) => {
                // The child thread owns the child now — we can't kill it here, but
                // we still return a timeout message to the model so the turn doesn't
                // stall. The thread will drain on its own when the child finishes.
                return Ok(format!("command timed out after {timeout_ms}ms"));
            }
        };

        // Combine stdout + stderr into one string.
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));

        // Cap to the LAST 8 000 chars so big build logs don't flood the context.
        const MAX_CHARS: usize = 8_000;
        let truncated;
        let tail: String = if combined.chars().count() > MAX_CHARS {
            truncated = true;
            combined.chars().rev().take(MAX_CHARS).collect::<String>()
                .chars().rev().collect()
        } else {
            truncated = false;
            combined
        };

        let exit_code = output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".to_string());

        let mut out = String::new();
        if truncated {
            out.push_str("... (output truncated to last 8000 chars; redirect to a file and read it if you need the full output)\n");
        }
        out.push_str(&tail);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("exit code: {exit_code}"));
        Ok(out)
    }
}

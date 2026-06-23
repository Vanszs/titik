# opencode Tools & Agent-Loop Reference

A practical implementation reference for porting opencode's built-in **Read**, **Write**, and **Bash** tools plus its function-calling loop and abort handling to a Rust agent.

> Source: local clone at `/media/wangsa/project-x/opencode/`
> Version: `packages/opencode` `1.17.9` · commit `5152150bf` ("make ACP resource text sourcing cross-platform")
> Stack: Bun/TypeScript, **Effect** runtime, Vercel **AI SDK** (`ai` + `@ai-sdk/provider`) for the LLM stream.

---

## Overview

opencode defines every tool with `Tool.define(id, Effect)` in `packages/opencode/src/tool/tool.ts`. Each tool is a `Tool.Def` (`tool.ts:55-65`):

```ts
{ id, description, parameters /* Effect Schema */, jsonSchema?, execute }
```

Design facts that matter for a port:

- **LLM-facing description strings live in separate `.txt` files** next to each `.ts` (`read.txt`, `write.txt`, `shell/shell.txt`). Keep these as data, not inline strings — opencode renders some of them per-shell/per-OS at runtime.
- **Parameter schemas are Effect `Schema`** objects, compiled to JSON Schema at registration time. Plugin tools may instead expose zod, which is boxed to JSON Schema at the registry boundary.
- **A shared `wrap` (`tool.ts:99-149`)** decodes args against the schema, raises a uniform "invalid arguments" error on mismatch, and post-truncates any tool output that didn't already mark itself truncated.
- **All `execute` bodies end in `.pipe(Effect.orDie)`** — failures become Effect defects that surface to the model as tool-error text.
- **Registry**: `packages/opencode/src/tool/registry.ts`. Built-in order (`registry.ts:218-239`): `invalid, question, shell, read, glob, grep, edit, write, task, fetch, todo, search, skill, patch, …`.

Shared invalid-argument message (`tool.ts:24-34`), surfaced verbatim to the model:

```
The ${tool} tool was called with invalid arguments: ${detail}.
Please rewrite the input so it satisfies the expected schema.
```

### Shared truncation service (`tool/truncate.ts`)

Used by both the `wrap` post-processing and the Bash tool.

- Defaults: `MAX_LINES = 2000`, `MAX_BYTES = 50 KB` (`truncate.ts:15-16`). Overridable via config `tool_output.max_lines` / `tool_output.max_bytes`.
- When output exceeds limits, the **full** text is written to a file under `TRUNCATION_DIR`, named `tool_<ascending-id>`, with 7-day retention cleanup (`truncate.ts:13,54-66`).
- The model receives a preview plus a hint. The hint differs by whether the agent has the Task tool (`truncate.ts:129-131`): with Task it tells the model to delegate processing of the full file; without, it tells the model to Grep/Read with offset/limit.

---

## Tool: Read

Files: `tool/read.ts`, `tool/read.txt`. Tool id: `"read"` (`read.ts:69`).

### 1. Schema / description

Description (verbatim, `read.txt`):

```
Read a file or directory from the local filesystem. If the path does not exist, an error is returned.

Usage:
- The filePath parameter should be an absolute path.
- By default, this tool returns up to 2000 lines from the start of the file.
- The offset parameter is the line number to start from (1-indexed).
- To read later sections, call this tool again with a larger offset.
- Use the grep tool to find specific content in large files or files with long lines.
- If you are unsure of the correct file path, use the glob tool to look up filenames by glob pattern.
- Contents are returned with each line prefixed by its line number as `<line>: <content>`. For example, if a file has contents "foo\n", you will receive "1: foo\n". For directories, entries are returned one per line (without line numbers) with a trailing `/` for subdirectories.
- Any line longer than 2000 characters is truncated.
- Call this tool in parallel when you know there are multiple files you want to read.
- Avoid tiny repeated slices (30 line chunks). If you need more context, read a larger window.
- This tool can read image files and PDFs and return them as file attachments.
```

Parameters (`read.ts:28-36`):

| Param | Type | Description |
|---|---|---|
| `filePath` | `string` | "The absolute path to the file or directory to read" |
| `offset` | `NonNegativeInt?` | "The line number to start reading from (1-indexed)" |
| `limit` | `NonNegativeInt?` | "The maximum number of lines to read (defaults to 2000)" |

Constants (`read.ts:13-19`): `DEFAULT_READ_LIMIT = 2000`, `MAX_LINE_LENGTH = 2000` (suffix `... (line truncated to 2000 chars)`), `MAX_BYTES = 50 KB`, `SAMPLE_BYTES = 4096` (MIME sniff), image MIMEs `image/jpeg|png|gif|webp`.

### 2. Execution (`read.ts:229-377`)

- **Path resolution** (`read.ts:234-241`): if not absolute, `path.resolve(instance.directory, filePath)`. Title is path relative to the worktree.
- **Permission** (`read.ts:255-260`): `ctx.ask({ permission: "read", patterns: [relPath], always: ["*"] })`.
- **Sandboxing** (`read.ts:250-253`): `assertExternalDirectoryEffect` — target outside the worktree triggers an `external_directory` permission prompt (unless `ctx.extra.bypassCwdCheck`).
- **Missing file** (`read.ts:76-99,262`): fuzzy-matches up to 3 sibling names → `File not found: <path>\n\nDid you mean one of these?\n…`, else `File not found: <path>`.
- **Directory** (`read.ts:264-297`): lists entries (subdirs get trailing `/`), sorted, sliced by offset/limit, wrapped in `<path>…<type>directory</type><entries>…</entries>`.
- **Images/PDF** (`read.ts:306-325`): MIME sniffed from a 4 KB sample; returns `"Image read successfully"` / `"PDF read successfully"` plus a base64 `data:` attachment.
- **Binary detection** (`read.ts:182-227`): by extension (zip/exe/so/docx/wasm/pyc/…) or heuristic (null byte → binary; >30% non-printable → binary). Binary throws `Cannot read binary file: <path>`.
- **Line reading** (`read.ts:137-180`): streams the file and splits lines with a manual `TextDecoder` (deliberately avoiding helpers that drop the final unterminated line). Starts at `offset-1`; caps at `limit` lines AND at `MAX_BYTES` cumulative bytes; lines >2000 chars are truncated with the suffix; a `ReadStop` tagged error halts the stream at the byte cap.
- **Offset out of range** (`read.ts:332-336`): `Offset N is out of range for this file (M lines)`.

### 3. Output format (`read.ts:338-357`)

```
<path>/abs/path</path>
<type>file</type>
<content>

<offset>: line
<offset+1>: line
…

(End of file - total <count> lines)
</content>
```

Line prefix is `${lineNumber}: ${line}`. Trailer is one of:
- `(Output capped at 50 KB. Showing lines A-B. Use offset=N to continue.)` (byte cap)
- `(Showing lines A-B of <count>. Use offset=N to continue.)` (line-limit)
- `(End of file - total <count> lines)`

Metadata carries `preview` (first 20 lines), `truncated`, `loaded`, and a `display` object (lineStart/lineEnd/totalLines). Matched instruction files append a `<system-reminder>…</system-reminder>`.

### 4. Safety / permissions

`read` permission prompt per file (default `always: ["*"]`), plus `external_directory` prompt for out-of-worktree paths. No allow/deny banlist.

### 5. Errors

Thrown `Error`s become Effect defects (`Effect.orDie`) relayed to the model: `File not found: …`, `Cannot read binary file: …`, `Offset N is out of range …`. Schema mismatch → the shared invalid-arguments message.

---

## Tool: Write

Files: `tool/write.ts`, `tool/write.txt`. Tool id: `"write"` (`write.ts:28`).

### 1. Schema / description

Description (verbatim, `write.txt`):

```
Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.
```

> Note: the "must Read first" rule is stated in the prompt but **not enforced in `write.ts`** — there is no read-tracking check in this handler (that gate lives on the `edit` tool, not here).

Parameters (`write.ts:20-25`):

| Param | Type | Description |
|---|---|---|
| `content` | `string` | "The content to write to the file" |
| `filePath` | `string` | "The absolute path to the file to write (must be absolute, not relative)" |

### 2. Execution (`write.ts:38-101`)

- **Path** (`write.ts:41-43`): absolute kept; else `path.join(instance.directory, filePath)`.
- **Sandbox** (`write.ts:44`): `assertExternalDirectoryEffect`.
- **Existing-file / overwrite** (`write.ts:46-52`): `fs.existsSafe`; reads old content (BOM-aware via `Bom.readFile`) for the diff; preserves a BOM if either old or new content had one. Always overwrites.
- **Permission** (`write.ts:54-62`): `ctx.ask({ permission: "edit", patterns: [relPath], always: ["*"], metadata: { filepath, diff } })`. Diff via `createTwoFilesPatch` + `trimDiff`.
- **Write + dirs** (`write.ts:64`): `fs.writeWithDirs` creates parent directories automatically; then runs a formatter (`format.file`) and re-syncs the BOM if formatting changed it.
- **Events** (`write.ts:68-72`): publishes `FileSystem.Event.Edited` and `Watcher.Event.Updated` (`add` if new, else `change`).
- **LSP diagnostics** (`write.ts:75-90`): after writing, collects diagnostics; appends `LSP errors detected in this file, please fix:` for the written file and `LSP errors detected in other files:` for up to `MAX_PROJECT_DIAGNOSTICS_FILES = 5` other files.

### 3. Output (`write.ts:74,92-100`)

Base string `"Wrote file successfully."` (+ appended LSP blocks). Metadata `{ diagnostics, filepath, exists }`. Title = path relative to worktree.

### 4. Safety / permissions

`edit` permission prompt carrying a diff in metadata; `external_directory` prompt for out-of-worktree paths.

### 5. Errors

Thrown errors → Effect defects → model. Schema mismatch → shared invalid-arguments message.

---

## Tool: Bash (internally "shell")

Files: `tool/shell.ts`, prompt builder `tool/shell/prompt.ts`, template `tool/shell/shell.txt`, id `tool/shell/id.ts`.

> The tool id is literally `"bash"` (`id.ts`: `ToolID = "bash"`, kept for plugin/permission compatibility — comment says "Rename with opencode 2.0"). The permission key is also `"bash"`.

### 1. Schema / description

The description is **rendered per shell at runtime** (`prompt.ts` `render`/`profile`), template in `shell/shell.txt`. There are full profiles for bash, pwsh (PowerShell 7+), powershell (5.1), and cmd.exe (`prompt.ts:43-271`), each with shell-specific chaining/quoting notes.

Template skeleton (`shell.txt`):

```
${intro}

Be aware: OS: ${os}, Shell: ${shell}

${workdirSection}

Use `${tmp}` for temporary work outside the workspace. This directory has already been created, already exists, and is pre-approved for external directory access.

IMPORTANT: This tool is for terminal operations like git, npm, docker, etc. DO NOT use it for file operations (reading, writing, editing, searching, finding files) - use the specialized tools for this instead.

${commandSection}

# Git and GitHub
- Only commit, amend, push, or create PRs when explicitly requested.
- Before committing, inspect `git status`, `git diff`, and `git log --oneline -10`; stage only intended files and never commit secrets.
- ...
- Use `gh` for GitHub tasks, including PRs, issues, checks, and releases; return the PR URL when done.
```

Bash `intro` (`prompt.ts:258-259`):

```
Executes a given bash command in a persistent shell session with optional timeout, ensuring proper handling and security measures.
```

Bash `${commandSection}` (`bashCommandSection`, `prompt.ts:78-118`) notable guidance:
- "If not specified, commands will time out after `${defaultTimeoutMs}`ms."
- "If the output exceeds `${maxLines}` lines or `${maxBytes}` bytes, it will be truncated and the full output will be written to a file. … Do NOT use `head`, `tail`, … the full output will already be captured to a file."
- Bans/discourages `find, grep, cat, head, tail, sed, awk, echo` → use Glob/Grep/Read/Edit/Write instead (`prompt.ts:100-106`).
- "AVOID using `cd <directory> && <command>`. Use the workdir parameter" (`prompt.ts:112`).
- Parallel calls in one message for independent commands; `&&` to chain dependent; `;` only when failure doesn't matter; no newlines between commands.

Parameters (`shell/prompt.ts:15-23`):

| Param | Type | Description |
|---|---|---|
| `command` | `string` | "The command to execute" |
| `timeout` | `PositiveInt?` | "Optional timeout in milliseconds" |
| `workdir` | `string?` | "The working directory to run the command in. Defaults to the current directory. Use this instead of 'cd' commands." |

### 2. Execution

**Limits / constants:**
- `MAX_METADATA_LENGTH = 30_000` (`shell.ts:27`) — metadata `output` preview is the last 30 KB.
- Output `maxLines`/`maxBytes` from the Truncate service (defaults 2000 lines / 50 KB, config-overridable).
- **Default timeout** = `flags.bashDefaultTimeoutMs ?? 2 * 60 * 1000` → **120000 ms** (`shell.ts:347`). **No hard max** — any positive value accepted; negative throws `Invalid timeout value: <n>. Timeout must be a positive number.` (`shell.ts:615-617`). Actual kill fires at `timeout + 100 ms` (`shell.ts:540`).

**Shell selection / spawn** (`shell.ts:293-310,597-604`):
- Shell from config (`Shell.acceptable(cfg.shell)`).
- win32 + PowerShell: `shell -NoLogo -NoProfile -NonInteractive -Command <cmd>`.
- Otherwise: `ChildProcess.make(command, [], { shell, cwd, env, stdin: "ignore", detached: process.platform !== "win32" })` — POSIX runs **detached** (own process group), stdin ignored.
- **cwd**: `params.workdir` resolved against `instance.directory`, else `instance.directory` (`shell.ts:611-614`).
- **env**: `process.env` merged with plugin `shell.env` hook output (`shell.ts:416-426`).

**Output capture & truncation** (`shell.ts:428-595`):
- Captures stdout+stderr combined via `handle.all`, decoded as text (`shell.ts:486-487`).
- Rolling buffer capped at `maxBytes * 2`, dropping oldest chunks (`cut = true`) once exceeded (`shell.ts:491-496`).
- Streams live preview to `ctx.metadata({ output: last })` where `last` = last 30 KB.
- When in-memory `full` exceeds `maxBytes`, spills full output to a truncation file and keeps appending there (`shell.ts:504-522`).
- Final shaping (`shell.ts:568-594`): `tail(raw, maxLines, maxBytes)` keeps the **tail**. If cut and a file exists, prefixes `...output truncated...\n\nFull output saved to: <file>\n\n` + tail.
- Empty output → `"(no output)"`.

> **No background-process feature** in this handler — there is no `run_in_background` param. Long-running commands are governed only by the timeout. (A separate `BackgroundJob` system exists elsewhere but is not wired into this tool's params.)

### 3. Output format

Final text per above. Metadata `{ output: <30KB preview>, exit: <code|null>, truncated, outputPath? }`. Title = the command string.

Timeout appends:
```
<shell_metadata>shell tool terminated command after exceeding timeout <n> ms. If this command is expected to take longer … retry with a larger timeout value in milliseconds.</shell_metadata>
```
User abort → `User aborted the command`.

### 4. Safety / permissions

There is **no static dangerous-command denylist** — everything routes through the permission system, scoped by a **tree-sitter** parse of the command (`shell.ts:311-336`):
- `collect` (`shell.ts:378-414`) walks each command node. File-touching commands (`FILES` set: rm/cp/mv/mkdir/touch/chmod/chown/cat + PowerShell `*-Item`/`*-Content`; `CMD_FILES`: copy/del/dir/move/rd/ren/type/…) have their path args resolved; any path outside the instance dir adds an `external_directory` prompt.
- Every non-`cd` command adds a permission pattern keyed by the command text, plus an `always` glob of the command prefix + `*` (via `BashArity.prefix`), so a user can permanently allow e.g. `git status *`.
- `ask` (`shell.ts:263-291`) raises `external_directory` prompts for out-of-tree dirs, then a `bash` permission prompt for the command patterns.

### 5. Errors

Process-level failures (nonzero exit) are returned as normal output with the exit code in metadata; thrown errors (invalid timeout, spawn failure) become Effect defects relayed to the model. Schema mismatch → shared invalid-arguments message.

---

## Agent Loop

Key files (all under `packages/opencode/src/`):
- Loop: `session/prompt.ts`
- Stream/runtime: `session/llm.ts`
- AI-SDK event adapter: `session/llm/ai-sdk.ts`
- Stream consumer / tool results: `session/processor.ts`
- Tool→AI-SDK conversion: `session/tools.ts`
- Effect↔Promise bridge: `effect/bridge.ts`
- Cancellation primitive: `effect/runner.ts`
- Run-state / cancel entry: `session/run-state.ts`

> **SDK:** Vercel AI SDK is the default. An opt-in native runtime (`@opencode-ai/llm`) exists behind `OPENCODE_EXPERIMENTAL_NATIVE_LLM`. The whole stack runs inside the **Effect** runtime; abort is Effect fiber interruption with an `AbortController`/`AbortSignal` derived from it.

### 6. Tool registration & provider format

- Built-in tools are constructed once into an `InstanceState` cache, concatenated with custom/plugin tools (`registry.ts:198-246`).
- `registry.tools()` filters the list per request by model/agent (e.g. GPT models swap `edit`/`write` for `apply_patch`; web-search is provider-gated) and lets plugins mutate definitions via a `"tool.definition"` hook (`registry.ts:267-307`).
- **Schema → JSON Schema** (`tool/json-schema.ts:8-26`): use explicit `jsonSchema` if present, else compile the Effect schema via `Schema.toJsonSchemaDocument` and normalize. Plugin tools exposing zod are boxed at the registry boundary — zod → JSON Schema, schema replaced with an opaque validator (`registry.ts:118-126`).
- **Conversion to AI SDK `tool()`** (`tools.ts:14,86-127`):

```ts
const schema = ProviderTransform.schema(input.model, ToolJsonSchema.fromTool(item))
tools[item.id] = tool({
  description: item.description,
  inputSchema: jsonSchema(schema),
  execute(args, options) {
    return run.promise(Effect.gen(function* () {
      const ctx = context(args, options)
      yield* plugin.trigger("tool.execute.before", { tool: item.id, sessionID: ctx.sessionID, callID: ctx.callID }, { args })
      const result = yield* item.execute(args, ctx)
      ...
```

The AI SDK validates/parses arguments against the JSON Schema and invokes `execute` with parsed `args` plus `ToolExecutionOptions` (`toolCallId`, `abortSignal`, `messages`). The `execute` closure crosses the Effect/Promise boundary via `EffectBridge` (`bridge.ts:54-82`): `run.promise(effect)` runs `Effect.runPromise` while preserving workspace/instance context. The AI-SDK `options.abortSignal` becomes `ctx.abort` (`tools.ts:53-58`).

Passed to the model in `streamText` (`llm.ts:316-321`):

```ts
activeTools: Object.keys(prepared.tools).filter((x) => x !== "invalid"),
tools: prepared.tools,
toolChoice: input.toolChoice,
maxOutputTokens: prepared.params.maxOutputTokens,
abortSignal: input.abort,
```

### 7. The autonomous turn

There are **two nested loops** — the most important porting detail.

**Outer loop (opencode-owned) — `runLoop` (`prompt.ts:1184-1437`).** A manual `while (true)` that re-derives state and re-calls the model each iteration. It does **not** rely on AI-SDK `maxSteps`/`stopWhen`.

1. Loads/filters messages, finds last user/assistant (`prompt.ts:1195-1199`).
2. **Exit condition** (`prompt.ts:1209-1233`): break only if the last assistant finished with a reason that is **not** `"tool-calls"`, there are no pending tool calls, and the user message predates the assistant. Some providers return `"stop"` even with tool calls, so it keeps looping to feed results back:

```ts
const hasToolCalls = lastAssistantMsg?.parts.some(
  (part) => part.type === "tool" && !part.metadata?.providerExecuted && !isOrphanedInterruptedTool(part)) ?? false
if (lastAssistant?.finish && !["tool-calls"].includes(lastAssistant.finish) && !hasToolCalls && lastUser.id < lastAssistant.id) { /* break */ }
```

3. **Max steps** is agent-driven (`prompt.ts:1281-1282`): `const maxSteps = agent.steps ?? Infinity`. On the last step it appends a `MAX_STEPS_PROMPT` assistant message to force a wrap-up (`prompt.ts:1376-1378`).
4. Resolves tools, builds system prompt, converts messages, calls `handle.process({...})` (`prompt.ts:1368-1382`).
5. Maps the processor result: `"break"`/`"stop"` exits, `"compact"` triggers compaction, `"continue"` loops (`prompt.ts:1384-1431`). Subtasks (`task` tool) run as a nested `prompt`.

Entered via `loop → state.ensureRunning(...)` (`prompt.ts:1439-1443`) on a single per-session `Runner` fiber.

**Inner step (AI SDK) — `processor.process` (`processor.ts:960-1034`).** `llm.stream(...)` returns an `LLMEvent` stream; the processor drains it:

```ts
const stream = llm.stream(streamInput)
yield* stream.pipe(
  Stream.tap((event) => handleEvent(event)),
  Stream.takeUntil(() => ctx.needsCompaction),
  Stream.runDrain,
)
```

Default path calls AI-SDK `streamText(...)` and converts `result.fullStream` into `LLMEvent`s via `LLMAISDK.toLLMEvents` (`ai-sdk.ts:76-286`). Because `streamText` gets the `tools` with `execute` callbacks, **the AI SDK runs tools internally**, emitting `tool-input-start/delta/end`, `tool-call`, then `tool-result`/`tool-error`, plus `start-step`/`finish-step`/`finish`. One `streamText` call can perform multiple internal steps.

- **Parsing & results** (`processor.ts:371-844`): `tool-call` creates/updates a running tool part (with a **doom-loop guard** — 3 identical consecutive calls trigger a `doom_loop` permission ask, `processor.ts:468-547`); `tool-result` writes completed output back; `tool-error`/`provider-error` fail it.
- **Sequential vs parallel:** the AI SDK executes a step's tool calls **in parallel** (all `execute` callbacks concurrently); opencode's processor consumes the resulting event stream **sequentially** (`Stream.tap` per event). Tool results are re-injected into the model **by the AI SDK automatically** between internal steps — opencode does not manually re-feed them; the outer loop only re-streams for finish/compaction/subtask control.
- **Finish detection** (`processor.ts:693-757`): records `value.reason` as `assistantMessage.finish`. `process` returns (`processor.ts:1030-1032`):

```ts
if (ctx.needsCompaction) return "compact"
if (ctx.blocked || ctx.assistantMessage.error) return "stop"
return "continue"
```

The outer loop treats finish reasons `"tool-calls"` and `"unknown"` as "keep going"; `content-filter` is surfaced as an error (`prompt.ts:1391-1404`). Provider `maxRetries` is `0` (`llm.ts:323`) — retries are opencode's own `SessionRetry.policy` around the stream (`processor.ts:994-1025`).

### 8. Abort / interrupt

Two layers: Effect fiber interruption is the source of truth, and an `AbortController`/`AbortSignal` is derived from it for AI-SDK and process code.

- **Entry:** `SessionPrompt.cancel → state.cancel(sessionID)` (`prompt.ts:156-159`) → `SessionRunState.cancel` (`run-state.ts:77-86`), which also cancels background jobs for the session/subagents (`run-state.ts:116-148`).
- **Runner interrupt** (`runner.ts:171-202`): the loop runs as a fiber; `Runner.cancel` does `Fiber.interrupt(st.run.fiber)`, propagating interruption down the whole effect tree (loop → processor → stream → tool `execute`). On interrupt the runner returns its `onInterrupt` effect (the last assistant message) so callers get a clean result.
- **Signal → AI SDK** (`llm.ts:357-381`): `LLM.stream` allocates an `AbortController` via `Effect.acquireRelease`; the release calls `ctrl.abort()` on scope close. Fiber interrupt → scope finalizes → `ctrl.abort()` fires the `abortSignal` passed to `streamText`, stopping the in-flight provider request. The same signal is handed to each tool's `options.abortSignal`.
- **Processor side** (`processor.ts:981-989`): the drain is wrapped with `Effect.onInterrupt` that marks the message aborted and halts with a `DOMException("Aborted","AbortError")`. `cleanup()` (`processor.ts:846-915`, via `Effect.ensuring`) awaits each in-flight tool's `done` Deferred for 250 ms, then marks still-running tool parts `status: "error", error: "Tool execution aborted", metadata.interrupted: true`. These "orphans" are skipped by the outer loop's exit check (`isOrphanedInterruptedTool`, `prompt.ts:100-104`).
- **Running processes on abort:** the shell tool races the process against the abort signal and timeout, and **kills the OS process** on abort (`shell.ts:533-555`):

```ts
const exit = yield* Effect.raceAll([
  handle.exitCode.pipe(Effect.map((code) => ({ kind: "exit" as const, code }))),
  abort.pipe(Effect.map(() => ({ kind: "abort" as const, code: null }))),
  timeout.pipe(Effect.map(() => ({ kind: "timeout" as const, code: null }))),
])
if (exit.kind === "abort") {
  aborted = true
  yield* handle.kill({ forceKillAfter: "3 seconds" }).pipe(Effect.orDie)
}
```

`forceKillAfter: "3 seconds"` = graceful kill, then SIGKILL.

---

## Porting Notes for a Rust Implementation

**What maps to what**

| opencode | Rust equivalent |
|---|---|
| Effect `Schema` params + `jsonSchema()` | `serde` structs + `schemars::JsonSchema` to emit JSON Schema for the tool definition |
| `Tool.define(id, execute)` | a `Tool` trait: `name()`, `description()`, `parameters_schema()`, `async execute(args, ctx) -> Result<ToolOutput>` |
| `.txt` description files | keep descriptions as data (string consts or template files), render per-OS/shell where needed |
| AI SDK `streamText` auto-executing tools | you must do this yourself: parse `tool_calls` from the provider response, dispatch, append tool-result messages, re-call |
| Effect fiber interrupt + `AbortController` | a single `tokio_util::sync::CancellationToken` fanned out to the HTTP request and each running tool |
| `ChildProcess` detached + `forceKillAfter` | `tokio::process::Command` with `process_group(0)` on Unix; on cancel send SIGTERM then SIGKILL after a deadline |

**Keep these (they are load-bearing prompt/behavior details)**

- The exact Read description rules: absolute paths, 2000-line default, 1-indexed `offset`, `<line>: <content>` numbering, 2000-char line truncation, parallel-read hint.
- Read's `<path>/<type>/<content>` envelope and the three trailer variants — they tell the model whether to paginate.
- Write's "overwrite + must-Read-first" prompt language (even if you choose not to enforce the read gate, as opencode itself doesn't in the handler).
- Bash defaults: **120000 ms** default timeout, 2000-line / 50 KB output cap with full output spilled to a file, the "don't use find/grep/cat/head/tail; use the specialized tools" guidance, and the `workdir` param instead of `cd`.
- The two-layer loop: own your step counter and exit condition (`tool-calls`/`unknown` ⇒ continue; anything else ⇒ stop). Don't outsource turn control entirely to a provider-side `maxSteps`.
- Abort fan-out: one cancel token → (a) provider request, (b) per-tool signal → OS process kill (graceful, then hard kill on a deadline), (c) cleanup that marks orphaned tool calls so the loop's exit check ignores them and the conversation stays well-formed.

**What you can simplify**

- **Effect runtime** — replace with `async`/`tokio`. The fiber-interrupt machinery collapses into a `CancellationToken` plus `select!`/`tokio::select!`.
- **Tree-sitter command parsing for permissions** — opencode uses it for fine-grained per-command allow rules. A simpler first pass: parse the command prefix (first token / `cmd args` shape) for an allow/deny list, and skip full AST scoping until you need per-path sandboxing.
- **Plugin hooks** (`tool.definition`, `tool.execute.before`, `shell.env`) — drop unless you need extensibility.
- **LSP diagnostics on Write** — nice-to-have feedback loop; optional for a first port.
- **BOM preservation, formatter-on-write, watcher events** — optional polish.
- **Truncation-to-file with 7-day cleanup** — you still want output caps, but can start by truncating in-memory with a clear marker before adding the spill-to-file + retention machinery.
- **doom-loop guard** (3 identical calls) — cheap and worth keeping, but not required for a first cut.

**Provider tool-call format reminder**

opencode leans on the AI SDK to translate between provider-specific shapes. In Rust you talk to the provider directly, so:
- Emit tool definitions as `{ name, description, input_schema }` (Anthropic) / `{ type: "function", function: { name, description, parameters } }` (OpenAI-style).
- Parse `tool_use`/`tool_calls` blocks from the response, run them, and append `tool_result`/`role:"tool"` messages keyed by the call id before the next request.
- Loop until the response has no tool calls and a non-tool finish reason.

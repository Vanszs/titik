# titik

**Your terminal, with a brain.**

titik is a fast, native AI coding agent that lives in your terminal — reading your code, shipping features, and running your tools without you ever leaving the command line.

→ **[koma.run](https://koma.run)** (upstream)

This project is a fork of [aula-id/koma](https://github.com/aula-id/koma). All credit for the original idea, design, and implementation goes to the upstream maintainers.

---

## License

titik is an Apache 2.0 fork of [aula-id/koma](https://github.com/aula-id/koma). See [LICENSE](LICENSE) for the full text.

---

## Why titik

- **Native and fast.** Written in Rust. No Electron, no browser tab, no lag — a crisp TUI that starts instantly and stays out of your way.
- **It actually does the work.** titik reads your code, edits files, runs commands, and verifies its own changes. You orchestrate; it executes.
- **Bring your own models.** Wire up your providers and assign different models to different roles — planning, coding, review — and switch on the fly.
- **Yours to control.** Every tool call runs behind an approval gate you set. Nothing touches your machine without your say-so.

## What's inside

**Parallel sub-agents.** Hand a chunk of work to agents that run side by side, then fold their results back in. Big refactors, broad audits, multi-file sweeps — fanned out, not serialized.

**Background jobs.** Fire off long-running commands and keep working. titik watches them, lets the agent grep and tail their output, and nudges it the moment they finish.

**Multi-session, detachable.** Run many sessions at once, each in its own tab. Detach the daemon, close the laptop, come back later — your work is exactly where you left it.

**Internet access.** Search and fetch from the web inline, or flip to Full mode for real browser-powered scraping when a page fights back.

**Security toolkit.** A curated, opt-in suite of security tools wired straight into the agent for authorized testing and research.

**MCP-ready.** Connect any Model Context Protocol server and its tools show up for the agent automatically.

**Memory that sticks.** A per-project memory carries conventions, decisions, and context across sessions — so titik stops relearning your codebase every morning.

**Vision.** Paste a screenshot. titik sees it.

**Cost in plain sight.** A live usage dashboard shows exactly what every turn costs, backed by a full ledger you can audit.

**Self-updating.** Run `titik update` and you're on the latest in seconds.

## Get titik

```sh
curl -fsSL https://koma.run/install.sh | sh
```

Installs to `~/.local/bin` — no sudo required. Then run `titik` and start a session.

More at **[koma.run](https://koma.run)** (upstream).

---

*Curious how it works under the hood? See [`ARCHITECTURE.md`](ARCHITECTURE.md).*

# Lumina — local-first AI coding & open-world agent harness

See `plan.md` for the full architecture & implementation plan,
and `test-plan.md` for the verification matrix (all suites PASS).

## Quickstart

```bash
make build                       # web SPA + static Go binary -> bin/lumina
./bin/lumina --help
./bin/lumina serve                # daemon on 127.0.0.1:7432
./bin/lumina run "explain this repo" --agent plan
```

BYO-LLM is configured in `~/.config/lumina/config.toml` (OpenAI-compatible,
Anthropic-compatible, Ollama, OpenAI-Responses, and **Copilot** protocols;
keys only via `api_key_env` / `api_key_cmd` — plaintext keys are rejected at load).

## Terminal TUI (`lumina`)

The TUI talks to the daemon over the typed SDK. Keys:

- `Tab` / `Shift+Tab` — flip primary agent (`plan`/`build`) / cycle
  approval mode (`ask` → `auto-edit` → `auto-approve` → `bypass`)
- `/` — autocomplete popup listing slash commands with descriptions
  (built-ins plus workspace `.lumina/commands/*.md`), `@file` paths and
  `[[memory-note]]` names; `Tab` accepts, `↑↓` navigate, `Esc` dismisses
- `/connect` — pick a Copilot provider, enter the GitHub host, open the
  link, auto-poll to connected; `/model` — live model picker

```toml
# GitHub Copilot: per-user endpoint (SaaS default; override for Enterprise/GHES)
[providers.copilot]
protocol = "copilot"
base_url = "https://api.githubcopilot.com"   # optional override
# token_url = "https://HOST/api/v3/copilot_internal/v2/token"  # GHES only
api_key_cmd = "gh auth token"                # or api_key_env = "GH_TOKEN"

[router]
executor = "copilot/gpt-4o"
```

Connect interactively: TUI `/model` (pick a live model, persists), TUI
`/connect` (pick provider → enter GitHub URL → open link → auto-poll to
connected), web sidebar **Copilot web auth** (device code), or terminal
`lumina auth device --provider copilot [--github-url URL]` (prints URL +
code, stores the token `0600` daemon-side, never shown). List live models
any time with `lumina models --provider copilot`.

## Web mode (`lumina web`)

`lumina serve` hosts the daemon **and** the embedded React workspace — a
dark UI: top bar (session menu, live server badge, panel toggles), centered
new-session view (workspace, branch, last-modified + composer), and active
sessions with a right **Review** pane (Unified/Split diffs, Collapse all,
per-file sections) plus Terminal, Graph, Memory, Harness, and Copilot-connect
tabs. Composer footer holds the agent (`plan`/`build`), live model picker
(router tiers + Copilot `/models` + custom id), reasoning-effort variants
(`auto`/`low`/`medium`/`high`/`xhigh`/`max`), an image **attach button**
(vision-capable models describe attached images), and approval mode
(`Default` = ask). `Tab` / `Shift+Tab` flip agent and approval without
leaving the keyboard; the Servers modal switches which daemon the app talks
to (persisted per browser).

Live end-to-end test (headless browser vs daemon on `lumina/muse-spark-1.3-contributor`):
image attached in the composer + "what is shown" prompt answered correctly
by the model, effort variant `xhigh` selected and honored, reply streamed
over SSE and rendered — plus zero console errors and zero non-loopback
network requests:

![Lumina web workspace answering a prompt](docs/web-e2e.png)

Also verified live: multi-client SSE sync (two subscribers + daemon),
shadow-Git `/undo` + `/redo` through the API, plan-mode scoped writes
(`plan.md` allowed, source tree fenced), and session restart persistence.

## Layout

- `cmd/lumina` — CLI: TUI, `run`, `serve`, `web`, `attach`, `session`, `plugin`, `kb`, `evolve`
- `internal/*` — daemon packages (config, server, security, rules, plugins, llm, agent, tools, kb, harness, tui)
- `pkg/sdk` — typed Go client for the daemon HTTP + SSE API
- `web/` — React + Vite + Tailwind workspace SPA (embedded via `//go:embed`)
- `memory/` — durable Markdown memory + `memory/graphify-out/`
- `docs/` — screenshots and test evidence

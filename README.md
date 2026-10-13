# roc — run open code

`roc` runs an AI coding agent inside a throwaway Docker container.
Supported agents are [opencode](https://opencode.ai), [goose](https://block.github.io/goose/),
[Claude Code](https://docs.claude.com/en/docs/claude-code) and [Codex](https://github.com/openai/codex).
Supported models: [LM Studio](https://lmstudio.ai), [Ollama](https://ollama.com), any
OpenAI-compatible API (local or public), or whatever the agent itself is logged in to.

```
roc -write-dir ~/friends_of/planning -read-dir ~/work
```

That command does five things:

1. It checks your directories, then mounts each one into the container at the same path it has on your Mac.
   So `/Users/andrei/friends_of/planning` is `/Users/andrei/friends_of/planning` inside the container too.
   Read dirs are mounted read-only and write dirs read-write. Nothing else from your machine is visible.
2. It **leases one worker** from your model pool. With four copies of `qwen3.8-27b` loaded in
   LM Studio, the workers are `qwen3.8-27b`, `:2`, `:3` and `:4`, shown as
   "Q #1 Agent" through "Q #4 Agent". Four workers means up to four roc sessions, each in its
   own container.
3. It starts an **MCP gateway** on the host. Through it the agent can use Docker
   (limited to containers and images it creates itself), your real browser, live library docs,
   Figma, a running Wails app, Xcode and the iOS simulators: whatever you switched on with
   `roc -stack frontend` or `roc -mcp add …`. The agent never gets the Docker socket, never gets
   shell access to your Mac, and never sees the tokens those servers use. It is also told which
   tools it has and how to use them.
4. It **drops you straight into the agent's TUI**. The agent never asks for approval:
   the container is the sandbox, and the agent is told to keep going until your
   `AGENTS.md` is satisfied.
5. When the agent exits, roc exits too. It removes the agent container, every
   container, image and network the agent created, the session network and the lease. All of
   this is recorded in `~/.local/roc/state.json`, so cleanup still works after a crash.

---

## Contents

- [Why](#why)
- [Quick start](#quick-start)
- [Usage](#usage)
- [Model providers](#model-providers)
- [The worker pool and `roc -list`](#the-worker-pool-and-roc--list)
- [Running several sessions at once](#running-several-sessions-at-once)
- [Seeing and stopping sessions: `-ps` and `-kill`](#seeing-and-stopping-sessions--ps-and--kill)
- [Mounts](#mounts)
- [Agents](#agents)
- [Agent settings and the never-ask default](#agent-settings-and-the-never-ask-default)
- [MCP servers](#mcp-servers)
- [The state file](#the-state-file)
- [Security model](#security-model)
- [Cleanup, recovery and logs](#cleanup-recovery-and-logs)
- [Troubleshooting](#troubleshooting)
- [Migrating from a host opencode install](#migrating-from-a-host-opencode-install)
- [Development](#development)
- [License](#license)

Installation is covered in **[INSTALL.md](INSTALL.md)**.

## Why

Running an agent directly on your laptop leaves you two bad choices. You can approve every
`ls`, `go test` and file edit by hand. Or you can grant broad permissions and hope nothing touches
`~/.ssh` or another project. roc replaces that permission list with hard boundaries:

| Concern | Without roc (host opencode.json) | With roc |
|---|---|---|
| Which files the agent can read | `read` allow/deny globs | Only the directories you mount exist |
| Which files it can change | `edit` globs per extension and per project | Read dirs are mounted `readonly` by the kernel |
| Shell commands | an allowlist of `git status`, `go`, `cargo`, … | Anything goes, inside a container with `--cap-drop=ALL`, `no-new-privileges` and a non-root user |
| Docker | full socket (= root on the host) or nothing | MCP tools scoped to the session's own resources |
| Leftovers | manual | tracked and removed on exit |

## Quick start

```sh
# 1. Install roc and build the agent image (details in INSTALL.md)
make install image

# 2. Answer a few questions (writes ~/.local/roc/state.json and ~/.local/roc/agents/)
roc -init
```

`roc -init` asks what it needs and checks every answer before saving:

```
Which model server are you using? (lmstudio, ollama, openai, none) [lmstudio]
Where is it running? (base URL) [http://127.0.0.1:1234/v1]
Connected. Models I can see: qwen3.8-27b, qwen3.8-27b:2, qwen3.8-27b:3, qwen3.8-27b:4
Which model are you using? [qwen3.8-27b]
Perfect, how many instances are running? [4]
Perfect, we'll use qwen3.8-27b, qwen3.8-27b:2, qwen3.8-27b:3 and qwen3.8-27b:4 for the Q #1 Agent, Q #2 Agent, Q #3 Agent and Q #4 Agent.
How large is each instance's context window? (tokens) [256256]
What directories do you want to read from? (csv list, read-only) ~/work,~/statuses
What directories do you want to write to? (csv list; empty = the directory you run roc from) ~/friends_of/planning,~/friends_of/knowledge
Which coding agent should roc start? (opencode, goose, claudecode, codex) [opencode]

MCP stacks give agents tools for a kind of work:
  frontend  React/Tailwind web apps: live docs, shadcn/ui, Figma, real-browser and DevTools testing
  …
Which MCP stacks should agents get? (csv: frontend, nextjs, wails, ios, data, research, github; empty = none) frontend
```

Answer `5` instances and you get five workers (`…:5`, "Q #5 Agent"). Run `roc -init` again
at any time to change answers; the current values are the defaults. Flags answer their
question up front (`roc -init -ai-model qwen3.8-27b -qty 4 -stack frontend`), and `-init -yes` (or a
non-interactive stdin) accepts every default without asking.

```sh
# 3. Check everything once: Docker, the image, the models, every MCP server and its secrets
roc -doctor

# 4. Check the pool and work
roc -list
# Q #1: available
# Q #2: available
# Q #3: available
# Q #4: available
cd ~/friends_of/planning && roc
```

Everything can also be given as flags. For example:

```sh
roc -ai-host "http://127.0.0.1:1234/v1" -ai-api-token "sk-lm-***" -ai-model "qwen3.8-27b" -qty 4 \
    -state ~/.local/roc/state.json -binary opencode \
    -write-dir "~/friends_of/planning,~/friends_of/knowledge" -read-dir "~/work,~/statuses"
```

## Usage

```
roc [FLAGS] [-- AGENT_ARGS…]
```

You can write flags Go-style with one dash (`-list`) or with two (`--list`). Everything
after `--` is passed to the agent unchanged, for example `roc -- run "fix the failing test"` or `roc -binary codex -- --search`.

### Session flags

| Flag | Description |
|---|---|
| `-write-dir CSV`, `-w` | Read-write directories, comma separated; repeatable. `~` is expanded. |
| `-read-dir CSV`, `-r` | Read-only directories, comma separated; repeatable. |
| `-workdir PATH` | Working directory in the container. Default: your current directory if it is mounted, otherwise the first write dir. |
| `-binary NAME` | `opencode` (default), `goose`, `claudecode` (also `claude`/`claude-code`) or `codex`. |
| `-worker N` | Use worker N. Default: the lowest-numbered available worker. |
| `-wait SECS` | If no worker is free, keep retrying for up to SECS seconds. |
| `-publish CSV` | Publish agent container ports on host `127.0.0.1`, e.g. `5173,8080:80`. Use this to open the agent's dev server in your browser. |
| `-env CSV` | `NAME` passes the host value through; `NAME=VALUE` sets it. |
| `-image IMAGE` | Agent image (default `roc-agent:latest`, env `ROC_IMAGE`). |
| `-keep-images` | Keep the images the agent built or pulled. |
| `-no-mcp` | Skip the MCP gateway. |
| `-with-mcp CSV` | Also use these MCP servers this session, configured or straight from the [catalog](#the-catalog). |
| `-without-mcp CSV` | Leave these configured MCP servers out this session. |
| `-assume-available` | Skip the model server probe and treat every free worker as available. |
| `-save` | Store this run's `-binary`, `-image`, `-read-dir`, `-write-dir` and `-publish` as defaults. |
| `-dry-run` | Print the worker, mounts, `docker run` command and generated agent config, then exit. |

If you pass neither `-read-dir` nor `-write-dir` and have no saved defaults, roc mounts the
current directory read-write.

### Pool flags (saved to the state file)

| Flag | Description |
|---|---|
| `-provider KIND` | `lmstudio` (default), `ollama`, `openai` (any OpenAI-compatible API) or `none` (env `ROC_PROVIDER`). |
| `-ai-host URL` | The server's OpenAI-compatible base URL as seen from the host. Defaults: LM Studio `http://127.0.0.1:1234/v1`, Ollama `http://127.0.0.1:11434/v1` (env `ROC_AI_HOST`). |
| `-ai-model ID` | Model id. On LM Studio, worker *n* > 1 is `ID:n`. |
| `-qty N` | Pool size (1–64). |
| `-ai-api-token TOKEN` | API token for the server. **Never saved.** By default roc reads `$ROC_AI_API_TOKEN`. |
| `-state PATH` | State file (default `~/.local/roc/state.json`, env `ROC_STATE`). Each state file is a separate roc config with its own `agents/` directory. |

### Commands

| Flag | Description |
|---|---|
| `-init` | Guided setup (see [Quick start](#quick-start)). `-init -yes` skips the questions; `-init -force` starts from defaults and keeps a backup. |
| `-list` | One line per worker: `Q #N: running\|available\|offline`. Add `-json` for details. |
| `-ps` | Running sessions by position: worker, agent, session id, uptime, working directory (`-json` too). |
| `-kill TARGET` | Stop sessions: a `-list` position (`2`), a label (`Q#2`), a session id, or `all`. `-force` skips the graceful stop. |
| `-doctor` | Check Docker, the image, the model server, sessions and every enabled MCP server; prints a fix for each problem (`-json` too). |
| `-mcp [ACTION]` | Manage MCP servers: `list`, `catalog`, `stacks`, `add`, `set`, `enable`, `disable`, `rm`, `info`, `test`. See [MCP servers](#mcp-servers). |
| `-stack CSV` | Enable MCP stacks: `frontend`, `nextjs`, `wails`, `ios`, `data`, `research`, `github`. |
| `-agent-config` | Show this config's settings file for `-binary` and where it applies (creates missing files). |
| `-cleanup` | Remove everything left by dead sessions, plus orphaned containers and networks of this config. |
| `-show-state` | Print the state file. |
| `-build-image` | Build the agent image from the Dockerfile embedded in the binary (`-with-playwright` adds Chromium). |
| `-version`, `-help` | |

### Environment variables

| Variable | Effect |
|---|---|
| `ROC_AI_API_TOKEN` | API token for the model server (or the name set in `config.ai.api_token_env`) |
| `ROC_PROVIDER` | same as `-provider` |
| `ROC_AI_HOST` | same as `-ai-host` |
| `ROC_STATE` | same as `-state` |
| `ROC_IMAGE` | same as `-image` |
| `ROC_DOCKER` | docker executable to use (default `docker`; used by the tests) |

Exit status: roc returns the agent's own exit code. It returns `1` for roc errors (bad mounts, no worker, Docker down) and `2` for usage errors.

## Model providers

| `-provider` | Default URL | Workers | Status check |
|---|---|---|---|
| `lmstudio` | `http://127.0.0.1:1234/v1` | Each loaded copy of the model is one worker: `model`, `model:2`, `model:3`, … | `/api/v0/models` (loaded or not), then `/v1/models` |
| `ollama` | `http://127.0.0.1:11434/v1` | Every worker uses the same model id. Set `OLLAMA_NUM_PARALLEL` to the number of workers so they run at the same time. | `/api/tags` (downloaded models), then `/v1/models` |
| `openai` | `https://api.openai.com/v1` | Every worker uses the same model id. `-qty` caps how many sessions run at once. Works with any OpenAI-compatible API, public or private. | `/v1/models` |
| `none` | – | No pool. roc doesn't configure a model; the agent uses its own providers and logins (stored in `~/.local/roc/home/<agent>`), or whatever you put in [`agents/`](#agent-settings-and-the-never-ask-default). | – |

roc never limits which providers an agent may use. The model roc configures is only a
default: opencode keeps every other provider it knows, and your settings in `agents/` can
point any agent at a public model. Pass provider API keys into the container with
`-env OPENAI_API_KEY` (or list them in `config.agent.env_passthrough`).

Claude Code talks the Anthropic Messages API. LM Studio and Ollama both provide one at the
server root, and roc points `ANTHROPIC_BASE_URL` there. For a generic `openai` provider, use
`-provider none` with Claude Code and its own login instead.

## The worker pool and `roc -list`

roc treats each model slot as a **worker** and gives each roc session exactly one of them.
With LM Studio, each copy of the model is one worker with its own context window.

| Status | Meaning |
|---|---|
| `running` | A live roc session holds the lease. |
| `available` | Servable by the server and free. |
| `offline` | Not loaded or not downloaded, `enabled: false`, or the server is unreachable. |

Leases are taken under a file lock, so two `roc` processes can never get the same worker.
If a roc process dies, its lease is freed as soon as its PID is gone. Labels come from the
model's first letter: `qwen…` gives `Q #1`, `Q #2`, …, and `llama…` gives `L #1`, ….
Change `label_template` / `name_template` in the state file to rename them.

Sizing: one 27B model at ~16 GB of weights × 4 copies is 64 GB, before KV cache. A
256K-token context adds a lot of KV cache per copy. To keep all four copies inside 256 GB of
unified memory, enable KV-cache quantization in LM Studio or shrink the context, and adjust
`limit.context` in the state file to match.

## Running several sessions at once

Open one terminal per session and run `roc` in each. With four workers you get four
independent sessions:

- each has its own worker, container (`roc-<session id>`), Docker network, MCP gateway
  (random port, its own token), generated config and log;
- containers the agent starts are named `roc-<session id>-<name>` and labelled with the
  session id, and the Docker tools refuse to touch any other session's containers;
- a fifth `roc` reports `no worker is available`, or waits for one with `-wait 600`;
- every session cleans up only its own resources when it ends.

Two configs (`-state ~/a/state.json`, `-state ~/b/state.json`) can run side by side too.
Each config only cleans up its own resources.

## Seeing and stopping sessions: `-ps` and `-kill`

A session's **position** is its number in `roc -list`: the session on `Q #2` is position 2.

```
$ roc -ps
POS  WORKER  AGENT     SESSION       UP     STATE    WORKDIR
1    Q #1    opencode  3f9a1c2b7d4e  42m    running  /Users/andrei/friends_of/planning
2    Q #2    codex     9b0e44d1a7c3  3h05m  running  /Users/andrei/work/api

$ roc -kill 2
stopping Q #2 (codex, session 9b0e44d1a7c3) …
stopped Q #2 (codex, session 9b0e44d1a7c3); Q #2 is free
```

`-kill` takes a position (`2`), a label (`Q#2` or `"Q #2"`), a session id or a prefix of it,
`all`, or a comma-separated mix (`-kill 1,3`). It stops the session's agent container; the
`roc` that owns the session sees its agent exit and removes everything it created, exactly as
on a normal exit, and the worker is free again. If the owner is already gone or doesn't finish
within 30 seconds, `-kill` removes the session's containers, networks, images and lease itself;
`-force` does that straight away. roc never signals a process ID, because process IDs are reused.

With provider `none` there is no pool, so sessions are numbered in the order they started
(`agent #1`, `agent #2`, …). Sessions started on another machine show `on <host>` and can only be
stopped there.

## Mounts

- **1:1 paths.** The container path is the path you typed, after `~` expansion and `.`/`..`
  normalization. If that path is a symlink, the bind source is the resolved real directory.
  This matters for host MCP servers: a path the agent hands to XcodeBuildMCP or Browser MCP
  is valid on your Mac as-is.
- **Checked before Docker runs.** Every entry must exist and be a directory. roc collects
  every problem it finds and reports them all at once.
- **Never mountable:** `/`, your home directory as a whole, system directories (`/etc`, `/usr`,
  `/System`, `/Library`, …), and the protected paths below. A protected path also blocks any
  parent directory that contains it (for example `/Users`):
  `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker`, `~/.config/gcloud`,
  `~/.config/gh`, `~/.password-store`, keychains, the Docker socket, and roc's own state
  directory. Add more under `config.mounts.denied`.
- **Nesting works.** A read-only parent with a read-write child (or the other way round) is fine.
  Mounts are applied parent-first. Listing the same path as both read and write is an error.
- Paths containing `,` cannot be used, because the flag value is CSV.

Besides your directories, the container gets three more mounts:
`~/.local/roc/home/<agent>` as `$HOME`, so agent history, auth and caches persist per agent;
`~/.local/roc/sessions/<id>` at `/roc/session`, holding the generated config (read-only, except for goose, which
writes next to its config file);
and your `~/.gitconfig` (read-only), so commits carry your name.

## Agents

| `-binary` | What roc generates (before your [settings](#agent-settings-and-the-never-ask-default) are merged on top) |
|---|---|
| `opencode` | `opencode.json` (via `OPENCODE_CONFIG`): the worker as provider `roc-<provider>` and default model, every permission `allow` (`question` denied), the session instructions, the MCP servers. |
| `claudecode` | `--settings` with `permissions.defaultMode = bypassPermissions`, `--dangerously-skip-permissions`, `--append-system-prompt` with the session instructions, `--mcp-config`. With a model provider: `ANTHROPIC_BASE_URL` (the server root), `ANTHROPIC_AUTH_TOKEN` and every model alias. It also pre-accepts onboarding and the trust prompt for the working directory. |
| `codex` | `-c` overrides: a `model_providers.roc-<provider>` entry (`wire_api` = `config.agent.codex_wire_api`, default `responses`), the model and context limits, `approval_policy="never"`, `sandbox_mode="danger-full-access"`, `developer_instructions`, and `mcp_servers.*`. It also trusts the working directory in `$CODEX_HOME/config.toml`. |
| `goose` | A session `config.yaml` with `GOOSE_MODE=auto`, the `developer` extension and the MCP servers. The `openai` provider is set via `OPENAI_HOST` / `OPENAI_BASE_PATH`, and the instructions go in as `.goosehints`. |

Loopback hosts in `-ai-host` (`127.0.0.1`, `localhost`) are rewritten to
`host.docker.internal` inside the container. Other hosts are left unchanged. To override
this, set `config.ai.container_host`.

The API token is never placed on the `docker run` command line, where `ps` could show it.
roc passes only the variable name (`--env=ROC_AI_API_TOKEN`), and Docker reads the value from roc's
environment.

## Agent settings and the never-ask-default

**Inside the container the default is "never ask".** Every agent starts with approvals off
(opencode `permission: allow`, Claude Code `bypassPermissions`, Codex `approval_policy =
"never"`, goose `GOOSE_MODE=auto`). Each agent is also given the same rules: the answer is
always yes, and keep working until every requirement in `AGENTS.md` (in the working directory
or above) is met and verified. Without an `AGENTS.md`, it finishes the request including verification.

Each roc config (each state file) has an `agents/` directory next to it, created by `roc -init`:

```
~/.local/roc/agents/
  instructions.md   the rules above; edit to taste (given to every agent)
  opencode.json     merged over the generated opencode.json
  claude.json       merged over Claude Code's settings file
  codex.toml        merged into $CODEX_HOME/config.toml; its keys replace roc's -c values
  goose.json        merged over goose's config.yaml
```

**Whatever you put in these files wins.** Some examples:

```jsonc
// agents/opencode.json — use a public model instead of the local worker
{ "model": "anthropic/claude-sonnet-4-5" }

// agents/opencode.json — ask before shell commands after all
{ "permission": { "bash": "ask" } }
```

```toml
# agents/codex.toml — different model provider for Codex only
model = "gpt-5"
model_provider = "openai"
```

`roc -agent-config -binary codex` prints the file and where it applies.
`roc -dry-run -binary codex` prints the final merged configuration. On every session start,
roc writes the merged result to `~/.local/roc/sessions/<id>/`, mounts it at `/roc/session`,
and deletes it when the session ends.

## MCP servers

Adding a tool for the agent is one command. roc ships a catalog of vetted MCP servers, groups
them into stacks, writes them into the state file, and at session start wires each one into
whichever agent you run, with the right transport and with secrets kept on your machine:

```sh
roc -stack frontend              # context7, shadcn, figma, browsermcp, chrome-devtools
roc -mcp add github              # one server from the catalog
roc -mcp                         # what is configured and whether each one will start
roc -mcp test                    # start them now and list their tools
roc -with-mcp memory             # just for this session
```

### Stacks

| stack | servers | for |
|---|---|---|
| `frontend` | context7, shadcn, figma, browsermcp, chrome-devtools | React/Tailwind web apps |
| `nextjs` | the frontend stack + nextjs | Next.js apps |
| `wails` | wails, wails-app, gopls, context7, shadcn, chrome-devtools | Wails v3 desktop apps (Go + React/Tailwind) |
| `ios` | xcodebuildmcp, xcode, context7 | iOS and macOS apps |
| `data` | database, memory | a SQL database and persistent memory |
| `research` | context7, brave-search, sequential-thinking, memory | docs, web search, planning |
| `github` | github | issues, pull requests, Actions |

### The catalog

`roc -mcp catalog` prints this list with what each server needs. `roc -mcp info NAME` shows the
full definition, its secrets (set or not) and its documentation.

| name | type | what for | needs |
|---|---|---|---|
| `docker` | builtin | Containers, images and networks **scoped to this session** ([below](#the-docker-mcp-server)). On by default. | – |
| `browsermcp` | host | Your real Chrome tab, with your logins ([Browser MCP](https://browsermcp.io) extension). On by default. | Node.js on the host |
| `chrome-devtools` | host | Chrome DevTools: performance traces, network, console with source maps, screenshots. | Node.js, Chrome |
| `playwright` | container | Headless Chromium inside the agent's container. | image built with `-with-playwright` |
| `playwright-host` | host | A visible Playwright browser on your machine, so you can watch. | Node.js |
| `context7` | remote | Current library docs (React, Next.js, Tailwind, Wails, …) instead of stale training data. | `CONTEXT7_API_KEY` optional |
| `shadcn` | container | The official shadcn/ui server: search registries, exact props, install commands. | – |
| `figma` | container | Framelink: Figma layout, styles and assets as data. | `FIGMA_API_KEY` |
| `figma-desktop` | proxy | Figma's own desktop server (Dev Mode → *Enable desktop MCP server*). | Figma desktop |
| `nextjs` | container | Next.js DevTools: docs, upgrades, live errors and routes from `next dev`. | – |
| `wails` | host | Wails v3 project server (`wails3 mcp`): inspect, doctor, build, dev mode, bindings, Taskfile tasks. | `wails3` on the host |
| `wails-app` | proxy | The running Wails app: windows, DOM, JS, mouse/keyboard, bound Go methods, events. | app started with `WAILS_MCP=1` |
| `gopls` | container | Go language server as MCP (gopls ≥ 0.20). | the agent image (included) |
| `xcodebuildmcp` | host | Build, test and run Xcode projects; iOS simulators, UI automation. On by default on macOS. | Xcode, Node.js |
| `xcode` | host | Xcode 26.3+'s built-in MCP via `xcrun mcpbridge`. | Xcode |
| `database` | container | DBHub: schemas and SQL for Postgres, MySQL, SQLite, SQL Server. | `DATABASE_URL` |
| `memory` | container | A knowledge graph that persists across this agent's sessions. | – |
| `sequential-thinking` | container | Structured step-by-step planning with revisions. | – |
| `brave-search` | container | Web search. | `BRAVE_API_KEY` |
| `github` | remote | GitHub issues, pull requests, code search, Actions. | `GITHUB_PERSONAL_ACCESS_TOKEN` |

A server whose required secret is unset, whose host command is missing, or that doesn't apply
to your OS is skipped for the session with a one-line note. `roc -mcp` and `roc -doctor` show
those reasons ahead of time, each with the command or variable that fixes it.

### How servers reach the agent

| type | runs where | reached how |
|---|---|---|
| `builtin` | inside roc | the gateway, `http://host.docker.internal:<port>/mcp/<name>` |
| `host` | a stdio process on **your machine**, started by roc on first use | the gateway; roc bridges HTTP ⇄ stdio |
| `proxy` | an HTTP server on your machine's loopback (a Wails app, Figma desktop) | the gateway forwards to it |
| `remote` | a hosted streamable-HTTP server | directly; **through the gateway when a header uses a secret** |
| `container` | a stdio process inside the agent container | the agent spawns it |

The gateway listens on a random port. Every request must carry a per-session bearer token,
and requests with an `Origin` header (that is, from browsers) are rejected. It binds to
`127.0.0.1` on macOS and to the docker0 bridge address on Linux. It exists only while the
session runs.

Picking a type: use `host` when the server must touch your machine (your browser, Xcode, a native
Wails build), `container` when it works on project files or talks to services on the session
network (a database you started with `docker_run`), `proxy` for HTTP servers that listen on your
machine's 127.0.0.1, and `remote` for hosted servers.

### Secrets stay on your machine

Write `${VAR}` wherever a server needs a secret; roc never stores the value.

- **Remote headers** (`"Authorization": "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}"`) make the server
  go through the gateway, which adds the header on your machine. The token never enters the
  container or a generated config file. A header whose variable is unset is left out, so optional
  keys (Context7) just work without one.
- **Container env** written as exactly `${NAME}` is forwarded by name: Docker reads the value from
  roc's environment, and each agent's config refers to it (`{env:NAME}` for opencode, `${NAME}`
  for Claude Code, `env_vars` for Codex, `env_keys` for goose).
- **Host env and args** are expanded when roc starts the server. `${ROC_WORKDIR}` (the session's
  working directory) and `${ROC_SESSION_ID}` are available too.
- `requires_env` lists the variables a server can't work without; it is skipped (with a note)
  when one is unset.

### Adding your own

```sh
roc -mcp add api=https://mcp.example.com/mcp                  # remote
roc -mcp add app=http://127.0.0.1:4000/mcp                    # loopback URLs become proxy servers
roc -mcp add "sqlite=uvx mcp-server-sqlite --db-path /Users/andrei/work/app/dev.db"   # container (default)
roc -mcp add "figma2=host:npx -y figma-developer-mcp --stdio"  # on your machine
roc -mcp set api 'header.Authorization=Bearer ${API_TOKEN}' requires=API_TOKEN \
                 hint='Use it for the billing API; never call DELETE endpoints.'
roc -mcp disable api ; roc -mcp enable api ; roc -mcp rm api
```

`set` keys: `type`, `command`, `args`, `url`, `description`, `hint` (added to the agent's
instructions), `enabled`, `requires`, `platforms`, `env.NAME`, `header.NAME` (an empty value
removes it). Quote values containing `${…}` in single quotes so your shell leaves them alone.
The state file can still be edited by hand; `roc -mcp info NAME` prints an entry as JSON.

### The agent knows what it has

Every session's instructions end with a generated section listing the servers that actually
started, what each is for and how to use it (for example: "browser tools on the host reach your
dev server only through published ports"). It also lists the ports published with `-publish`,
or says that none are, so the agent doesn't chase a dev server your browser can't reach.

### Protocol versions

The gateway speaks MCP 2024-11-05 through 2025-11-25 and the stateless
[2026-07-28](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/) revision:
clients may skip `initialize`, ask `server/discover`, and send `Mcp-Method` routing headers (a
header that disagrees with the body is rejected). roc initialises older stdio and HTTP servers on
such a client's behalf, keeps their `Mcp-Session-Id`, and renews an expired session once.
Remote servers honour `HTTPS_PROXY`/`NO_PROXY` (never for local or private hosts), and TLS uses
your system's trust store.

### The Docker MCP server

The agent sees these tools: `docker_run`, `docker_build`, `docker_pull`, `docker_ps`,
`docker_images`, `docker_logs`, `docker_exec`, `docker_start`, `docker_stop`,
`docker_restart`, `docker_rm`, `docker_inspect`, `docker_rmi`, `docker_network_create`,
`docker_network_rm` and `roc_session_info`.

Policy (configurable under `config.docker`):

- Every resource gets the labels `roc.managed=true` and `roc.session=<id>`, and is recorded in the
  state file **before** the tool returns.
- stop, rm, exec, logs and inspect check the `roc.session` label and refuse any container from
  another session or from outside roc. They also refuse the agent's own container. Removing an
  image works only if this session built it, or pulled it when it wasn't already present.
- A bind-mount `source` is resolved through symlinks and must land inside one of the session's
  mounts. Sources inside read-only mounts are forced to read-only. The default target is the same path (1:1).
- There is no privileged mode, `--cap-add`, devices, host network or PID namespace, and no
  arbitrary flags. Tool arguments are typed, and values are passed as `--flag=value` so they cannot be read as extra flags.
- Published ports bind to `127.0.0.1`, so your browser (and Browser MCP) can open
  `http://localhost:8080`.
- Built tags are forced under `roc-local/`. A session may have at most 20 containers and 20 images.
- Containers join the session network `roc-<id>`, which the agent container is also on, so
  the agent can `curl http://roc-<session id>-api:8080` by container name.
- When the session ends, everything goes: containers, networks, and images (unless `-keep-images`).

### Workflows

**A React/Tailwind frontend** (`roc -stack frontend`, then `roc -publish 5173`):

1. The agent checks APIs it isn't sure of with `context7` and gets exact component props from `shadcn`.
2. Given a Figma frame link, it reads the design with `figma` and downloads the assets.
3. It runs `npm run dev -- --host 0.0.0.0` in its container; the dev server is at
   `http://localhost:5173` on your machine.
4. It checks the page in your real browser with `browsermcp`, and console errors, failed requests
   and performance with `chrome-devtools`.
5. It starts a database with `docker_run {"image":"postgres:17","name":"db","env":{…}}` and
   connects to `roc-<session id>-db:5432`.

**A Wails v3 desktop app** (`roc -stack wails`; needs `wails3` on your machine):

1. The agent edits Go services and the React/Tailwind frontend in the container, using `gopls` for
   Go references and diagnostics and `context7`/`shadcn` for the UI.
2. Through `wails` (the `wails3 mcp` server, running on your machine and scoped to the working
   directory) it regenerates TypeScript bindings, runs `wails3 doctor`, and builds or starts
   dev mode natively, so you get a real `.app`/`.exe`, not a Linux binary.
3. While you run `WAILS_MCP=1 wails3 dev` on your machine, `wails-app` lets it inspect the DOM,
   evaluate JavaScript, click and type, and call bound Go methods in the real window. (Keep
   `WAILS_MCP` out of your shell profile; builds inherit it and production builds must not include
   the in-app server.)
4. `go test` and `npm test` run in the container as usual.

**An iOS app** (`roc -stack ios`): XcodeBuildMCP builds and tests on your Mac, boots simulators
and drives the UI; project paths are identical on both sides, so the agent passes them as it sees them.

## The state file

`~/.local/roc/state.json` (mode 0600, directory 0700) is the single source of truth:

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/playandprosper/roc/main/schema/state.schema.json",
  "version": 1,                        // schema version; roc refuses newer files
  "updated_at": "2026-10-07T14:00:00Z",
  "config": {                          // durable; safe to hand-edit while no session runs
    "ai": {
      "provider": "lmstudio",          // lmstudio | ollama | openai | none
      "provider_id": "lmstudio",
      "provider_name": "LM Studio",
      "host": "http://127.0.0.1:1234/v1",
      "container_host": "",
      "api_token_env": "ROC_AI_API_TOKEN",  // tokens are never stored
      "model": "qwen3.8-27b",
      "qty": 4,
      "name_template": "{initial} #{n} Agent",
      "label_template": "{initial} #{n}",
      "limit": { "context": 256256, "output": 32768 },
      "models": {                       // the worker pool, keyed by worker key
        "qwen3.8-27b":   { "name": "Q #1 Agent", "label": "Q #1", "worker": 1, "limit": { "context": 256256, "output": 32768 }, "enabled": true },
        "qwen3.8-27b:2": { "name": "Q #2 Agent", "label": "Q #2", "worker": 2, "limit": { "context": 256256, "output": 32768 }, "enabled": true },
        "qwen3.8-27b:3": { "name": "Q #3 Agent", "label": "Q #3", "worker": 3, "limit": { "context": 256256, "output": 32768 }, "enabled": true },
        "qwen3.8-27b:4": { "name": "Q #4 Agent", "label": "Q #4", "worker": 4, "limit": { "context": 256256, "output": 32768 }, "enabled": true }
      }
    },
    "agent":  { "binary": "opencode", "image": "roc-agent:latest", "publish": [], "env_passthrough": [], "mount_gitconfig": true,
                "codex_wire_api": "responses", "extra_docker_args": [], "memory": "", "cpus": "" },
    "mounts": { "read": ["~/work", "~/statuses"], "write": ["~/friends_of/planning", "~/friends_of/knowledge"], "denied": [] },
    "mcp":    { "bind": "auto", "port": 0, "request_timeout_secs": 600, "servers": { "…": "…" } },
    "docker": { "max_containers": 20, "max_images": 20, "image_tag_prefix": "roc-local/", "allow_pull": true,
                "allow_publish": true, "publish_bind": "127.0.0.1", "remove_images_on_exit": true, "command_timeout_secs": 1800 }
  },
  "sessions": {                        // runtime; written by roc only
    "3f9a1c2b7d4e": {
      "id": "3f9a1c2b7d4e", "pid": 48211, "hostname": "andrei-mbp", "status": "running",
      "binary": "opencode", "model": "qwen3.8-27b:2", "key": "qwen3.8-27b:2", "worker": 2,
      "container": "roc-3f9a1c2b7d4e", "network": "roc-3f9a1c2b7d4e", "gateway": "127.0.0.1:53817",
      "mounts": [ … ], "workdir": "/Users/andrei/friends_of/planning",
      "resources": { "containers": [ … ], "images": [ … ], "networks": [ … ] }
    }
  }
}
```

The complete example is in [`examples/state.example.json`](examples/state.example.json) and the
JSON Schema in [`schema/state.schema.json`](schema/state.schema.json). If your editor supports
JSON Schema, it will autocomplete and validate the file.

Why this file is safe to rely on:

- Every read-modify-write runs under an exclusive `flock` on `state.json.lock`.
- Writes are atomic: temp file, `fsync`, `rename`, directory `fsync`. A crash leaves either the old file or the new one, never a half-written mix.
- Its meaning is checked on every load and save: unique worker numbers, a valid host URL, valid MCP server names.
  An invalid edit is rejected; it never silently resets your config.
- Unknown top-level keys are preserved, and a file from a newer roc is refused rather than downgraded.
- Changing `-ai-model` or `-qty` regenerates `models` but keeps the names and limits you customized for ids that still exist.
- `config.agent.binary`, `image`, `publish`, `mounts.read`/`write` act as defaults. To change them, use `-save` or edit the file.

## Security model

- **The agent's view of your files** is exactly the mounted directories. Read-only mounts are
  enforced by the kernel, not by agent settings.
- **Container hardening:** your uid:gid (not root), `--cap-drop=ALL`,
  `--security-opt=no-new-privileges`, `--init`, and its own network. No Docker socket, no SSH agent, no
  cloud credentials. Host environment variables pass through only when you ask with `-env` or `env_passthrough`.
- **Host MCP servers run as you on your Mac.** That is their purpose: driving your browser and
  simulators. Enable only servers you trust. Their stderr goes to log files, never to your terminal.
- **MCP secrets stay on the host.** Tokens in remote server headers are added by the gateway;
  container servers get secrets forwarded by name, never written to a config file.
- **The gateway** listens on loopback (macOS) and requires a 192-bit random token that changes every session.
  Requests with an `Origin` header are refused, so a web page cannot use DNS rebinding to reach it.
- **The network is not isolated.** The agent can reach your model server, your LAN and the internet
  (package registries). If you need an air-gapped session, use a Docker network policy or
  firewall. roc's job is file and process isolation.
- **Things roc can't protect against:** secrets you mount yourself (for example a `.env` inside a write dir), and
  what the agent does with the tools you enable.

## Cleanup, recovery and logs

- **Normal exit:** quit the agent and roc removes everything, then exits with the agent's code.
- **Closing the terminal or `kill <roc pid>`:** on SIGTERM, SIGHUP or SIGQUIT, roc stops the agent
  container gracefully (10 s), then cleans up. Ctrl-C belongs to the agent, because the terminal
  is in raw mode.
- **`kill -9` or a power loss:** the next `roc` start sees the dead PID and cleans that session
  automatically. `roc -cleanup` does the same on demand and also sweeps any `roc.managed`
  containers and networks whose session no longer exists.
- **Logs:** `~/.local/roc/logs/<session>.log` records each docker command, MCP tool call and cleanup
  step. Logs for host MCP servers are in `~/.local/roc/logs/<session>-mcp-<name>.log`. roc keeps the most recent 200 log files.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `image roc-agent:latest not found` | `roc -build-image` or `make image` |
| `… is unreachable` | Check that the server is running, that it listens on the network for non-local hosts (LM Studio: *Serve on Local Network*; Ollama: `OLLAMA_HOST=0.0.0.0`), and that the URL matches `-ai-host`. Try `curl $HOST/models`. |
| A worker shows `offline` but is loaded | The id must match exactly. Compare `roc -list -json` with `curl $HOST/models`. Fix the model with `roc -init`. |
| `no worker is available` | All workers are running or offline. Use `-wait 600`, or `roc -cleanup` if a crashed session still holds a lease. |
| The agent can't reach a server at `127.0.0.1` | roc rewrites loopback to `host.docker.internal`. On Colima or Rancher, set `config.ai.container_host` to an address the VM can reach. |
| MCP tools missing on Colima/Rancher/rootless Docker | Set `config.mcp.bind` to an address reachable from containers. The bearer token still protects it. |
| `browsermcp` errors | Install the Browser MCP Chrome extension and click *Connect*. Only one session can own it at a time (port 9009). |
| `npx … not found; skipped` | Install Node.js on the host (needed for host MCP servers). |
| Claude Code refuses `--dangerously-skip-permissions` | It won't run as root. Don't run roc as root. |
| Codex can't talk to the model | Set `config.agent.codex_wire_api` to `chat` (older LM Studio and Ollama builds have no `/v1/responses`). |
| The agent still asks for approval | Check `agents/<agent>` for a stricter setting; `roc -dry-run` shows the merged config. |
| `MCP server X skipped: …` at start | Run `roc -doctor`: it names the missing secret or host tool and how to get it. |
| An MCP server misbehaves | `roc -mcp test NAME` starts it outside a session and lists its tools; host servers log to `~/.local/roc/logs/<session>-mcp-<name>.log`. |
| A session is stuck or you need its worker | `roc -ps`, then `roc -kill N` (add `-force` if it doesn't stop). |
| Anything else | `roc -doctor`, then `roc -dry-run …`: it shows exactly what would run, including the MCP wiring and the agent's instructions. |

## Migrating from a host opencode install

Your old `~/.config/opencode/opencode.json` maps onto roc like this:

| Old setting | roc equivalent |
|---|---|
| `read allow ~/friends_of/**, ~/work/**, ~/go/**` | `-read-dir "~/friends_of,~/work,~/go"` |
| `edit allow ~/friends_of/planning/**`, … | `-write-dir ~/friends_of/planning,…` (read-only parents plus read-write children work) |
| `edit allow *.go, *.php, …` | not needed: anything inside a write dir may be edited |
| `shell allow git/go/cargo/npm/ls/…` | not needed: the shell is the container's |
| `provider.lmstudio-studio.models` | `config.ai.models`, managed by `-ai-model`/`-qty` |
| `mcp.browsermcp` | `config.mcp.servers.browsermcp` (runs on the host, bridged) |
| `agent.build.temperature/top_p` and anything else | `~/.local/roc/agents/opencode.json` (roc 0.1.0's `config.agent.opencode_overrides` is moved there automatically) |

Then remove opencode from the host. See [INSTALL.md](INSTALL.md#6-remove-host-installed-agents).

## Development

```sh
make help      # list targets
make test      # unit + integration tests (no Docker or model server needed)
make lint      # rustfmt --check + clippy -D warnings
make image     # build the agent image
make dist      # tarball of the release binary for this machine
make build-deps build-all   # all six release binaries into dist/
```

`make build-all` produces `roc-darwin-arm64`, `roc-darwin-amd64`, `roc-linux-amd64`,
`roc-linux-arm64` (static musl; `LINUX_LIBC=gnu` for glibc), `roc-amd64.exe` and
`roc-arm64.exe`, plus `SHA256SUMS`. Your own platform is built with cargo and the others with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild); `make build-deps` installs the Rust
targets, zig and cargo-zigbuild. Each platform also has its own target:
`make build-windows-amd64`, `build-windows-arm64`, `build-linux-amd64`, `build-linux-arm64`,
`build-darwin-amd64` and `build-darwin-arm64`. Releases (tags `v*`) run the same command on CI.
On Windows, host paths appear in the container as `/c/Users/...` (see
[INSTALL.md](INSTALL.md#windows)).

The integration tests in `tests/cli.rs` replace Docker with a shell script (`ROC_DOCKER`) and
the model server with an in-process HTTP server on 127.0.0.1. They run a full session: lease, gateway, a fake "agent"
calling the Docker MCP server over HTTP, then cleanup. Another test starts four sessions at once,
checks they get four workers and four containers, and checks that a fifth is refused.

Source map: `cli.rs` (flags), `init.rs` (guided setup), `session.rs` (lifecycle), `state.rs`
(state file), `pool.rs` (leases), `provider.rs` (LM Studio / Ollama / OpenAI probes), `paths.rs`
(mount validation), `agents.rs` (per-agent config), `agent_files.rs` (`agents/` settings),
`docker.rs` (docker CLI and cleanup), `control.rs` (`-ps`, `-kill`), `doctor.rs` (`-doctor`),
`mcpctl.rs` (`-mcp`, `-stack`), `mcp/` (gateway, stdio bridge, HTTP proxy, Docker tools,
`catalog.rs` for the built-in servers and stacks, `plan.rs` for how each server reaches the agent).

Please report security issues privately via GitHub security advisories on
`playandprosper/roc`.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

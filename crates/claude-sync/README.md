# claude-sync

Keep your whole team's **Claude Code setup** in sync, live, while everyone keeps **their own Claude subscription** and their own personal settings and secrets.

One person publishes the team setup: shared instructions, slash commands, skills, subagents, hooks, MCP servers and permission rules. Everyone else's machine stays connected to a small hub and picks up each change within moments. Nobody's personal configuration is overwritten.

```
  team folder (git repo)                          ┌── bob's laptop   ~/.claude  ✓ rev 7
        │  claude-sync publish                    │
        ▼                                         ├── carol's laptop ~/.claude  ✓ rev 7
   ┌─────────┐    WebSocket push (live)           │
   │   hub   │ ───────────────────────────────────┤
   └─────────┘   only changed files are sent      └── ci runner      ~/.claude  ✓ rev 7
```

## What gets synced, and how

| Team file | Goes to | How it merges |
|---|---|---|
| `CLAUDE.md` | `~/.claude/CLAUDE.md` | Placed in a marked team block. Everything outside the block is yours and never changes. |
| `settings.json` | `~/.claude/settings.json` | Permission rules and hooks are **added** to yours. `env` and other keys are added only where you have nothing. Your values always win. |
| `mcp.json` (`{"mcpServers": {...}}`) | `~/.claude.json` → `mcpServers` | Team servers are added. A server you already have under the same name is left alone. No other part of `~/.claude.json` (login, project history) is touched. |
| `commands/**` `agents/**` `skills/**` | `~/.claude/...` | Copied in. A file you edited, or one you already had, is never overwritten. |

When the team **changes or removes** something, claude-sync updates or removes exactly what it added earlier, as long as you haven't edited it since. To make that possible, it records everything it writes in `~/.claude-sync/state.json`. If you edited something, your version stays and is listed as `kept yours`.

### Personal secrets stay local

Team files never contain credentials. They use placeholders:

```json
{"mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                           "env": {"GITHUB_TOKEN": "{{secret:GITHUB_TOKEN}}"}}}}
```

Each person sets their own value with `claude-sync secrets set GITHUB_TOKEN`. It's stored in `~/.claude-sync/secrets.json` (mode 0600) and never uploaded. Until you set it, that MCP server is skipped and you get a warning saying which secret is missing.

Publishing is **refused** if a team file looks like it contains a real credential, such as a GitHub, Slack, AWS, Anthropic, Stripe or GitLab token, or a private key. Both the client and the hub check.

## Quick start

**1. Run a hub** somewhere the team can reach. Put it behind HTTPS for anything beyond a LAN.

```bash
claude-sync gen-token   # one per person
```

```toml
# claude-sync-hub.toml
listen = "0.0.0.0:8790"
data_dir = "claude-sync-hub"

[[members]]
name = "alice"
token = "env:TOKEN_ALICE"
role = "publisher"   # can publish and roll back

[[members]]
name = "bob"
token = "env:TOKEN_BOB"   # role defaults to "member" (receive only)
```

```bash
claude-sync hub -c claude-sync-hub.toml
```

**2. Create the team setup** (publisher), starting from your own Claude Code config:

```bash
claude-sync login --hub https://sync.example.com --token cst-...
claude-sync init-team ./team-claude   # copies your setup; credentials become {{secret:...}} placeholders
cd team-claude && git init            # optional: keep it in git for history and review
claude-sync publish . -m "initial team setup"
```

`init-team` exports your `CLAUDE.md`, commands, agents and skills, and the permissions, hooks and env from your settings. It skips personal preferences such as `model`. It also exports your MCP servers, with every server env value and auth header replaced by a placeholder. Your real values go into your own secret store, so your setup keeps working.

**3. Everyone else connects once:**

```bash
claude-sync login --hub https://sync.example.com --token cst-...
claude-sync diff       # preview exactly what will change
claude-sync connect    # stay connected; changes apply as soon as they're published
```

Run `claude-sync connect` at login, for example as a systemd user service or a launchd agent. It reconnects automatically with backoff.

## Commands

| Command | What it does |
|---|---|
| `connect` | Stay connected and apply every new team revision immediately |
| `pull [--dry-run]` | Apply the latest team setup once |
| `diff` | Show unified diffs of what a pull would change |
| `status` | Your revision versus the team's, plus everyone connected right now and whether they're in sync |
| `publish DIR [-m MSG] [--watch]` | Publish a team folder. Only files the hub doesn't have are uploaded. With `--watch`, re-publishes whenever the folder changes. |
| `history` | Revision list: number, time, author, message |
| `rollback REV` | Re-publish an earlier revision as a new one |
| `init-team DIR` | Create a team folder from your own setup |
| `secrets set NAME [VALUE]` / `list` / `rm` | Your personal secret values (if VALUE is omitted, it's read from stdin) |
| `login --hub URL --token T` | Save the hub address and your token in `~/.claude-sync/config.toml` (0600) |
| `hub -c FILE` | Run the hub |
| `gen-token` | Generate a member token |

`CLAUDE_CONFIG_DIR` (Claude Code's own override) and `CLAUDE_SYNC_HOME` are respected.

## Team folder layout

```
team-claude/
├── CLAUDE.md
├── settings.json        {"permissions": {...}, "hooks": {...}, "env": {...}}
├── mcp.json             {"mcpServers": {...}}
├── commands/review.md
├── agents/tester.md
├── skills/pdf/SKILL.md  (plus any scripts or resources)
└── README.md            (not synced: document the team setup here)
```

Dotfiles and folders (such as `.git`) are ignored. Anything outside this layout is rejected, and paths can't escape `~/.claude`.

## How the hub works

- Files are stored **content-addressed** (SHA-256). A publish uploads only new content, and clients cache what they've already downloaded, so an update that touches one file transfers only that file.
- Every publish is an immutable **revision** with an author and a message. `rollback` re-publishes an old one.
- Publishing uses optimistic concurrency. If someone else published first, you get a clear "pull and retry" instead of silently overwriting their change.
- Connected clients get a WebSocket push on every publish. In the tests, a teammate's machine applied a change about 6 ms after it was published (loopback).
- The hub tracks who is connected, which revision they have, and how many items they kept locally (`claude-sync status`).

## Scope

claude-sync only moves **configuration**. Each person signs in to Claude Code with their own account and subscription as usual. It never reads, copies or shares login credentials or OAuth tokens. In `~/.claude.json` it reads and writes only the `mcpServers` key.

## Development

```bash
cargo test -p claude-sync
```

`tests/sync.rs` runs a real hub on loopback with two teammates, each with a separate fake home directory. It covers:
- publish, pull and merge, including later team updates and removals
- local edits winning over team changes
- secrets, both missing and set
- dry runs and diffs
- live push to a connected agent, and presence reporting
- roles, auth, credential blocking and path traversal
- deduplicated uploads, stale-publish rejection, rollback, and `init-team` secret extraction

# claude-pool

Two Rust tools for teams that work with Claude:

| Tool | For teams that… | What it does |
|---|---|---|
| [**claude-sync**](crates/claude-sync) | use **Claude Code subscriptions** (each person has their own account) | Live-syncs the team's Claude Code setup to everyone: `CLAUDE.md`, slash commands, skills, subagents, hooks, MCP servers and permission rules. Personal settings and secrets are never overwritten. |
| [**claude-pool**](crates/claude-pool) | use **Claude API keys** | A fast gateway that pools several API accounts behind one endpoint, with smart routing, failover, prompt-cache affinity, per-person usage and budgets, and alerts. |

## Install

```bash
git clone <this repo> && cd claude-pool
cargo build --release
# binaries: target/release/claude-sync  target/release/claude-pool
```

This needs Rust 1.85 or newer (edition 2024). There's also a Dockerfile that builds both binaries.

## claude-sync in 30 seconds

```bash
# publisher
claude-sync init-team ./team-claude        # start from your own setup; secrets become placeholders
claude-sync publish ./team-claude -m "team setup"

# everyone else
claude-sync login --hub https://sync.example.com --token cst-...
claude-sync connect                        # stays connected; updates apply the moment they're published
```

See [crates/claude-sync/README.md](crates/claude-sync/README.md) for the full guide.

## claude-pool in 30 seconds

```bash
cp crates/claude-pool/claude-pool.example.toml claude-pool.toml   # add your API keys and team tokens
claude-pool serve
export ANTHROPIC_BASE_URL=http://pool-host:8787 ANTHROPIC_API_KEY=cpk-...   # on each machine
```

Claude Code and the Anthropic SDKs work through it unchanged. See [crates/claude-pool/README.md](crates/claude-pool/README.md).

## Layout

```
crates/
├── claude-sync/   hub server + client agent (axum, WebSocket, content-addressed store)
└── claude-pool/   API gateway (axum, reqwest, lock-free routing and accounting)
```

## Development

```bash
cargo test --workspace                          # 41 tests, including end-to-end suites for both tools
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Terms

claude-sync shares **configuration**, never accounts: everyone signs in with their own subscription. claude-pool pools **API keys from organizations you control**. Neither tool shares Claude Pro or Max logins, which the consumer terms don't allow.

## License

MIT

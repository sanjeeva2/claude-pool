# claude-pool

A fast, single-binary gateway written in Rust that pools several **Claude API accounts** behind one endpoint, so a team can share them efficiently.

Point your tools (Anthropic SDKs, Claude Code, curl, anything that speaks the Claude API) at `claude-pool` instead of `api.anthropic.com`. Each team member gets their own token. The gateway decides which account serves each request, fails over when one is rate-limited, and keeps a record of who used what.

```
 alice ─┐                         ┌─► account "team-main"   (weight 2)
 bob   ─┼─► claude-pool :8787 ────┼─► account "team-backup" (weight 1)
 ci    ─┘   routing · failover    └─► account "research"
            usage · budgets · alerts
```

## Features

- **Smart routing.** Each request goes to the account with the lowest `(in-flight + 1) / (weight × rate-limit headroom)`. Headroom comes from the `anthropic-ratelimit-*` headers on every response, so traffic moves away from an account *before* it starts returning 429s.
- **Prompt-cache affinity.** Requests that share a prompt prefix (tools → system → first message) go to the same account through weighted rendezvous hashing, so the provider-side prompt cache keeps hitting and cached reads stay cheap. If an account drops out, only that account's conversations move. You can pin explicitly with the `x-claude-pool-affinity: <any string>` header.
- **Transparent failover.** On 429, 529, 5xx, 408 or a connection error, the request is retried on another account (up to `max_attempts`). The failed account cools down for the duration given by `retry-after`, or with exponential backoff. A 401 (revoked key) parks the account for an hour.
- **Zero-copy streaming.** SSE streams pass through byte for byte. Token usage is read from the stream as it flows: only the current partial line is buffered, and only `data:` lines containing `"usage"` (found with SIMD `memmem`) get parsed.
- **Usage and cost tracking** per account, per team member and per model: requests, input/output/cache tokens and estimated USD. Counters are lock-free atomics and are saved to disk atomically, so they survive restarts.
- **Budgets.** Monthly USD budgets per account (traffic stops going to it) and per team member (they get a 429 with a clear message).
- **Alerts** for low rate-limit headroom, cooling-down or disabled accounts, budget thresholds (80%, 100%) and an exhausted pool. Each alert fires once when it starts and once when it resolves. Alerts are always logged and can also go to a Slack-compatible webhook.
- **Org-wide usage report.** `claude-pool usage` pulls the Admin API usage and cost reports from every account that has an admin key, and prints combined totals.
- **Observability.** `/pool/stats` (JSON), `/metrics` (Prometheus) and `/pool/health`.

## Quick start

```bash
cargo build --release
cp claude-pool.example.toml claude-pool.toml   # then edit it
./target/release/claude-pool gen-token          # one per team member
./target/release/claude-pool check              # validate config + secrets
./target/release/claude-pool serve
```

Each team member then uses their pool token in place of an API key:

```bash
export ANTHROPIC_BASE_URL=http://pool-host:8787
export ANTHROPIC_API_KEY=cpk-...your-pool-token...
```

The Anthropic SDKs and Claude Code read both variables. The pool accepts the token as `x-api-key` or as `Authorization: Bearer`. It strips the token, injects the chosen account's real key, and never sends client credentials upstream.

## Commands

| Command | What it does |
|---|---|
| `serve` | Run the gateway |
| `check` | Validate the config and resolve `env:` / `file:` secrets |
| `gen-token` | Generate a random team-member token |
| `stats [--url URL] [--token T]` | Print live stats from a running gateway |
| `usage [--days N] [--json]` | Combined Admin API usage and cost across accounts (needs `admin_key`). Accounts that share an admin key are counted once. |

The config path defaults to `claude-pool.toml`. Override it with `-c` or `CLAUDE_POOL_CONFIG`. Set the log level with `RUST_LOG` (for example `RUST_LOG=claude_pool=debug`).

## Endpoints

| Path | Auth | Description |
|---|---|---|
| `/v1/*` | pool token | Proxied to the upstream Claude API |
| `/pool/health` | none | `200` if at least one account can take traffic, otherwise `503` |
| `/pool/stats` | `admin_token` | JSON: per-account state, headroom, in-flight, usage; per-client and per-model usage |
| `/metrics` | `admin_token` | Prometheus text format |

Every proxied response includes `x-claude-pool-account` (disable with `expose_account_header = false`).

When no account is available, the pool returns `503` with an `overloaded_error` body and a `retry-after` header, so SDKs back off correctly.

## Configuration

See [`claude-pool.example.toml`](claude-pool.example.toml), where every option is documented. Secrets can be literal, `env:VAR` or `file:/path`.

Cost figures are **estimates** based on the built-in list-price table in [`src/pricing.rs`](src/pricing.rs). Override them with `[pricing."model-prefix"]` to match your contract. For authoritative billing, use `claude-pool usage`, which reads Anthropic's own cost report.

## Performance

Run `cargo run --release --example bench -- <requests> <concurrency>`. It compares direct calls to a zero-latency mock upstream against calls through the gateway, with an ~7 KB request body. Results on a 4-vCPU container, with client, gateway and mock sharing the same cores:

| Scenario | Direct | Through gateway |
|---|---|---|
| 1 connection, 1 worker thread | p50 29 µs | p50 76 µs |
| 64 concurrent connections | 56k req/s | 21k req/s, p99 4.9 ms |

That is about 50 µs of added work per request, against Claude responses that take hundreds of milliseconds to minutes.

Other choices that keep it fast:
- Account selection is an O(accounts) scan over atomics, with no locks or allocation.
- Request bodies are parsed with borrowed `RawValue`s, so unneeded fields are skipped without allocating.
- Upstream connections are pooled and kept alive, with HTTP/2 when the upstream supports it.
- TCP_NODELAY is on, mimalloc is the allocator, and release builds use fat LTO.

## Scope and terms

This pools **Claude API keys from Console organizations you control**: your company's orgs, workspaces or keys. It does not share consumer claude.ai Pro or Max logins; those are personal and sharing them breaks the consumer terms. Make sure your use stays within your agreement with Anthropic and its usage policies.

## Development

```bash
cargo test                       # unit + end-to-end tests against a mock Claude API
cargo clippy --all-targets
cargo fmt
```

The end-to-end tests in `tests/gateway.rs` start a mock upstream and the real gateway on loopback. They cover failover (429/500/401), the 503 exhausted-pool response, byte-exact SSE passthrough with usage accounting, auth, affinity, budgets, persistence, and exact accounting under 800 concurrent requests.

## License

MIT

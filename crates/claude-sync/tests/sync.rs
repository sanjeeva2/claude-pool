//! End-to-end: a real hub on loopback and two teammates with separate fake home directories.

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_sync::apply::{LocalState, Paths};
use claude_sync::client::{self, HubClient};
use claude_sync::config::ClientConfig;
use claude_sync::hub::{self, HubConfig};
use claude_sync::secrets;
use serde_json::{Value, json};

const ALICE: &str = "cst-alice-publisher-token-0001";
const BOB: &str = "cst-bob-member-token-00000002";

struct Env {
    _tmp: tempfile::TempDir,
    url: String,
    alice: Paths,
    bob: Paths,
    team: PathBuf,
}

impl Env {
    fn cfg(&self, token: &str) -> ClientConfig {
        ClientConfig {
            hub: self.url.clone(),
            token: token.into(),
        }
    }
    fn client(&self, token: &str) -> HubClient {
        HubClient::new(&self.cfg(token)).unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

async fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let hub_cfg = HubConfig::from_toml(&format!(
        "data_dir = \"{}\"\n[[members]]\nname = \"alice\"\ntoken = \"{ALICE}\"\nrole = \"publisher\"\n[[members]]\nname = \"bob\"\ntoken = \"{BOB}\"\n",
        tmp.path().join("hub").display()
    ))
    .unwrap();
    let h = hub::Hub::new(hub_cfg).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, hub::app(h)).await.unwrap() });

    let alice = Paths::from_home(&tmp.path().join("alice"));
    let bob = Paths::from_home(&tmp.path().join("bob"));
    // Bob already has a personal setup that must survive.
    write(
        &bob.claude_dir.join("settings.json"),
        r#"{"model": "opus", "permissions": {"allow": ["Bash(ls:*)"]}}"#,
    );
    write(
        &bob.claude_json,
        r#"{"numStartups": 42, "oauthAccount": {"emailAddress": "bob@example.com"}, "mcpServers": {"mine": {"command": "my-server"}}}"#,
    );
    write(
        &bob.claude_dir.join("CLAUDE.md"),
        "# Bob's notes\nPrefer short answers.\n",
    );
    write(&bob.claude_dir.join("commands/mine.md"), "my own command");

    let team = tmp.path().join("team");
    write(&team.join("CLAUDE.md"), "Use pnpm, never npm.");
    write(
        &team.join("settings.json"),
        r#"{"model": "sonnet", "permissions": {"allow": ["Bash(git status:*)", "Bash(pnpm test:*)"], "deny": ["Read(./.env)"]},
            "hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "pnpm lint --fix"}]}]},
            "env": {"TEAM_REGION": "eu"}}"#,
    );
    write(
        &team.join("mcp.json"),
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "{{secret:GITHUB_TOKEN}}"}},
            "docs": {"type": "http", "url": "https://docs.example.com/mcp"}}}"#,
    );
    write(
        &team.join("commands/review.md"),
        "Review the diff for bugs.",
    );
    write(
        &team.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\n---\nHow to read PDFs.",
    );
    write(&team.join("skills/pdf/scripts/extract.py"), "print('hi')");
    write(&team.join("README.md"), "not synced");

    Env {
        _tmp: tmp,
        url: format!("http://{addr}"),
        alice,
        bob,
        team,
    }
}

#[tokio::test]
async fn publish_pull_merge_and_update() {
    let env = setup().await;
    let alice = env.client(ALICE);
    let bob = env.client(BOB);
    let b = &env.bob;

    let r = alice.publish_dir(&env.team, "first", false).await.unwrap();
    assert_eq!(r.rev, 1);

    let rep = client::pull(b, &bob, false).await.unwrap();
    assert_eq!(rep.rev, 1);

    // Files were added; Bob's own command is untouched.
    assert_eq!(
        std::fs::read_to_string(b.claude_dir.join("commands/review.md")).unwrap(),
        "Review the diff for bugs."
    );
    assert!(b.claude_dir.join("skills/pdf/scripts/extract.py").exists());
    assert_eq!(
        std::fs::read_to_string(b.claude_dir.join("commands/mine.md")).unwrap(),
        "my own command"
    );
    assert!(!b.claude_dir.join("README.md").exists());

    // settings.json: union of rules, Bob's model kept, team env + hooks added.
    let s = read_json(&b.claude_dir.join("settings.json"));
    assert_eq!(s["model"], "opus");
    assert_eq!(
        s["permissions"]["allow"],
        json!(["Bash(ls:*)", "Bash(git status:*)", "Bash(pnpm test:*)"])
    );
    assert_eq!(s["permissions"]["deny"], json!(["Read(./.env)"]));
    assert_eq!(s["env"]["TEAM_REGION"], "eu");
    assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Edit");
    assert!(rep.conflicts.iter().any(|c| c.contains("model")));

    // ~/.claude.json: only mcpServers touched; github skipped until Bob sets his token.
    let cj = read_json(&b.claude_json);
    assert_eq!(cj["numStartups"], 42);
    assert_eq!(cj["oauthAccount"]["emailAddress"], "bob@example.com");
    assert_eq!(cj["mcpServers"]["mine"]["command"], "my-server");
    assert_eq!(
        cj["mcpServers"]["docs"]["url"],
        "https://docs.example.com/mcp"
    );
    assert!(cj["mcpServers"].get("github").is_none());
    assert!(rep.warnings.iter().any(|w| w.contains("GITHUB_TOKEN")));

    // CLAUDE.md: Bob's text first, team block after.
    let md = std::fs::read_to_string(b.claude_dir.join("CLAUDE.md")).unwrap();
    assert!(md.starts_with("# Bob's notes\nPrefer short answers.\n\n<!-- claude-sync:team:begin"));
    assert!(md.contains("Use pnpm, never npm."));

    // Bob adds his own token: the server now appears, with his value, and nothing else moves.
    let mut s = secrets::Secrets::new();
    s.insert("GITHUB_TOKEN".into(), "ghp_bobs_own_token_value".into());
    secrets::save(&b.secrets_file(), &s).unwrap();
    client::pull(b, &bob, false).await.unwrap();
    let cj = read_json(&b.claude_json);
    assert_eq!(
        cj["mcpServers"]["github"]["env"]["GITHUB_TOKEN"],
        "ghp_bobs_own_token_value"
    );

    // Bob edits a team command locally.
    write(
        &b.claude_dir.join("commands/review.md"),
        "Bob's tweaked review",
    );

    // Alice changes things: drop a rule, change the command, delete the skill, add an agent.
    write(
        &env.team.join("settings.json"),
        r#"{"permissions": {"allow": ["Bash(git status:*)"]}, "env": {"TEAM_REGION": "us"}}"#,
    );
    write(&env.team.join("commands/review.md"), "Review v2.");
    std::fs::remove_dir_all(env.team.join("skills")).unwrap();
    write(
        &env.team.join("agents/tester.md"),
        "---\nname: tester\n---\nWrite tests.",
    );
    write(
        &env.team.join("CLAUDE.md"),
        "Use pnpm. Run tests before committing.",
    );
    assert_eq!(
        alice
            .publish_dir(&env.team, "second", false)
            .await
            .unwrap()
            .rev,
        2
    );

    let rep = client::pull(b, &bob, false).await.unwrap();
    let s = read_json(&b.claude_dir.join("settings.json"));
    assert_eq!(
        s["permissions"]["allow"],
        json!(["Bash(ls:*)", "Bash(git status:*)"])
    );
    assert!(s["permissions"]["deny"].as_array().unwrap().is_empty());
    assert_eq!(s["env"]["TEAM_REGION"], "us");
    assert!(s["hooks"]["PostToolUse"].as_array().unwrap().is_empty());
    assert_eq!(s["model"], "opus");
    // Bob's edit wins and is reported.
    assert_eq!(
        std::fs::read_to_string(b.claude_dir.join("commands/review.md")).unwrap(),
        "Bob's tweaked review"
    );
    assert!(rep.conflicts.iter().any(|c| c.contains("review.md")));
    // Removed skill is cleaned up, including empty directories.
    assert!(!b.claude_dir.join("skills").exists());
    assert!(b.claude_dir.join("agents/tester.md").exists());
    let md = std::fs::read_to_string(b.claude_dir.join("CLAUDE.md")).unwrap();
    assert!(md.contains("Run tests before committing.") && !md.contains("never npm"));
    assert!(md.starts_with("# Bob's notes"));

    // Pulling again is a no-op.
    let rep = client::pull(b, &bob, false).await.unwrap();
    assert!(rep.changes.is_empty(), "{:?}", rep.changes);
    assert_eq!(LocalState::load(b).unwrap().applied_rev, 2);
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let env = setup().await;
    env.client(ALICE)
        .publish_dir(&env.team, "first", false)
        .await
        .unwrap();
    let before = std::fs::read(env.bob.claude_dir.join("settings.json")).unwrap();
    let rep = client::pull(&env.bob, &env.client(BOB), true)
        .await
        .unwrap();
    assert!(!rep.changes.is_empty());
    assert_eq!(
        std::fs::read(env.bob.claude_dir.join("settings.json")).unwrap(),
        before
    );
    assert!(!env.bob.claude_dir.join("commands/review.md").exists());
    assert_eq!(LocalState::load(&env.bob).unwrap().applied_rev, 0);
    let text = claude_sync::apply::render(&rep, true);
    assert!(text.contains("+++ after sync") && text.contains("Bash(git status:*)"));
}

#[tokio::test]
async fn connected_teammates_get_updates_live() {
    let env = setup().await;
    let alice = env.client(ALICE);
    alice.publish_dir(&env.team, "first", false).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = tokio::spawn(client::connect(env.bob.clone(), env.cfg(BOB), move |r| {
        let _ = tx.send(r.rev);
    }));
    // On connect Bob catches up to revision 1.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(1)
    );

    // Alice publishes; Bob's machine has it within moments, with no command run.
    write(&env.team.join("commands/deploy.md"), "Deploy carefully.");
    let started = std::time::Instant::now();
    alice
        .publish_dir(&env.team, "add deploy", false)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(2)
    );
    assert!(env.bob.claude_dir.join("commands/deploy.md").exists());
    println!("live update applied in {:?}", started.elapsed());

    // The hub knows Bob is connected and in sync.
    let mut ok = false;
    for _ in 0..100 {
        let st = alice.status().await.unwrap();
        if st
            .connected
            .iter()
            .any(|p| p.member == "bob" && p.applied_rev == 2)
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "presence not reported");
    agent.abort();
}

#[tokio::test]
async fn access_control_and_safety() {
    let env = setup().await;
    // Members cannot publish.
    let e = env
        .client(BOB)
        .publish_dir(&env.team, "x", false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("403"), "{e}");
    // Bad tokens are rejected.
    assert!(
        env.client("cst-not-a-real-token-xxxxxx")
            .head()
            .await
            .unwrap_err()
            .to_string()
            .contains("401")
    );

    // Credentials in team files are refused by the client, and by the hub too.
    write(
        &env.team.join("mcp.json"),
        r#"{"mcpServers": {"gh": {"command": "x", "env": {"T": "ghp_realLookingToken12345678"}}}}"#,
    );
    let e = env
        .client(ALICE)
        .publish_dir(&env.team, "x", false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("credentials"), "{e}");
    let e = env
        .client(ALICE)
        .publish_dir(&env.team, "x", true)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("422") && e.to_string().contains("GitHub token"),
        "{e}"
    );

    // Path traversal through the raw API is rejected.
    let http = reqwest::Client::new();
    let body = b"evil".to_vec();
    let sha = claude_sync::bundle::sha256_hex(&body);
    let put = http
        .put(format!("{}/api/blobs/{sha}", env.url))
        .bearer_auth(ALICE)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 204);
    let resp = http
        .post(format!("{}/api/publish", env.url))
        .bearer_auth(ALICE)
        .json(&json!({"base_rev": 0, "message": "x", "files": {"commands/../../.bashrc": sha}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    // Blob hash must match its content.
    let bad = http
        .put(format!("{}/api/blobs/{}", env.url, "0".repeat(64)))
        .bearer_auth(ALICE)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn dedup_conflicts_and_rollback() {
    let env = setup().await;
    let alice = env.client(ALICE);
    assert_eq!(
        alice
            .publish_dir(&env.team, "first", false)
            .await
            .unwrap()
            .rev,
        1
    );
    let again = alice.publish_dir(&env.team, "same", false).await.unwrap();
    assert!(again.unchanged && again.rev == 1);

    // Only the changed file's content has to be uploaded.
    write(&env.team.join("commands/review.md"), "changed");
    let (manifest, blobs) = claude_sync::bundle::scan_dir(&env.team).unwrap();
    let resp: claude_sync::proto::MissingResponse = reqwest::Client::new()
        .post(format!("{}/api/blobs/missing", env.url))
        .bearer_auth(ALICE)
        .json(&json!({"shas": blobs.keys().collect::<Vec<_>>()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.missing, vec![manifest["commands/review.md"].clone()]);

    // Publishing on top of a stale revision is refused.
    let stale = reqwest::Client::new()
        .post(format!("{}/api/publish", env.url))
        .bearer_auth(ALICE)
        .json(&json!({"base_rev": 0, "message": "stale", "files": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);

    assert_eq!(
        alice
            .publish_dir(&env.team, "second", false)
            .await
            .unwrap()
            .rev,
        2
    );
    let rb = alice.rollback(1).await.unwrap();
    assert_eq!(rb.rev, 3);
    assert_eq!(
        alice.head().await.unwrap().files,
        alice.rev(1).await.unwrap().files
    );
    let hist = alice.revs().await.unwrap();
    assert_eq!(
        hist.iter().map(|r| r.rev).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert_eq!(hist[0].message, "rollback to revision 1");
}

#[tokio::test]
async fn init_team_extracts_secrets() {
    let env = setup().await;
    let a = &env.alice;
    write(&a.claude_dir.join("CLAUDE.md"), "Team conventions.");
    write(&a.claude_dir.join("commands/ship.md"), "Ship it.");
    write(
        &a.claude_dir.join("settings.json"),
        r#"{"model": "opus", "permissions": {"allow": ["Bash(make:*)"]}, "env": {"API_TOKEN": "abc123secretvalue", "REGION": "eu"}}"#,
    );
    write(
        &a.claude_json,
        r#"{"oauthAccount": {}, "mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp", "headers": {"Authorization": "Bearer lin_api_xyz"}}}}"#,
    );

    let dir = env._tmp.path().join("alice-team");
    let r = claude_sync::init::init_team(a, &dir).unwrap();
    assert!(r.files.contains(&"commands/ship.md".to_string()));
    let settings = read_json(&dir.join("settings.json"));
    assert!(
        settings.get("model").is_none(),
        "personal preferences are not exported"
    );
    assert_eq!(settings["env"]["API_TOKEN"], "{{secret:ENV_API_TOKEN}}");
    assert_eq!(settings["env"]["REGION"], "eu");
    let mcp = read_json(&dir.join("mcp.json"));
    assert_eq!(
        mcp["mcpServers"]["linear"]["headers"]["Authorization"],
        "{{secret:LINEAR_HEADER_AUTHORIZATION}}"
    );
    let mine = secrets::load(&a.secrets_file()).unwrap();
    assert_eq!(mine["LINEAR_HEADER_AUTHORIZATION"], "Bearer lin_api_xyz");

    // The bootstrapped directory publishes cleanly, and Alice's own pull restores her values.
    env.client(ALICE)
        .publish_dir(&dir, "bootstrap", false)
        .await
        .unwrap();
    client::pull(a, &env.client(ALICE), false).await.unwrap();
    let cj = read_json(&a.claude_json);
    assert_eq!(
        cj["mcpServers"]["linear"]["headers"]["Authorization"],
        "Bearer lin_api_xyz"
    );
    let s = read_json(&a.claude_dir.join("settings.json"));
    assert_eq!(s["env"]["API_TOKEN"], "abc123secretvalue");
    assert_eq!(s["model"], "opus");
}

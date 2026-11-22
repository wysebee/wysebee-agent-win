//! Pairing with Wysebee Cloud, and the ways a pairing ends (the agent-link protocol, "Pairing"
//! and "The app's poll"), against the fake cloud.

mod common;

use agent_core::cloud::Source;
use agent_core::link::secret_hash;
use agent_core::secrets::SecretStore;
use agent_core::{Agent, Step};
use common::{fake, pair, program, Bench, Mode, LINK_ID};
use serde_json::json;

#[tokio::test]
async fn pairing_shows_a_code_and_ends_paired_as_a_new_agent() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    let before = agent.status();
    assert_eq!((before.state, before.connected, before.code.clone()), ("unpaired", false, None));
    assert_eq!((before.cloud_url.as_str(), before.cloud_url_source), (fake.url.as_str(), Source::Env));
    assert_eq!(agent.step().await, Step::Idle, "nothing to ask while unpaired");

    agent.start_pairing("  PC helper ").await.unwrap();
    let secret = bench.secrets.peek().expect("the secret is kept in the secret store");
    assert_eq!(secret.len(), 43);
    let start = fake.get(|c| c.starts[0].clone());
    assert_eq!(start, json!({"kind": "wysebee-windows", "name": "PC helper", "secret_hash": secret_hash(&secret), "browser": "Test Surface Laptop Studio 2 wit", "extension_version": "0.1.0"}));
    assert_eq!(start["browser"].as_str().unwrap().chars().count(), 32);

    let waiting = agent.status();
    assert_eq!((waiting.state, waiting.code.as_deref(), waiting.name.as_deref()), ("waiting", Some("ABCD-EFGH"), Some("PC helper")));
    assert!(waiting.expires_in.unwrap() > 590 && waiting.expires_in.unwrap() <= 600);
    assert!(waiting.agent.is_none() && waiting.message.is_none());
    assert_eq!(agent.start_pairing("Another").await.unwrap_err(), "This PC is already paired, or waiting for its code.");

    assert_eq!(agent.step().await, Step::Pending);
    let pending = agent.status();
    assert!(pending.connected && pending.expires_in.unwrap() <= 432, "the cloud's count of the time left is taken");
    assert_eq!(fake.get(|c| c.bearers.last().cloned()).as_deref(), Some(secret.as_str()), "the secret is the poll's bearer token");
    assert_eq!(fake.last_status(), json!({"paused": false, "extension_version": "0.1.0", "browser": "Test Surface Laptop Studio 2 wit", "approved": [], "approved_skills": [], "folders": [], "any_program": false, "screen_control": false, "dev_access": false, "android_dev": false}));

    // The code is typed in Wysebee.
    fake.set(|c| {
        c.mode = Mode::Paired;
        c.programs = Some(vec![program("hello", "/bin/echo", json!([{"value": "hi"}]))]);
    });
    assert_eq!(agent.step().await, Step::Paired { calls: 0 });
    let paired = agent.status();
    assert_eq!((paired.state, paired.connected, paired.code.clone(), paired.expires_in), ("paired", true, None, None));
    assert_eq!(paired.agent.as_ref().map(|a| (a.name.as_str(), a.label.as_str())), Some(("pc-helper", "PC helper")));
    assert_eq!((paired.programs.len(), paired.waiting), (1, 1));

    // The state file holds the pairing and never the secret.
    let text = bench.state_text();
    assert!(text.contains(LINK_ID) && text.contains("pc-helper") && text.contains("\"hello\""));
    assert!(!text.contains(&secret) && !text.contains(&secret_hash(&secret)) && !text.to_lowercase().contains("secret"));

    // A new launch finds it all again, with the secret from its store.
    let again = Agent::open(bench.options(Some(&fake.url), Bench::timing()));
    assert_eq!(again.state(), agent.state());
    assert_eq!(again.status().state, "paired");
    assert!(!again.status().connected, "not until it has asked");
    assert_eq!(again.step().await, Step::Paired { calls: 0 });
    assert!(again.status().connected);
}

#[tokio::test]
async fn a_pairing_that_cannot_start_leaves_nothing_behind() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    for bad in ["", "   ", "two\nlines"] {
        assert!(agent.start_pairing(bad).await.unwrap_err().starts_with("Type a name for the agent"));
    }
    assert!(agent.start_pairing(&"n".repeat(61)).await.is_err());
    assert_eq!(fake.get(|c| c.starts.len()), 0, "a name that cannot be one is not sent");

    // The cloud's own refusal, in its words.
    fake.set(|c| c.start_answer = Some((422, json!({"detail": "That name cannot be used.", "code": "invalid_name"}))));
    assert_eq!(agent.start_pairing("PC helper").await.unwrap_err(), "That name cannot be used.");
    // An answer with no sentence, and one that is not a pairing.
    fake.set(|c| c.start_answer = Some((500, json!("oops"))));
    assert_eq!(agent.start_pairing("PC helper").await.unwrap_err(), "Wysebee Cloud answered 500.");
    fake.set(|c| c.start_answer = Some((200, json!({"id": "../../evil", "code": "ABCDEFGH"}))));
    assert_eq!(agent.start_pairing("PC helper").await.unwrap_err(), "Wysebee Cloud's answer could not be read.");
    fake.set(|c| c.start_answer = Some((200, json!({"id": LINK_ID, "code": "<b>"}))));
    assert_eq!(agent.start_pairing("PC helper").await.unwrap_err(), "Wysebee Cloud's answer could not be read.");
    fake.set(|c| c.start_answer = Some((200, json!("not an object"))));
    assert!(agent.start_pairing("PC helper").await.is_err());
    assert!(bench.secrets.peek().is_none() && agent.state().link.is_none());
    assert_eq!(agent.status().state, "unpaired");

    // A secret store that cannot be written: no pairing.
    fake.set(|c| c.start_answer = None);
    bench.secrets.fail_with(Some("The Credential Manager could not be written: locked"));
    assert_eq!(agent.start_pairing("PC helper").await.unwrap_err(), "The Credential Manager could not be written: locked");
    assert!(agent.state().link.is_none());
    bench.secrets.fail_with(None);

    // A cloud nobody answers at.
    let nowhere = Agent::open(bench.options(Some("http://127.0.0.1:1"), Bench::timing()));
    assert_eq!(nowhere.start_pairing("PC helper").await.unwrap_err(), "Wysebee Cloud could not be reached.");
    assert!(bench.secrets.peek().is_none());
}

#[tokio::test]
async fn a_refused_claim_says_why_and_starts_over() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    agent.start_pairing("Wysebee").await.unwrap();
    fake.set(|c| c.mode = Mode::Refused("An agent already has that name. Pick another.".into()));
    assert_eq!(agent.step().await, Step::Refused);
    let s = agent.status();
    assert_eq!((s.state, s.message.as_deref(), s.code.clone()), ("refused", Some("An agent already has that name. Pick another."), None));
    assert!(bench.secrets.peek().is_none() && agent.state().link.is_none(), "the pairing and its secret are forgotten");
    assert_eq!(agent.step().await, Step::Idle);

    // Dismissed, or simply started again with another name.
    agent.cancel_pairing().await;
    assert_eq!(agent.status().state, "unpaired");
    fake.set(|c| c.mode = Mode::Refused(" ".into()));
    agent.start_pairing("Another").await.unwrap();
    assert_eq!(agent.step().await, Step::Refused);
    assert_eq!(agent.status().message.as_deref(), Some("Wysebee refused that name. Start again with another."));
    fake.set(|c| c.mode = Mode::Pending);
    agent.start_pairing("Third").await.unwrap();
    assert_eq!((agent.status().state, agent.status().message), ("waiting", None));
    assert_eq!(fake.get(|c| c.starts.len()), 3);
}

#[tokio::test]
async fn a_code_that_ran_out_is_forgotten() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    agent.start_pairing("PC helper").await.unwrap();
    fake.set(|c| c.mode = Mode::Expired);
    assert_eq!(agent.step().await, Step::Expired);
    let s = agent.status();
    assert_eq!((s.state, s.note.as_deref()), ("unpaired", Some("The code ran out before it was typed. Start again.")));
    assert!(bench.secrets.peek().is_none() && agent.state().link.is_none());
    // The note goes with the next pairing.
    fake.set(|c| c.mode = Mode::Pending);
    agent.start_pairing("PC helper").await.unwrap();
    assert_eq!(agent.status().note, None);
}

#[tokio::test]
async fn unpaired_in_wysebee_forgets_the_pairing_the_secret_and_the_approvals() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    bench.script("hello", "out hi");
    fake.set(|c| c.programs = Some(vec![program("hello", "hello", json!([]))]));
    pair(&agent, &fake).await;
    agent.allow("hello").unwrap();
    assert_eq!(agent.state().approved.len(), 1);

    fake.set(|c| c.mode = Mode::Unpaired);
    assert_eq!(agent.step().await, Step::Unpaired);
    let s = agent.status();
    assert_eq!((s.state, s.connected), ("unpaired", false));
    assert_eq!(s.note.as_deref(), Some("This PC was unpaired in Wysebee, or its agent was deleted."));
    let state = agent.state();
    assert!(state.link.is_none() && state.programs.is_empty() && state.approved.is_empty());
    assert!(bench.secrets.peek().is_none(), "the secret is deleted");
    assert!(!bench.state_text().contains(LINK_ID));
    assert_eq!(agent.step().await, Step::Idle);

    // A secret the cloud does not know is a 401 too (its hash is of another).
    fake.set(|c| c.mode = Mode::Paired);
    agent.start_pairing("PC helper").await.unwrap();
    bench.secrets.set("some-other-secret").unwrap();
    let fresh = Agent::open(bench.options(Some(&fake.url), Bench::timing()));
    assert_eq!(fresh.step().await, Step::Unpaired);
    assert!(bench.secrets.peek().is_none());
}

#[tokio::test]
async fn a_pairing_whose_secret_is_gone_is_forgotten_and_a_store_that_fails_is_not() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    pair(&agent, &fake).await;

    // A new launch whose store cannot be read: the pairing stays, the poll waits.
    let locked = Agent::open(bench.options(Some(&fake.url), Bench::timing()));
    bench.secrets.fail_with(Some("The Credential Manager could not be read: locked"));
    assert_eq!(locked.step().await, Step::Failed("The Credential Manager could not be read: locked".into()));
    assert!(locked.state().link.is_some() && !locked.status().connected);
    bench.secrets.fail_with(None);
    assert_eq!(locked.step().await, Step::Paired { calls: 0 });

    // A new launch whose store holds nothing: there is no pairing without its secret.
    bench.secrets.delete().unwrap();
    let empty = Agent::open(bench.options(Some(&fake.url), Bench::timing()));
    let polls = fake.get(|c| c.polls.len());
    assert_eq!(empty.step().await, Step::Unpaired);
    assert_eq!(empty.status().note.as_deref(), Some("This PC's pairing key is no longer in the Credential Manager. Pair it again."));
    assert!(empty.state().link.is_none());
    assert_eq!(fake.get(|c| c.polls.len()), polls, "nothing was asked without a secret");
}

#[tokio::test]
async fn unpairing_and_cancelling_tell_the_cloud() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);

    // Cancel while the code waits to be typed.
    agent.start_pairing("PC helper").await.unwrap();
    let secret = bench.secrets.peek().unwrap();
    agent.cancel_pairing().await;
    assert_eq!(fake.get(|c| (c.deletes, c.bearers.last().cloned())), (1, Some(secret)));
    assert_eq!((agent.status().state, agent.status().note), ("unpaired", None));
    assert!(bench.secrets.peek().is_none());

    // Unpair once paired.
    pair(&agent, &fake).await;
    agent.cancel_pairing().await;
    assert_eq!(agent.status().state, "paired", "cancelling is not unpairing");
    agent.unpair().await;
    assert_eq!(fake.get(|c| c.deletes), 2);
    assert_eq!((agent.status().state, agent.status().note), ("unpaired", None));
    assert!(bench.secrets.peek().is_none() && agent.state().link.is_none());
    agent.unpair().await;
    assert_eq!(fake.get(|c| c.deletes), 2, "nothing to unpair");

    // A cloud that cannot be told: unpaired here all the same, and the person is told.
    pair(&agent, &fake).await;
    fake.set(|c| c.delete_status = 500);
    agent.unpair().await;
    let s = agent.status();
    assert_eq!(s.state, "unpaired");
    assert_eq!(s.note.as_deref(), Some("Wysebee Cloud could not be told. Unpair this PC in Wysebee too, under Devices."));
    assert!(bench.secrets.peek().is_none());

    // A secret store that fails: unpaired here, the cloud not told.
    fake.set(|c| c.delete_status = 204);
    pair(&agent, &fake).await;
    let fresh = Agent::open(bench.options(Some(&fake.url), Bench::timing()));
    bench.secrets.fail_with(Some("locked"));
    let deletes = fake.get(|c| c.deletes);
    fresh.unpair().await;
    assert_eq!(fake.get(|c| c.deletes), deletes);
    assert!(fresh.state().link.is_none());
}

#[tokio::test]
async fn the_cloud_address_is_https_or_this_computer_and_a_link_keeps_its_own() {
    let (fake, bench) = (fake().await, Bench::new());
    // No environment: the default, until one is saved from the window.
    let agent = Agent::open(bench.options(None, Bench::timing()));
    assert_eq!(agent.cloud(), ("https://app.wysebee.com".to_string(), Source::Default));
    for bad in ["http://example.com", "http://localhost@evil.example", "ftp://localhost", "example.com", "http://192.168.0.2:8000"] {
        assert!(agent.set_cloud_url(bad).unwrap_err().contains("https://"), "{bad}");
    }
    assert_eq!(agent.cloud().1, Source::Default);
    agent.set_cloud_url("https://staging.example.com/").unwrap();
    assert_eq!(agent.cloud(), ("https://staging.example.com".to_string(), Source::Saved));
    agent.set_cloud_url("").unwrap();
    assert_eq!(agent.cloud().1, Source::Default);

    // Saved: this computer's fake cloud, which the pairing is then made with.
    agent.set_cloud_url(&format!("{}/", fake.url)).unwrap();
    assert_eq!(agent.status().cloud_url, fake.url);
    pair(&agent, &fake).await;
    assert_eq!(agent.state().link.unwrap().cloud_url, fake.url);
    assert_eq!(agent.set_cloud_url("https://other.example").unwrap_err(), "Unpair first: the address is used when this PC is paired.");

    // The saved address wins over the environment's, and the link keeps the cloud it was
    // made with: its secret goes nowhere else.
    let moved = Agent::open(bench.options(Some("https://elsewhere.example"), Bench::timing()));
    assert_eq!(moved.cloud(), (fake.url.clone(), Source::Saved));
    assert_eq!((moved.status().cloud_url, moved.status().cloud_url_source, moved.status().cloud_default), (fake.url.clone(), Source::Saved, "https://elsewhere.example".to_string()));
    assert_eq!(moved.step().await, Step::Paired { calls: 0 });

    // An environment address that does not pass the rule is not used.
    let insecure = Agent::open(bench.options(Some("http://evil.example"), Bench::timing()));
    assert_eq!(insecure.status().cloud_default, "https://app.wysebee.com");
}

#[tokio::test]
async fn the_environment_is_only_the_default_a_saved_address_wins_over() {
    let bench = Bench::new();
    let agent = Agent::open(bench.options(Some("http://localhost:9/"), Bench::timing()));
    assert_eq!(agent.cloud(), ("http://localhost:9".to_string(), Source::Env));
    agent.set_cloud_url("http://localhost:8001").unwrap();
    let saved = agent.status();
    assert_eq!((saved.cloud_url.as_str(), saved.cloud_url_source, saved.cloud_default.as_str()), ("http://localhost:8001", Source::Saved, "http://localhost:9"));
    // Kept across a restart, still over the environment's.
    let again = Agent::open(bench.options(Some("http://localhost:9"), Bench::timing()));
    assert_eq!(again.cloud(), ("http://localhost:8001".to_string(), Source::Saved));
    // "Use the default": the environment's again.
    again.set_cloud_url("").unwrap();
    assert_eq!(again.cloud(), ("http://localhost:9".to_string(), Source::Env));
}

#[tokio::test]
async fn the_time_left_is_the_clouds_count_when_it_gives_one_and_only_a_real_change_is_kept() {
    let (fake, bench) = (fake().await, Bench::new());
    let agent = bench.agent(&fake);
    agent.start_pairing("PC helper").await.unwrap();
    let started = agent.state().link.unwrap().expires_at.unwrap();

    // An answer without the time left: the code's own count stands.
    fake.set(|c| c.expires_in = None);
    assert_eq!(agent.step().await, Step::Pending);
    assert_eq!(agent.state().link.unwrap().expires_at, Some(started));
    assert!(agent.status().expires_in.unwrap() > 590);

    // The cloud's count is taken…
    fake.set(|c| c.expires_in = Some(432));
    assert_eq!(agent.step().await, Step::Pending);
    let taken = agent.state().link.unwrap().expires_at.unwrap();
    assert!(taken < started && agent.status().expires_in.unwrap() <= 432);
    let text = bench.state_text();
    // …but a count a second or two off is the same time: nothing changes, nothing is written.
    fake.set(|c| c.expires_in = Some(434));
    assert_eq!(agent.step().await, Step::Pending);
    assert_eq!(agent.state().link.unwrap().expires_at, Some(taken));
    assert_eq!(bench.state_text(), text);
}

#[tokio::test]
async fn the_link_follows_no_redirect() {
    use axum::http::StatusCode;
    use axum::response::Redirect;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // A cloud that sends the request elsewhere: the secret's hash must not follow it.
    let followed = Arc::new(AtomicUsize::new(0));
    let counted = followed.clone();
    let app = axum::Router::new().route("/api/v1/agent-links/start", post(|| async { Redirect::temporary("/elsewhere") })).route(
        "/elsewhere",
        post(move || async move {
            counted.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = agent_core::link::Client::default();
    let answer = client.start(&base, "PC helper", "a-secret", "Test PC", "0.1.0", std::time::Duration::from_secs(5)).await;
    assert_eq!(answer.unwrap_err().message(), "Wysebee Cloud answered 307.");
    assert_eq!(followed.load(Ordering::SeqCst), 0);
}

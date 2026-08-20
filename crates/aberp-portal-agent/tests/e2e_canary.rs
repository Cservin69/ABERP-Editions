//! The canary trap, end to end.
//!
//! Same loopback harness as `e2e_portal.rs`: a real front with a real
//! aggregator task, a real tunnel, a real agent. The alert sink is the
//! file sink, so nothing touches SMTP or the keychain.
//!
//! Three claims, in the order they matter:
//!
//! 1. **The trap changes nothing.** A probe that trips it — including
//!    the decoy — receives the byte-identical uniform 404. If this test
//!    ever fails, the trap has become the fingerprint the whole design
//!    exists to avoid, and it should be ripped out rather than patched.
//! 2. **A probe is recorded and alerted**, with the right severity.
//! 3. **A legitimate flow is silent.** Knock, enrol, authenticate, read
//!    invoices — no canary, no alert.

mod harness;

use std::time::Duration;

use harness::authenticator::VirtualAuthenticator;
use harness::{start_portal, Portal, TRIPWIRE_PATH};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// Everything a prober can observe, minus `Date`.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn observe(c: &reqwest::Client, url: &str, host: Option<&str>) -> Observed {
    let mut req = c.get(url);
    if let Some(h) = host {
        req = req.header("host", h);
    }
    let res = req.send().await.expect("the front must answer every probe");
    let status = res.status().as_u16();
    let mut headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "date")
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string()))
        .collect();
    headers.sort();
    Observed {
        status,
        headers,
        body: res.bytes().await.expect("body").to_vec(),
    }
}

async fn enrol_and_authenticate(p: &Portal, c: &reqwest::Client, auth: &VirtualAuthenticator) {
    let token = p.agent.enrolment.mint("iPhone").expect("console enrolment");
    let begin: serde_json::Value = c
        .post(p.url("/api/enrol/begin"))
        .json(&serde_json::json!({ "token": token }))
        .send()
        .await
        .expect("begin")
        .json()
        .await
        .expect("json");
    let challenge = begin["options"]["challenge"].as_str().expect("challenge");
    let mut body = auth.register_verified(&p.rp_id, &p.origin, challenge);
    body["token"] = serde_json::Value::String(token);
    let res = c
        .post(p.url("/api/enrol/finish"))
        .json(&body)
        .send()
        .await
        .expect("finish");
    assert_eq!(res.status().as_u16(), 200);
}

fn severities(samples: &[serde_json::Value]) -> Vec<String> {
    samples
        .iter()
        .filter_map(|s| s.get("severity")?.as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trap_does_not_change_the_response() {
    // Claim 1, and the one that outranks the rest. Four probes that
    // classify differently — background noise, the decoy, a knock-shaped
    // guess, and a request naming the portal's hostname — must be
    // indistinguishable to the prober.
    let p = start_portal("canary-uniform").await;
    let c = client();

    let noise = observe(&c, &format!("{}/wp-login.php", p.base), None).await;
    let decoy = observe(&c, &format!("{}{TRIPWIRE_PATH}", p.base), None).await;
    let guess = observe(&c, &format!("{}/{}", p.base, "A".repeat(43)), None).await;
    let named = observe(&c, &format!("{}/", p.base), Some(&p.rp_id)).await;

    assert_eq!(noise.status, 404);
    assert_eq!(
        noise, decoy,
        "the decoy answered differently — the tripwire is findable"
    );
    assert_eq!(noise, guess, "a knock-shaped guess answered differently");
    assert_eq!(noise, named, "naming the host answered differently");
    assert_eq!(
        noise.body,
        aberp_portal_relay::UNIFORM_404_BODY.as_bytes(),
        "the 404 drifted from the compiled-in constant"
    );
    assert!(!noise.headers.iter().any(|(k, _)| k == "set-cookie"));
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknocked_probe_is_recorded_on_the_mac() {
    // Claim 2. The record travels front → aggregator → tunnel → agent,
    // all of it off the response path.
    let p = start_portal("canary-record").await;
    let c = client();

    let res = c
        .get(format!("{}/.env", p.base))
        .header("user-agent", "masscan/1.3")
        .send()
        .await
        .expect("probe");
    assert_eq!(res.status().as_u16(), 404);

    let samples = p.await_canary(1).await;
    assert!(!samples.is_empty(), "the probe was never recorded");
    let s = &samples[0];
    assert_eq!(s["severity"], serde_json::json!("low"));
    assert_eq!(s["reason"], serde_json::json!("background_noise"));
    assert_eq!(s["path"], serde_json::json!("/.env"));
    assert_eq!(s["user_agent"], serde_json::json!("masscan/1.3"));
    assert_eq!(s["method"], serde_json::json!("GET"));
    assert!(s["source_ip"]
        .as_str()
        .is_some_and(|ip| ip.contains("127.0.0.1")));

    // Metadata-only, structurally: nothing that could hold content.
    for record in p.canary_log() {
        let obj = record.as_object().expect("object");
        for forbidden in ["body", "body_b64", "query", "cookie", "token", "knock"] {
            assert!(
                !obj.contains_key(forbidden),
                "probe log leaked `{forbidden}`"
            );
        }
    }
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tripwire_is_an_instant_high_severity_canary() {
    // Claim 2, the dedicated decoy. Nothing references this path, so a
    // hit is unambiguous — and it must not wait for the ordinary flush
    // cadence.
    let p = start_portal("canary-tripwire").await;
    let c = client();

    let res = c
        .get(format!("{}{TRIPWIRE_PATH}", p.base))
        .send()
        .await
        .expect("probe");
    assert_eq!(res.status().as_u16(), 404);
    assert_eq!(
        res.text().await.expect("body"),
        aberp_portal_relay::UNIFORM_404_BODY
    );

    let samples = p.await_canary(1).await;
    assert_eq!(severities(&samples), vec!["high"]);
    assert_eq!(samples[0]["reason"], serde_json::json!("tripwire"));

    // …and Ervin was told, once.
    let alerts = p.alerts();
    assert!(alerts.contains("HIGH"), "no high-severity alert: {alerts}");
    assert_eq!(alerts.matches("Subject:").count(), 1);
    assert!(
        alerts.contains("rotate-knock"),
        "the alert should say what to do about it"
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn naming_the_portal_hostname_is_high_but_the_label_never_reaches_the_alert() {
    // "Someone guessed or leaked the label" is the loudest signal this
    // design has. The alert says so without repeating the label into a
    // mailbox — the same value the wildcard certificate keeps out of CT.
    let p = start_portal("canary-host").await;
    let c = client();

    let res = c
        .get(format!("{}/", p.base))
        .header("host", &p.rp_id)
        .send()
        .await
        .expect("probe");
    assert_eq!(res.status().as_u16(), 404);

    let samples = p.await_canary(1).await;
    assert_eq!(severities(&samples), vec!["high"]);
    assert_eq!(samples[0]["reason"], serde_json::json!("named_the_host"));
    assert_eq!(samples[0]["named_the_host"], serde_json::json!(true));

    let alerts = p.alerts();
    assert!(alerts.contains("addressed the portal by name"));
    assert!(
        !alerts.contains(&p.rp_id),
        "the alert repeated the portal label into an email: {alerts}"
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_burst_is_one_alert_not_a_flood() {
    // The coalescing requirement, across both ceilings: the front
    // batches, and the agent rate-limits.
    let p = start_portal("canary-burst").await;
    let c = client();

    for i in 0..60 {
        let _ = c
            .get(format!("{}{TRIPWIRE_PATH}?probe={i}", p.base))
            .send()
            .await;
    }

    let samples = p.await_canary(1).await;
    assert!(!samples.is_empty());
    // Every probe is counted even though only a capped sample survives:
    // the batch summaries' totals add up to the whole burst.
    for _ in 0..200 {
        let total: u64 = p
            .canary_log()
            .iter()
            .filter_map(|v| v.get("total")?.as_u64())
            .sum();
        if total >= 60 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let total: u64 = p
        .canary_log()
        .iter()
        .filter_map(|v| v.get("total")?.as_u64())
        .sum();
    assert!(total >= 60, "probes went uncounted: {total}");

    let alerts = p.alerts();
    assert_eq!(
        alerts.matches("Subject:").count(),
        1,
        "a 60-probe burst produced more than one alert:\n{alerts}"
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legitimate_knock_and_passkey_flow_trips_no_canary() {
    // Claim 3. The alert that fires during normal use is the alert that
    // gets ignored.
    let p = start_portal("canary-quiet").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(21);

    // The full legitimate path: shell, enrol, authenticate, read.
    let shell = c.get(p.url("/")).send().await.expect("shell");
    assert_eq!(shell.status(), 200);
    enrol_and_authenticate(&p, &c, &iphone).await;
    let status = c.get(p.url("/api/status")).send().await.expect("status");
    assert_eq!(status.status(), 200);
    let invoices = c
        .get(p.url("/api/invoices"))
        .send()
        .await
        .expect("invoices");
    assert_eq!(invoices.status(), 200);

    // Give the aggregator more than a flush interval's worth of chances
    // to have recorded something it should not have.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        p.canary_log().is_empty(),
        "a legitimate flow tripped the canary: {:?}",
        p.canary_log()
    );
    assert!(p.alerts().is_empty(), "a legitimate flow sent an alert");
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operators_own_favicon_request_is_suppressed_not_alerted() {
    // The concrete false positive the grace window exists for: after a
    // portal visit, the browser asks the BARE HOST for /favicon.ico —
    // no knock, but the portal's hostname. Without suppression every
    // legitimate visit would page Ervin at HIGH.
    let p = start_portal("canary-favicon").await;
    let c = client();

    // Pass the knock first, which is what marks the source authorised.
    assert_eq!(c.get(p.url("/")).send().await.expect("shell").status(), 200);

    let res = c
        .get(format!("{}/favicon.ico", p.base))
        .header("host", &p.rp_id)
        .send()
        .await
        .expect("favicon");
    assert_eq!(res.status().as_u16(), 404);

    let samples = p.await_canary(1).await;
    assert_eq!(
        severities(&samples),
        vec!["suppressed"],
        "the operator's own browser was classified as a probe"
    );
    assert!(
        p.alerts().is_empty(),
        "a suppressed probe still paged someone: {}",
        p.alerts()
    );
    p.stub.stop();
}

#[test]
fn the_shell_declares_inline_icons_so_the_request_is_not_made_at_all() {
    // Belt to the grace window's braces — and the §3.2 answer too: no
    // favicon is served, because none is fetched.
    let shell = aberp_portal_relay::front::SHELL_HTML;
    assert!(shell.contains(r#"<link rel="icon" href="data:,">"#));
    assert!(shell.contains(r#"<link rel="apple-touch-icon" href="data:,">"#));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mutating_verb_on_an_unknocked_path_is_still_only_a_404_and_a_canary() {
    // A prober using POST learns nothing more than one using GET.
    let p = start_portal("canary-verbs").await;
    let c = client();

    for method in [
        reqwest::Method::POST,
        reqwest::Method::DELETE,
        reqwest::Method::PUT,
    ] {
        let res = c
            .request(method.clone(), format!("{}{TRIPWIRE_PATH}", p.base))
            .send()
            .await
            .expect("probe");
        assert_eq!(res.status().as_u16(), 404, "{method}");
        assert_eq!(
            res.text().await.expect("body"),
            aberp_portal_relay::UNIFORM_404_BODY
        );
    }

    let samples = p.await_canary(1).await;
    assert!(samples
        .iter()
        .any(|s| s["severity"] == serde_json::json!("high")));
    let methods: Vec<_> = samples
        .iter()
        .filter_map(|s| s.get("method")?.as_str())
        .collect();
    assert!(
        methods.contains(&"POST"),
        "the verb was not recorded: {methods:?}"
    );
    p.stub.stop();
}

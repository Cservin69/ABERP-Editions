//! The ADR-0113 Phase 0 end-to-end proof, on loopback.
//!
//! One narrative test walks the whole path the ADR describes —
//!
//! ```text
//! browser -> front -> knock -> passkey (register + authenticate)
//!         -> relay -> agent -> local ABERP read -> render
//! ```
//!
//! — and the tests around it pin the properties that path is supposed
//! to have: uniform 404 without the knock, read-only enforced at the
//! agent, ABERP-down reported by an agent that is still up.
//!
//! Every component is the real one except the three named in
//! `harness/mod.rs`: ABERP itself (a contract-shaped stub), the
//! authenticator (a software P-256 key), and Leg A's TLS (plaintext
//! loopback, no wildcard certificate available here).

mod harness;

use std::sync::atomic::Ordering;
use std::time::Duration;

use harness::authenticator::{VirtualAuthenticator, FLAG_AT, FLAG_UP};
use harness::{start_portal, Portal, INVOICE_ID, PDF_MAGIC};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

async fn post_json(
    c: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let res = c.post(url).json(body).send().await.expect("post");
    let status = res.status().as_u16();
    let value = res.json().await.unwrap_or(serde_json::Value::Null);
    (status, value)
}

async fn get_json(c: &reqwest::Client, url: &str) -> (u16, serde_json::Value) {
    let res = c.get(url).send().await.expect("get");
    let status = res.status().as_u16();
    let value = res.json().await.unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Register the iPhone passkey through a console-minted enrolment
/// window, exactly as ADR-0113 §4.3 requires.
async fn enrol_device(
    p: &Portal,
    c: &reqwest::Client,
    auth: &VirtualAuthenticator,
    label: &str,
) -> u16 {
    let token = p.agent.enrolment.mint(label).expect("console enrolment");
    let (status, begin) = post_json(
        c,
        &p.url("/api/enrol/begin"),
        &serde_json::json!({ "token": token }),
    )
    .await;
    assert_eq!(status, 200, "enrol/begin refused: {begin}");
    let challenge = begin["options"]["challenge"].as_str().expect("challenge");

    let mut body = auth.register_verified(&p.rp_id, &p.origin, challenge);
    body["token"] = serde_json::Value::String(token);
    let (status, _) = post_json(c, &p.url("/api/enrol/finish"), &body).await;
    status
}

/// Authenticate an already-enrolled passkey.
async fn authenticate(p: &Portal, c: &reqwest::Client, auth: &VirtualAuthenticator) -> u16 {
    let (status, begin) = post_json(c, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    assert_eq!(status, 200, "auth/begin refused: {begin}");
    let challenge = begin["challenge"].as_str().expect("challenge");
    let body = auth.assert_verified(&p.rp_id, &p.origin, challenge);
    let (status, _) = post_json(c, &p.url("/api/auth/finish"), &body).await;
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_path_works_end_to_end() {
    let p = start_portal("full").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(1);

    // 1. The knocked URL serves the shell — and nothing else does.
    let shell = c.get(p.url("/")).send().await.expect("shell");
    assert_eq!(shell.status(), 200);
    let shell_body = shell.text().await.expect("body");
    assert!(shell_body.contains("navigator.credentials"));
    // Passwordless, passkey-only (Ervin's §9.1 decision): the shell has
    // no input element of any kind, so there is no field to type a
    // secret into and none to phish. Checked structurally rather than by
    // searching for the word, which the file's own header comment uses.
    let lower = shell_body.to_ascii_lowercase();
    assert!(!lower.contains("<input"), "the shell grew an input element");
    assert!(!lower.contains("<form"), "the shell grew a form");
    assert!(!lower.contains("type=\"password\""));

    // 2. Nothing is readable before a passkey exists.
    let (status, _) = get_json(&c, &p.url("/api/status")).await;
    assert_eq!(status, 401, "the status card must require a session");
    let (status, _) = get_json(&c, &p.url("/api/invoices")).await;
    assert_eq!(status, 401);

    // 3. Remote enrolment is impossible without a console-minted token.
    let (status, _) = post_json(
        &c,
        &p.url("/api/enrol/begin"),
        &serde_json::json!({ "token": "made-up" }),
    )
    .await;
    assert_eq!(status, 401, "enrolment must be console-gated (§4.3)");

    // 4. Enrol at the console, then complete the ceremony in the browser.
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    // 5. The status card renders, from the agent's own observations.
    let (status, view) = get_json(&c, &p.url("/api/status")).await;
    assert_eq!(status, 200);
    assert_eq!(view["aberp_up"], serde_json::json!(true));
    assert!(view["agent_uptime_seconds"].is_number());

    // 6. The read surface: list, detail, PDF — all four §6.2 rows.
    let (status, list) = get_json(&c, &p.url("/api/invoices")).await;
    assert_eq!(status, 200);
    assert_eq!(list[0]["id"], serde_json::json!(INVOICE_ID));

    let (status, detail) = get_json(&c, &p.url(&format!("/api/invoices/{INVOICE_ID}"))).await;
    assert_eq!(status, 200);
    assert_eq!(detail["id"], serde_json::json!(INVOICE_ID));

    let pdf = c
        .get(p.url(&format!("/api/invoices/{INVOICE_ID}/pdf")))
        .send()
        .await
        .expect("pdf");
    assert_eq!(pdf.status(), 200);
    assert_eq!(
        pdf.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    assert_eq!(pdf.bytes().await.expect("pdf bytes").as_ref(), PDF_MAGIC);

    let (status, upstream_health) = get_json(&c, &p.url("/api/health")).await;
    assert_eq!(status, 200);
    assert_eq!(upstream_health["ok"], serde_json::json!(true));

    // 7. A second, cookie-less browser is a stranger again.
    let stranger = client();
    let (status, _) = get_json(&stranger, &p.url("/api/status")).await;
    assert_eq!(status, 401);

    // 8. And re-authenticating with the passkey restores access —
    //    the §4.4 "one Face ID glance away" path, without a password.
    assert_eq!(authenticate(&p, &stranger, &iphone).await, 200);
    let (status, _) = get_json(&stranger, &p.url("/api/status")).await;
    assert_eq!(status, 200);

    // 9. The audit log recorded the ceremonies and the reads (§6.5),
    //    and carries no bodies.
    let kinds = p.audit_kinds();
    for expected in [
        "portal.tunnel.up",
        "portal.enrol.challenge_issued",
        "portal.enrol.registered",
        "portal.session.minted",
        "portal.proxy.ok",
        "portal.auth.verified",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "missing audit kind {expected}"
        );
    }
    for record in p.audit() {
        let obj = record.as_object().expect("audit record");
        for forbidden in ["body", "body_b64", "query", "cookie", "token"] {
            assert!(
                !obj.contains_key(forbidden),
                "audit record leaked `{forbidden}`"
            );
        }
    }
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mutating_request_is_refused_at_the_agent() {
    // ADR-0113 §G5 / §6.3. The relay forwards the verb verbatim; the
    // Mac is what says no. The stub's mutating routes are live and
    // counting, so "refused" is proven by nothing arriving, not merely
    // by a status code.
    let p = start_portal("readonly").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(2);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    for (method, path, expected) in [
        (reqwest::Method::POST, "/api/invoices".to_string(), 405),
        (
            reqwest::Method::PUT,
            format!("/api/invoices/{INVOICE_ID}"),
            405,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/invoices/{INVOICE_ID}"),
            405,
        ),
        (reqwest::Method::PATCH, "/api/invoices".to_string(), 405),
        // A GET at a mutating upstream route: refused by shape, not verb.
        (reqwest::Method::GET, "/api/invoices/issue".to_string(), 404),
        (
            reqwest::Method::GET,
            format!("/api/invoices/{INVOICE_ID}/submit"),
            404,
        ),
    ] {
        let res = c
            .request(method.clone(), p.url(&path))
            .send()
            .await
            .expect("send");
        assert_eq!(
            res.status().as_u16(),
            expected,
            "{method} {path} was not refused as expected"
        );
    }

    assert_eq!(
        p.stub.counters.mutating_hits.load(Ordering::Relaxed),
        0,
        "a mutating request reached ABERP — the read-only claim is false"
    );

    // The refusals are in the audit log, as loudly as the successes.
    let refusals: Vec<_> = p
        .audit()
        .into_iter()
        .filter(|r| r["kind"] == serde_json::json!("portal.proxy.refused"))
        .collect();
    assert!(
        refusals.len() >= 6,
        "expected every refusal to be audited, saw {}",
        refusals.len()
    );
    assert!(refusals
        .iter()
        .any(|r| r["reason"] == serde_json::json!("method not allowed (read-only portal)")));
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reports_aberp_down_while_staying_up_itself() {
    // §5.3 row 2 — "the raison d'être of the separate agent".
    let p = start_portal("down").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(3);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    let (_, up) = get_json(&c, &p.url("/api/status")).await;
    assert_eq!(up["aberp_up"], serde_json::json!(true));

    p.stub.stop();
    // Give the listener a moment to actually stop accepting.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (status, down) = get_json(&c, &p.url("/api/status")).await;
    assert_eq!(status, 200, "the portal itself must still answer");
    assert_eq!(
        down["aberp_up"],
        serde_json::json!(false),
        "the agent did not notice ABERP going away: {down}"
    );
    assert!(
        down["last_good"].is_string(),
        "last-known-good must survive the outage (§5.1)"
    );
    assert!(down["detail"].is_string());

    // And the reads are refused server-side, not merely hidden in the UI.
    let (status, body) = get_json(&c, &p.url("/api/invoices")).await;
    assert_eq!(status, 503, "a read must be refused while ABERP is down");
    assert_eq!(body["aberp_up"], serde_json::json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_knock_gets_the_uniform_404_even_for_an_enrolled_operator() {
    // §3.3: the knock decides whether the door is visible at all. An
    // enrolled user with a stale bookmark is indistinguishable from a
    // scanner.
    let p = start_portal("knock").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(4);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    let wrong = format!("{}/{}/api/status", p.base, "A".repeat(p.knock.len()));
    let res = c.get(&wrong).send().await.expect("send");
    assert_eq!(res.status().as_u16(), 404);
    assert_eq!(
        res.text().await.expect("body"),
        aberp_portal_relay::UNIFORM_404_BODY
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_an_assertion_without_user_verification() {
    // §4.3: `userVerification: required`. A tap that did not fire the
    // biometric must not open the portal.
    let p = start_portal("uv").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(5);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    let stranger = client();
    let (status, begin) =
        post_json(&stranger, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    assert_eq!(status, 200);
    let challenge = begin["challenge"].as_str().expect("challenge");
    // Presence only: UP set, UV clear.
    let body = iphone.assert(&p.rp_id, &p.origin, challenge, FLAG_UP);
    let (status, _) = post_json(&stranger, &p.url("/api/auth/finish"), &body).await;
    assert_eq!(status, 401, "a presence-only assertion must be refused");

    let reasons: Vec<_> = p
        .audit()
        .into_iter()
        .filter(|r| r["kind"] == serde_json::json!("portal.auth.failed"))
        .map(|r| r["reason"].clone())
        .collect();
    assert!(reasons.contains(&serde_json::json!("user verification absent")));
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_an_assertion_minted_for_another_origin() {
    // §G3 — the phishing-resistance property. Same passkey, same
    // challenge, a look-alike origin.
    let p = start_portal("origin").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(6);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    let stranger = client();
    let (_, begin) = post_json(&stranger, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    let challenge = begin["challenge"].as_str().expect("challenge");
    let body = iphone.assert_verified(&p.rp_id, "https://evil.test", challenge);
    let (status, _) = post_json(&stranger, &p.url("/api/auth/finish"), &body).await;
    assert_eq!(
        status, 401,
        "an assertion for another origin must be refused"
    );

    let reasons: Vec<_> = p
        .audit()
        .into_iter()
        .filter(|r| r["kind"] == serde_json::json!("portal.auth.failed"))
        .map(|r| r["reason"].clone())
        .collect();
    assert!(reasons.contains(&serde_json::json!("origin mismatch")));
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_a_replayed_challenge() {
    // §4.3 single-use nonce. The same assertion, posted twice.
    let p = start_portal("replay").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(7);
    assert_eq!(enrol_device(&p, &c, &iphone, "iPhone").await, 200);

    let stranger = client();
    let (_, begin) = post_json(&stranger, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    let challenge = begin["challenge"].as_str().expect("challenge");
    let body = iphone.assert_verified(&p.rp_id, &p.origin, challenge);

    let (first, _) = post_json(&stranger, &p.url("/api/auth/finish"), &body).await;
    assert_eq!(first, 200);
    let replay = client();
    let (second, _) = post_json(&replay, &p.url("/api/auth/finish"), &body).await;
    assert_eq!(second, 401, "a replayed assertion must be refused");
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_a_passkey_that_was_never_enrolled() {
    let p = start_portal("unknown").await;
    let c = client();
    let enrolled = VirtualAuthenticator::new(8);
    assert_eq!(enrol_device(&p, &c, &enrolled, "iPhone").await, 200);

    let attacker = VirtualAuthenticator::new(9);
    let stranger = client();
    let (_, begin) = post_json(&stranger, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    let challenge = begin["challenge"].as_str().expect("challenge");
    let body = attacker.assert_verified(&p.rp_id, &p.origin, challenge);
    let (status, _) = post_json(&stranger, &p.url("/api/auth/finish"), &body).await;
    assert_eq!(status, 401);
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_enrolment_token_is_single_use() {
    // §4.3. Even inside the 10-minute window, the URL works once.
    let p = start_portal("single-use").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(10);
    let token = p.agent.enrolment.mint("iPhone").expect("mint");

    let (status, begin) = post_json(
        &c,
        &p.url("/api/enrol/begin"),
        &serde_json::json!({ "token": token }),
    )
    .await;
    assert_eq!(status, 200);
    let challenge = begin["options"]["challenge"].as_str().expect("challenge");
    let mut body = iphone.register_verified(&p.rp_id, &p.origin, challenge);
    body["token"] = serde_json::Value::String(token.clone());
    let (status, _) = post_json(&c, &p.url("/api/enrol/finish"), &body).await;
    assert_eq!(status, 200);

    // Second use of the same URL: the window is closed.
    let (status, _) = post_json(
        &c,
        &p.url("/api/enrol/begin"),
        &serde_json::json!({ "token": token }),
    )
    .await;
    assert_eq!(status, 401);
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registration_without_attested_credential_data_is_refused() {
    // A malformed authenticator response must not become a credential.
    let p = start_portal("noat").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(11);
    let token = p.agent.enrolment.mint("iPhone").expect("mint");
    let (_, begin) = post_json(
        &c,
        &p.url("/api/enrol/begin"),
        &serde_json::json!({ "token": token }),
    )
    .await;
    let challenge = begin["options"]["challenge"].as_str().expect("challenge");
    // AT flag cleared: no credential data in the authData.
    let mut body = iphone.register(&p.rp_id, &p.origin, challenge, FLAG_UP | 0x04);
    body["token"] = serde_json::Value::String(token);
    let (status, _) = post_json(&c, &p.url("/api/enrol/finish"), &body).await;
    assert_eq!(status, 401);
    assert!(
        p.agent.credentials.load().expect("load").is_empty(),
        "a refused registration must not store a credential"
    );
    let _ = FLAG_AT;
    p.stub.stop();
}

//! The ADR-0115 Phase 0 end-to-end proof, on loopback.
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

/// Drive a full enrolment ceremony through a console-minted window.
///
/// Since ADR-0115 §4.3a this is a **negative** path for any software
/// authenticator: the ceremony is well-formed and correctly signed, the
/// enrolment token is genuine and unspent, and it is still refused,
/// because the attestation does not chain to Apple. That refusal is the
/// enrolment defence, and
/// `a_software_credential_cannot_enrol` is where it is asserted.
///
/// Tests that need an ENROLLED device use
/// [`Portal::provision_credential`] instead — the console-confirmation
/// step (§4.3b), which is how a credential legitimately arrives.
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
    p.provision_credential(&iphone, "iPhone");
    assert_eq!(authenticate(&p, &c, &iphone).await, 200);

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
        // Renamed with the transport: there is no tunnel to come up,
        // only a poll loop that starts asking (§2.1).
        "portal.leg_b.up",
        "portal.auth.challenge_issued",
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
    // ADR-0115 §G5 / §6.3. The relay forwards the verb verbatim; the
    // Mac is what says no. The stub's mutating routes are live and
    // counting, so "refused" is proven by nothing arriving, not merely
    // by a status code.
    let p = start_portal("readonly").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(2);
    p.provision_credential(&iphone, "iPhone");
    assert_eq!(authenticate(&p, &c, &iphone).await, 200);

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
    p.provision_credential(&iphone, "iPhone");
    // The console step commits the credential; signing in is still a
    // real assertion against it (§4.4).
    assert_eq!(authenticate(&p, &c, &iphone).await, 200);

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
    p.provision_credential(&iphone, "iPhone");

    let wrong = format!("{}/{}/api/status", p.base, "A".repeat(p.knock.len()));
    let res = c.get(&wrong).send().await.expect("send");
    assert_eq!(res.status().as_u16(), 404);
    assert_eq!(
        res.text().await.expect("body"),
        aberp_portal_relay::Class::NotFound.body()
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_an_assertion_without_user_verification() {
    // §4.3: `userVerification: required`. A tap that did not fire the
    // biometric must not open the portal.
    let p = start_portal("uv").await;

    let iphone = VirtualAuthenticator::new(5);
    p.provision_credential(&iphone, "iPhone");

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

    let iphone = VirtualAuthenticator::new(6);
    p.provision_credential(&iphone, "iPhone");

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

    let iphone = VirtualAuthenticator::new(7);
    p.provision_credential(&iphone, "iPhone");

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

    let enrolled = VirtualAuthenticator::new(8);
    p.provision_credential(&enrolled, "iPhone");

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
async fn security_headers_are_on_the_shell_and_nowhere_else() {
    // §3.2 (should-fix 6). CSP, `Referrer-Policy`,
    // `X-Content-Type-Options` and HSTS belong on the authenticated
    // surface — it is a real browser context with real invoice data in
    // it — and must be ABSENT from the parked answers, because a parked
    // nginx sends none of them and a response that did would be unique
    // on the whole host.
    let p = start_portal("headers").await;
    let c = client();

    let shell = c.get(p.url("/")).send().await.expect("shell");
    assert_eq!(shell.status(), 200);
    let h = shell.headers().clone();
    let csp = h
        .get("content-security-policy")
        .expect("the shell has no CSP")
        .to_str()
        .expect("utf8");
    assert!(
        csp.contains("frame-ancestors 'none'"),
        "the load-bearing directive is missing: {csp}"
    );
    // The knock token is IN THE PATH; without this, following any
    // outbound link hands the whole gate to a third party.
    assert_eq!(
        h.get("referrer-policy").and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    assert_eq!(
        h.get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert!(h.get("strict-transport-security").is_some());
    assert_eq!(
        h.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    // …and the parked surface carries none of them.
    let parked = c
        .get(format!("{}/definitely-not-the-knock/x", p.base))
        .send()
        .await
        .expect("parked");
    assert_eq!(parked.status().as_u16(), 404);
    for forbidden in [
        "content-security-policy",
        "referrer-policy",
        "x-content-type-options",
        "x-frame-options",
        "strict-transport-security",
        "cache-control",
    ] {
        assert!(
            parked.headers().get(forbidden).is_none(),
            "the parked answer leaked `{forbidden}` — it is not what nginx sends"
        );
    }
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_cookie_is_scoped_to_the_knock_prefix() {
    // §4.4 (should-fix 8). A cookie at `Path=/` is offered to EVERY
    // path on the host, including the un-knocked ones — so any request
    // that brushes the bare hostname (a mistyped URL, a prefetch, an
    // embedded image) would carry the session. Scoping it to the knock
    // prefix means the browser only ever offers it inside the portal.
    let p = start_portal("cookiepath").await;
    let c = client();
    let iphone = VirtualAuthenticator::new(11);
    p.provision_credential(&iphone, "iPhone");

    let (_, begin) = post_json(&c, &p.url("/api/auth/begin"), &serde_json::json!({})).await;
    let challenge = begin["challenge"].as_str().expect("challenge");
    let body = iphone.assert_verified(&p.rp_id, &p.origin, challenge);
    let res = c
        .post(p.url("/api/auth/finish"))
        .json(&body)
        .send()
        .await
        .expect("finish");
    assert_eq!(res.status().as_u16(), 200);

    let cookie = res
        .headers()
        .get("set-cookie")
        .expect("no session cookie")
        .to_str()
        .expect("utf8");
    assert!(
        cookie.contains(&format!("Path=/{}/", p.knock)),
        "the session cookie is not scoped to the knock: {cookie}"
    );
    assert_eq!(
        cookie.matches("Path=").count(),
        1,
        "more than one Path attribute: {cookie}"
    );
    // The agent's own flags survive the relay stamping the path.
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pre_auth_status_does_not_publish_the_enrolment_window() {
    // should-fix 7. This endpoint is reachable by anyone holding the
    // knock and nothing else, so `enrolment_open` was a live oracle:
    // poll it a few times a minute and learn the exact 10-minute window
    // in which a registration ceremony is accepted at all.
    let p = start_portal("nooracle").await;
    let c = client();

    let (status, closed) = get_json(&c, &p.url("/api/session")).await;
    assert_eq!(status, 200);
    assert!(
        closed.get("enrolment_open").is_none(),
        "the enrolment window is still published: {closed}"
    );

    // …and it stays absent while a window is genuinely open, which is
    // the case that matters.
    p.agent.enrolment.mint("iPhone").expect("console enrolment");
    let (_, open) = get_json(&c, &p.url("/api/session")).await;
    assert!(
        open.get("enrolment_open").is_none(),
        "an open window is observable: {open}"
    );
    assert_eq!(
        closed, open,
        "opening a window changed the pre-auth response"
    );
    p.stub.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_software_credential_cannot_enrol() {
    // ADR-0115 §4.3a, end to end, through the real front, the real
    // relay, the real poll transport and the real relying party.
    //
    // Everything about this attempt is legitimate except the hardware:
    // the enrolment token was minted at the console and is unspent, the
    // ceremony is well-formed, the challenge is live, the origin and RP
    // ID match, user verification is asserted, and the P-256 signature
    // is real. Under `attestation: "none"` it enrolled. It must now be
    // refused, because the attestation does not chain to the Apple
    // WebAuthn Root CA.
    //
    // This is the exact capability a compromised relay had: until
    // hardening H1 the ceremony crosses relay memory in plaintext, so
    // the relay can see a live console-minted token — and with nothing
    // to distinguish a Secure Enclave key from one it generated itself,
    // it could turn that glimpse into a credential, which is STANDING
    // access that outlives knock rotation and the cleanup.
    let p = start_portal("attestation").await;
    let c = client();
    let software = VirtualAuthenticator::new(42);

    let status = enrol_device(&p, &c, &software, "Attacker").await;
    assert_eq!(status, 401, "a software credential was enrolled");

    // Nothing was written, nothing was staged, and no session exists.
    assert!(
        p.agent.credentials.load().expect("store").is_empty(),
        "a refused enrolment still wrote a credential"
    );
    assert!(
        p.agent.staging.peek().is_err(),
        "a refused enrolment still staged a credential for confirmation"
    );
    assert_eq!(
        p.agent.sessions.len(),
        0,
        "a refused enrolment minted a session"
    );

    // …and the refusal is named in the audit log, distinctly from a
    // merely malformed one: this line is what tells Ervin somebody
    // tried to enrol hardware they do not have.
    let kinds = p.audit_kinds();
    assert!(
        kinds.iter().any(|k| k == "portal.enrol.failed"),
        "the attempt was not audited: {kinds:?}"
    );
    let reasons: Vec<String> = p
        .audit()
        .iter()
        .filter_map(|r| r.get("reason")?.as_str().map(str::to_string))
        .collect();
    assert!(
        reasons
            .iter()
            .any(|r| r == "attestation not Apple hardware"),
        "the refusal reason was not recorded: {reasons:?}"
    );

    // The operator can still enrol — the console path is unaffected,
    // which is what stops this defence from being a lockout.
    p.provision_credential(&software, "iPhone");
    assert_eq!(authenticate(&p, &c, &software).await, 200);
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
    // Refused on attestation (§4.3a) — and that is precisely the case
    // worth testing here. The token is consumed BEFORE the ceremony is
    // verified, so a failed attempt still burns the window. If it did
    // not, an attacker who reached a live token could retry against it
    // indefinitely.
    assert_eq!(status, 401, "a software credential must not enrol");

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

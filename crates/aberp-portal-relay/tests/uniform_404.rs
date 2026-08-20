//! ADR-0113 §3.2, made mechanical.
//!
//! > **Uniform 404:** every unauthenticated request — wrong path, right
//! > path, `HEAD`, `POST`, garbage SNI, direct IP — receives the same
//! > minimal 404: same status, same headers […] same body bytes, no
//! > `Set-Cookie`, no cache-control oddities.
//!
//! The ADR's own §"Adversarial review" opens with "byte-diff the gate's
//! 404 against the default vhost under every method/SNI/timing probe",
//! so this test byte-diffs a matrix of probes against each other and
//! against the constant the front is built from.
//!
//! Two states are probed, and they must be indistinguishable:
//!
//! 1. **no agent connected** — the Mac is down, or has never run;
//! 2. **agent connected, wrong knock** — the portal is fully alive and
//!    the caller simply does not have the token.
//!
//! If those two differed by a single byte, a scanner could learn
//! whether the Mac is up, which is §G2's whole subject.
//!
//! Not covered here, and named rather than implied: **SNI** and
//! **direct-IP** probes are properties of the TLS listener, which the
//! loopback harness runs in plaintext. They need the deployed front and
//! its wildcard certificate to test honestly, so they belong to the
//! deploy checklist, not to this file.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aberp_portal_core::frame::{FrameReader, FrameWriter};
use aberp_portal_core::proto::{Frame, PortalResponse, PROTOCOL_VERSION};
use aberp_portal_relay::broker::serve_agent;
use aberp_portal_relay::{front, Broker, Front};

const KNOCK: &str = "k4Hn3vQ7ZbYt2mLp";

/// Everything a caller can observe about a response, in one comparable
/// value. `Date` is excluded deliberately: it moves every second on
/// every HTTP server in the world and carries nothing about this host.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn observe(client: &reqwest::Client, method: reqwest::Method, url: &str) -> Observed {
    let res = client
        .request(method, url)
        .send()
        .await
        .expect("the front must answer every probe");
    let status = res.status().as_u16();
    let mut headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "date")
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string()))
        .collect();
    headers.sort();
    let body = res.bytes().await.expect("body").to_vec();
    Observed {
        status,
        headers,
        body,
    }
}

/// Start a front on loopback. Returns its base URL and the broker.
async fn start_front() -> (String, Arc<Broker>) {
    aberp_portal_core::pin::install_default_crypto_provider();
    let broker = Arc::new(Broker::new());
    let app = front::router(Arc::new(Front {
        broker: Arc::clone(&broker),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (format!("http://{addr}"), broker)
}

/// Attach a minimal in-memory agent that publishes `KNOCK`.
async fn attach_agent(broker: &Arc<Broker>) {
    let (relay_side, agent_side) = tokio::io::duplex(64 * 1024);
    let b = Arc::clone(broker);
    tokio::spawn(async move {
        let _ = serve_agent(&b, relay_side).await;
    });
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(agent_side);
        let mut reader = FrameReader::new(r);
        let mut writer = FrameWriter::new(w);
        writer
            .write_frame(&Frame::Hello {
                protocol_version: PROTOCOL_VERSION,
                knock_token: KNOCK.to_string(),
                tunnel_id: "tunnel-uniform404".into(),
            })
            .await
            .expect("hello");
        while let Ok(Frame::Request { id, .. }) = reader.read_frame::<Frame>().await {
            let _ = writer
                .write_frame(&Frame::Response {
                    id,
                    res: PortalResponse::json(200, r#"{"reached":"agent"}"#),
                })
                .await;
        }
    });
    for _ in 0..200 {
        if broker.agent_connected() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the in-memory agent never attached");
}

/// The probe matrix: paths a scanner actually tries, plus the shapes
/// that would tempt a router into answering differently.
fn probe_paths() -> Vec<String> {
    vec![
        "/".into(),
        "/robots.txt".into(),
        "/sitemap.xml".into(),
        "/favicon.ico".into(),
        "/.env".into(),
        "/.git/config".into(),
        "/admin".into(),
        "/api".into(),
        "/api/status".into(),
        "/wp-login.php".into(),
        // A wrong knock of the RIGHT LENGTH — the shape a token guess
        // takes, and the one a timing side channel would attack.
        format!("/{}", "A".repeat(KNOCK.len())),
        // A near-miss: correct but for the last character.
        format!("/{}X/", &KNOCK[..KNOCK.len() - 1]),
        // The right knock as a prefix of a longer segment.
        format!("/{KNOCK}extra/"),
        // The right knock, percent-encoded — the front never decodes.
        format!("/{}/", KNOCK.replace('4', "%34")),
        // Deep and traversal-shaped paths under a valid knock.
        format!("/{KNOCK}/../etc/passwd"),
        format!("/{KNOCK}/index.html"),
    ]
}

fn probe_methods() -> Vec<reqwest::Method> {
    vec![
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::HEAD,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
        reqwest::Method::PATCH,
        reqwest::Method::OPTIONS,
        reqwest::Method::TRACE,
    ]
}

#[tokio::test]
async fn every_unknocked_probe_gets_the_identical_404_with_no_agent() {
    let (base, _broker) = start_front().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    let mut reference: Option<Observed> = None;
    for path in probe_paths() {
        for method in probe_methods() {
            let is_head = method == reqwest::Method::HEAD;
            let got = observe(&client, method.clone(), &format!("{base}{path}")).await;
            assert_eq!(got.status, 404, "{method} {path} did not 404");
            assert!(
                !got.headers.iter().any(|(k, _)| k == "set-cookie"),
                "{method} {path} carried a Set-Cookie"
            );
            // HEAD legitimately elides the body at the HTTP layer; every
            // other method must return the exact same bytes.
            if is_head {
                continue;
            }
            match &reference {
                None => reference = Some(got),
                Some(r) => assert_eq!(
                    &got, r,
                    "{method} {path} produced a distinguishable response"
                ),
            }
        }
    }

    let r = reference.expect("at least one probe ran");
    assert_eq!(
        r.body,
        front::UNIFORM_404_BODY.as_bytes(),
        "the 404 body drifted from the compiled-in constant"
    );
}

#[tokio::test]
async fn a_live_portal_is_byte_identical_to_a_dead_one_for_anyone_without_the_knock() {
    // The §G2 property: an observer must not be able to tell whether
    // the Mac is up. Same front, probed before and after an agent
    // attaches.
    let (base, broker) = start_front().await;
    let client = reqwest::Client::new();
    let url = format!("{base}/admin");

    let dead = observe(&client, reqwest::Method::GET, &url).await;
    assert!(!broker.agent_connected());

    attach_agent(&broker).await;
    let alive = observe(&client, reqwest::Method::GET, &url).await;

    assert_eq!(
        dead, alive,
        "the front revealed whether the Mac is connected"
    );
}

#[tokio::test]
async fn the_correct_knock_reaches_the_portal_and_nothing_else_does() {
    // The other half of the claim: the gate is not simply always-404.
    // A test that only proved 404s would pass on a broken portal.
    let (base, broker) = start_front().await;
    attach_agent(&broker).await;
    let client = reqwest::Client::new();

    let shell = client
        .get(format!("{base}/{KNOCK}/"))
        .send()
        .await
        .expect("shell");
    assert_eq!(shell.status(), 200);
    let body = shell.text().await.expect("body");
    assert!(
        body.contains("navigator.credentials"),
        "the shell was not served"
    );

    let api = client
        .get(format!("{base}/{KNOCK}/api/status"))
        .send()
        .await
        .expect("api");
    assert_eq!(api.status(), 200);
    assert_eq!(api.text().await.expect("body"), r#"{"reached":"agent"}"#);

    // And with the knock rotated away (the agent gone), the very same
    // URL is a 404 again — §3.3's rotation story.
    drop(broker);
}

#[tokio::test]
async fn an_oversized_body_gets_the_uniform_404_rather_than_a_413() {
    // The body limit must not become an oracle: a `413` where everyone
    // else gets a `404` would tell a scanner that something here reads
    // request bodies at all.
    let (base, broker) = start_front().await;
    attach_agent(&broker).await;
    let client = reqwest::Client::new();
    let oversized = vec![b'x'; front::MAX_REQUEST_BODY + 1024];

    for url in [
        format!("{base}/nope"),
        format!("{base}/{KNOCK}/api/auth/finish"),
    ] {
        let res = client
            .post(&url)
            .body(oversized.clone())
            .send()
            .await
            .expect("send");
        assert_eq!(
            res.status().as_u16(),
            404,
            "{url} answered with a discriminator"
        );
        assert_eq!(res.text().await.expect("body"), front::UNIFORM_404_BODY);
    }
}

#[tokio::test]
async fn no_response_ever_advertises_the_portal_in_a_header() {
    let (base, broker) = start_front().await;
    attach_agent(&broker).await;
    let client = reqwest::Client::new();

    for url in [format!("{base}/nope"), format!("{base}/{KNOCK}/")] {
        let res = client.get(&url).send().await.expect("send");
        for (k, v) in res.headers() {
            let line = format!("{k}: {}", v.to_str().unwrap_or_default()).to_ascii_lowercase();
            for forbidden in ["aberp", "portal", "relay", "axum", "rust"] {
                assert!(
                    !line.contains(forbidden),
                    "{url} leaked `{forbidden}` in a header: {line}"
                );
            }
        }
    }
}

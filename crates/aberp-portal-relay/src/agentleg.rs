//! Leg B, from the relay's side (ADR-0115 §2.1) — two endpoints, and
//! nothing else.
//!
//! The Mac dials **out** to this listener over mutually-pinned TLS and
//! drives the whole conversation:
//!
//! - `POST /agent/v3/poll` renews the presence lease and collects
//!   whatever work is parked;
//! - `POST /agent/v3/deliver` posts one answer back.
//!
//! There is no third endpoint, no `GET`, and nothing the relay can
//! initiate. That is the transport decision made structural: if this
//! module grew a way for the relay to *ask* the Mac for something, the
//! "every leg is Mac-initiated" claim in §G1 would stop being checkable
//! by reading one file.
//!
//! # Why this listener also wears the disguise
//!
//! An unpinned peer never gets here — [`aberp_portal_core::pin`] fails
//! the handshake before any application byte, which §2.3 describes as
//! "indistinguishable from a closed service". So the parked-nginx
//! answers below are pure defence in depth, for the case where that is
//! somehow not true: a scan that does get through learns the same
//! nothing it learns from the front. It costs one `match` arm.
//!
//! No canary here, deliberately. The trap's whole premise is that this
//! host has no legitimate unauthenticated visitors, and the only party
//! that can reach this listener has already proved it holds the pinned
//! key. Feeding it would report the operator's own Mac as a probe.

use std::net::SocketAddr;
use std::sync::Arc;

use aberp_portal_core::proto::{Delivery, PollRequest, DELIVER_PATH, MAX_BODY_BYTES, POLL_PATH};

use crate::broker::Broker;
use crate::http1::{Answer, Handler, PortalAnswer, RequestHead};
use crate::nginx::Class;

/// The relay half of Leg B.
#[derive(Debug)]
pub struct AgentLeg {
    pub broker: Arc<Broker>,
}

impl Handler for AgentLeg {
    fn handle<'a>(
        &'a self,
        head: &'a RequestHead,
        body: &'a [u8],
        _peer: Option<SocketAddr>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Answer> + Send + 'a>> {
        Box::pin(self.respond(head, body))
    }

    fn observe_protocol_error(
        &self,
        _class: Class,
        _peer: Option<SocketAddr>,
        _hint: Option<&str>,
    ) {
        // See the module note: the only reachable peer is the pinned
        // Mac, and recording it as a probe would be a false positive
        // that trains the operator to ignore the alert.
    }

    fn max_body(&self) -> usize {
        // A delivery carries a whole response — an invoice PDF at the
        // top end. Bounded on BOTH sides of Leg B: the agent refuses an
        // oversized poll response from a hostile relay with the same
        // constant.
        MAX_BODY_BYTES
    }
}

impl AgentLeg {
    async fn respond(&self, head: &RequestHead, body: &[u8]) -> Answer {
        if head.method != "POST" {
            return Answer::not_found();
        }
        match head.path() {
            POLL_PATH => self.poll(body).await,
            DELIVER_PATH => self.deliver(body),
            _ => Answer::not_found(),
        }
    }

    async fn poll(&self, body: &[u8]) -> Answer {
        let Ok(req) = serde_json::from_slice::<PollRequest>(body) else {
            return Answer::not_found();
        };
        match self.broker.poll(&req).await {
            Ok(res) => json(200, &res),
            // A version skew must fail loudly at the agent, but it
            // must not announce itself on the wire. The agent logs the
            // refusal it sees; the socket shows a parked host.
            Err(e) => {
                tracing::warn!(error = %e, "refusing an agent poll");
                Answer::not_found()
            }
        }
    }

    fn deliver(&self, body: &[u8]) -> Answer {
        let Ok(delivery) = serde_json::from_slice::<Delivery>(body) else {
            return Answer::not_found();
        };
        json(200, &self.broker.deliver(delivery))
    }
}

/// One JSON answer to the Mac.
fn json<T: serde::Serialize>(status: u16, value: &T) -> Answer {
    let Ok(body) = serde_json::to_vec(value) else {
        return Answer::not_found();
    };
    Answer::Portal(Box::new(PortalAnswer {
        status,
        reason: "OK",
        content_type: "application/json".to_string(),
        body,
        set_cookie: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_portal_core::proto::{AgentIdentity, PollResponse, PROTOCOL_VERSION};

    fn head(method: &str, path: &str) -> RequestHead {
        RequestHead {
            method: method.to_string(),
            target: path.to_string(),
            version: crate::http1::Version::Http11,
            headers: vec![("host".into(), "relay".into())],
            client_wants_keep_alive: true,
        }
    }

    fn leg() -> AgentLeg {
        AgentLeg {
            broker: Arc::new(Broker::new()),
        }
    }

    fn poll_body(epoch: &str) -> Vec<u8> {
        serde_json::to_vec(&PollRequest {
            agent: AgentIdentity {
                protocol_version: PROTOCOL_VERSION,
                knock_token: "t".into(),
                expected_host: None,
                tripwire_path: "/d".into(),
                epoch: epoch.into(),
            },
            wait_ms: 0,
            ack_canary_seq: 0,
        })
        .expect("json")
    }

    fn body_of(answer: &Answer) -> Vec<u8> {
        match answer {
            Answer::Portal(p) => p.body.clone(),
            Answer::Nginx(_) => panic!("expected a portal answer, got the parked one"),
        }
    }

    #[tokio::test]
    async fn a_poll_renews_the_lease_and_answers_json() {
        let l = leg();
        let a = l.respond(&head("POST", POLL_PATH), &poll_body("e1")).await;
        let res: PollResponse = serde_json::from_slice(&body_of(&a)).expect("decodes");
        assert!(res.work.is_empty());
        assert_eq!(res.heartbeat.seq, 1);
        assert!(l.broker.agent_present(), "the poll renewed the lease");
    }

    #[tokio::test]
    async fn every_other_shape_is_the_parked_answer() {
        // No GET, no third endpoint, no unparseable body, and nothing
        // that distinguishes one refusal from another.
        let l = leg();
        for (m, p, b) in [
            ("GET", POLL_PATH, poll_body("e1")),
            ("POST", "/agent/v3/anything-else", poll_body("e1")),
            ("POST", "/", poll_body("e1")),
            ("POST", POLL_PATH, b"not json".to_vec()),
            ("POST", DELIVER_PATH, b"not json".to_vec()),
        ] {
            assert!(
                matches!(
                    l.respond(&head(m, p), &b).await,
                    Answer::Nginx(Class::NotFound)
                ),
                "{m} {p} was not the parked answer"
            );
        }
    }

    #[tokio::test]
    async fn a_version_skew_looks_like_a_parked_host_on_the_wire() {
        let l = leg();
        let mut req: PollRequest = serde_json::from_slice(&poll_body("e1")).expect("decodes");
        req.agent.protocol_version = PROTOCOL_VERSION + 1;
        let raw = serde_json::to_vec(&req).expect("json");
        assert!(matches!(
            l.respond(&head("POST", POLL_PATH), &raw).await,
            Answer::Nginx(Class::NotFound)
        ));
        assert!(!l.broker.agent_present());
    }

    #[test]
    fn the_delivery_cap_is_the_shared_one() {
        // Bounded on both sides of Leg B with the SAME constant, so a
        // hostile relay and a hostile agent meet the same ceiling.
        assert_eq!(leg().max_body(), MAX_BODY_BYTES);
    }
}

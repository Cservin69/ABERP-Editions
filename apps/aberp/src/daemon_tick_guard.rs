//! ADR-0098 R4 / Fable-5 finding F — daemon write-tick panic containment.
//!
//! # Why
//!
//! A panic inside a daemon's write-tick runs the [`aberp_db::WriteGuard`]'s
//! `Drop` during unwind, which drops the shared writer `MutexGuard` and POISONS
//! the process-wide [`aberp_db::Handle`] writer mutex. Before the shared Handle
//! (ADR-0098 Gap 1a) a panicking daemon hurt only itself; the shared instance
//! turned one panic into a process-wide write outage. `aberp-db` now RECOVERS a
//! poisoned writer on the next acquire (`clear_poison` + a post-poison integrity
//! re-verify), so the poison is transient rather than terminal — and that
//! recovery is itself AUDITED (`db.auto_recovered`, `trigger=writer_poison_
//! recovered`).
//!
//! [`guard_write_tick`] is the complementary apps-layer half: it wraps the
//! synchronous DB-write body of a daemon tick (the closure handed to
//! `spawn_blocking`) in [`std::panic::catch_unwind`] so the panic is CONTAINED
//! at the tick boundary, sanitized + logged, and surfaced as an ordinary `Err`.
//! The daemon loops already route a step `Err` into "log it, sleep a tick,
//! continue" — so behaviour is preserved (a caught panic skips exactly that one
//! tick; the next tick proceeds), while the shared writer self-heals on the next
//! `write()`.
//!
//! Note this does NOT (and cannot) stop the poisoning itself: the `WriteGuard`'s
//! `Drop` has already run by the time `catch_unwind` observes the panic. The
//! poison→recover→audit path in `aberp-db` is the mechanism that un-bricks the
//! process; this guard's job is containment, a loud log, and preventing the
//! panic from unwinding past the tick.

use std::any::Any;
use std::panic::AssertUnwindSafe;

/// Run a synchronous daemon write-tick body, catching any panic at the tick
/// boundary. On the no-panic path the closure's own `anyhow::Result` is returned
/// verbatim (zero behaviour change). On a caught panic the message is sanitized
/// and logged loudly, and a normal `Err` is returned so the daemon's existing
/// "step failed → next tick" path takes over. The panic is audited out-of-band
/// by the `aberp-db` poison-recovery row emitted on the next `write()`.
pub fn guard_write_tick<R>(
    tick: &str,
    body: impl FnOnce() -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            let msg = panic_payload_to_string(payload);
            tracing::error!(
                tick = %tick,
                panic_msg = %msg,
                "daemon write-tick PANIC caught at tick boundary (ADR-0098 R4 / \
                 finding F); the shared writer self-recovers on its next acquire \
                 (clear_poison + integrity re-verify, audited as db.auto_recovered); \
                 skipping this tick"
            );
            Err(anyhow::anyhow!(
                "daemon write-tick '{tick}' panicked (caught at tick boundary): {msg}"
            ))
        }
    }
}

/// Render a panic payload to a sanitized, bounded string. Mirrors
/// `quote_pricing_pipeline::panic_payload_to_string`: strings/Strings are the
/// two payloads stdlib emits; CR/LF/NUL are stripped so a panic message can't
/// forge an extra log line, and it is truncated so a huge payload can't bloat
/// the log.
fn panic_payload_to_string(payload: Box<dyn Any + Send>) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    raw.chars()
        .filter(|c| !matches!(*c, '\r' | '\n' | '\0'))
        .take(1000)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_panic_passes_result_through() {
        let ok: anyhow::Result<u32> = guard_write_tick("t", || Ok(7));
        assert_eq!(ok.unwrap(), 7);
        let err: anyhow::Result<u32> = guard_write_tick("t", || Err(anyhow::anyhow!("boom")));
        assert!(err.is_err());
    }

    #[test]
    fn panic_is_caught_and_becomes_err() {
        let r: anyhow::Result<u32> = guard_write_tick("claim", || panic!("simulated tick panic"));
        let e = r.unwrap_err().to_string();
        assert!(e.contains("panicked (caught at tick boundary)"));
        assert!(e.contains("simulated tick panic"));
    }

    #[test]
    fn panic_message_is_sanitized() {
        let r: anyhow::Result<()> = guard_write_tick("t", || panic!("line1\nline2\r\0tail"));
        let e = r.unwrap_err().to_string();
        assert!(!e.contains('\n') && !e.contains('\r') && !e.contains('\0'));
    }
}

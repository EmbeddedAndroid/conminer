//! Server-initiated notifications (§8, §13 `notify`).
//!
//! A supervising agent — the one watching a soak test — should learn about a
//! novel template or a stage transition without polling. Two properties make
//! that safe rather than noisy:
//!
//! * **A novel template fires exactly once.** "Novel" means first-ever-seen, and
//!   the store already knows that; re-announcing it on every occurrence would
//!   train an agent to ignore the channel.
//! * **Storms coalesce.** A board in a boot loop produces the same transitions
//!   thousands of times a minute. Within `notify.coalesce_ms` those collapse
//!   into one event carrying a count, so the channel stays readable exactly when
//!   things are going worst.

use crate::protocol::Notification;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const CHANNEL_DEPTH: usize = 256;

#[derive(Clone)]
pub struct Broadcaster {
    tx: broadcast::Sender<Notification>,
    coalesce: Arc<Mutex<Coalescer>>,
}

struct Coalescer {
    window_ms: i64,
    /// key → (window opened at, suppressed count)
    last: HashMap<String, (i64, u64)>,
}

impl Default for Broadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl Broadcaster {
    pub fn new() -> Self {
        Self::with_window(5_000)
    }

    pub fn with_window(window_ms: i64) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_DEPTH);
        Self {
            tx,
            coalesce: Arc::new(Mutex::new(Coalescer {
                window_ms,
                last: HashMap::new(),
            })),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.tx.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Publish, unless an identical event is already inside its coalesce window.
    ///
    /// Returns the number of events suppressed since the last publish for this
    /// key, which the emitted notification carries so nothing is silently lost.
    pub fn publish(&self, key: &str, mut n: Notification, now_ms: i64) -> bool {
        let suppressed = {
            let mut c = self.coalesce.lock().unwrap_or_else(|e| e.into_inner());
            let window = c.window_ms;
            match c.last.get_mut(key) {
                Some((opened, count)) if now_ms - *opened < window => {
                    *count += 1;
                    return false;
                }
                Some((opened, count)) => {
                    let s = *count;
                    *opened = now_ms;
                    *count = 0;
                    s
                }
                None => {
                    c.last.insert(key.to_string(), (now_ms, 0));
                    0
                }
            }
        };
        if suppressed > 0 {
            if let Some(o) = n.params.as_object_mut() {
                o.insert("coalesced".into(), json!(suppressed));
            }
        }
        // An error here means nobody is listening, which is not a failure.
        let _ = self.tx.send(n);
        true
    }

    /// A template seen for the first time ever on this device.
    pub fn novel_template(&self, device: &str, template_id: i64, text: &str, now_ms: i64) -> bool {
        // Keyed by template, so each novel template announces itself once and
        // never again — the coalescer cannot swallow a *different* novel one.
        self.publish(
            &format!("novel:{device}:{template_id}"),
            Notification::resource_updated(
                format!("conminer://device/{device}"),
                json!({
                    "event": "novel_template",
                    "device": device,
                    "template_id": template_id,
                    "text": text,
                }),
            ),
            now_ms,
        )
    }

    pub fn stage_transition(
        &self,
        device: &str,
        stage: &str,
        is_reset: bool,
        boot_id: Option<i64>,
        now_ms: i64,
    ) -> bool {
        self.publish(
            &format!("stage:{device}:{stage}:{is_reset}"),
            Notification::resource_updated(
                format!("conminer://device/{device}"),
                json!({
                    "event": "stage_transition",
                    "device": device,
                    "stage": stage,
                    "is_reset": is_reset,
                    "boot_id": boot_id,
                }),
            ),
            now_ms,
        )
    }

    pub fn console_state(
        &self,
        device: &str,
        state: &str,
        detail: serde_json::Value,
        now_ms: i64,
    ) -> bool {
        self.publish(
            &format!("state:{device}:{state}"),
            Notification::resource_updated(
                format!("conminer://device/{device}"),
                json!({
                    "event": "console_state",
                    "device": device,
                    "state": state,
                    "detail": detail,
                }),
            ),
            now_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_novel_template_fires_exactly_once() {
        let b = Broadcaster::with_window(5_000);
        let mut rx = b.subscribe();
        assert!(b.novel_template("rb3-ap", 7, "Kernel panic", 1_000));
        assert!(!b.novel_template("rb3-ap", 7, "Kernel panic", 1_100));
        assert!(!b.novel_template("rb3-ap", 7, "Kernel panic", 2_000));
        let n = rx.try_recv().unwrap();
        assert_eq!(n.params["detail"]["template_id"], 7);
        assert!(rx.try_recv().is_err(), "only one event");
    }

    #[test]
    fn a_different_novel_template_is_never_swallowed_by_coalescing() {
        let b = Broadcaster::with_window(60_000);
        let mut rx = b.subscribe();
        assert!(b.novel_template("dev", 1, "first", 0));
        assert!(
            b.novel_template("dev", 2, "second", 10),
            "a distinct novel template must still be announced"
        );
        assert_eq!(rx.try_recv().unwrap().params["detail"]["template_id"], 1);
        assert_eq!(rx.try_recv().unwrap().params["detail"]["template_id"], 2);
    }

    #[test]
    fn a_notification_storm_during_a_boot_loop_coalesces_and_reports_the_count() {
        let b = Broadcaster::with_window(5_000);
        let mut rx = b.subscribe();

        assert!(b.stage_transition("dev", "bl1", true, Some(1), 0));
        // 400 more resets inside the window: all suppressed.
        for i in 1..=400 {
            assert!(!b.stage_transition("dev", "bl1", true, Some(i), i * 10));
        }
        // Past the window, the next one publishes and carries what was hidden.
        assert!(b.stage_transition("dev", "bl1", true, Some(999), 10_000));

        let first = rx.try_recv().unwrap();
        assert!(first.params.get("coalesced").is_none());
        let second = rx.try_recv().unwrap();
        assert_eq!(
            second.params["coalesced"], 400,
            "the suppressed count must be reported, never silently dropped"
        );
    }

    #[test]
    fn stage_transitions_of_different_stages_do_not_coalesce_into_each_other() {
        let b = Broadcaster::with_window(60_000);
        let mut rx = b.subscribe();
        assert!(b.stage_transition("dev", "bl1", false, None, 0));
        assert!(b.stage_transition("dev", "uboot", false, None, 1));
        assert!(b.stage_transition("dev", "kernel", false, None, 2));
        for want in ["bl1", "uboot", "kernel"] {
            assert_eq!(rx.try_recv().unwrap().params["detail"]["stage"], want);
        }
    }

    #[test]
    fn a_subscriber_that_reconnects_starts_receiving_again() {
        let b = Broadcaster::with_window(0);
        {
            let mut rx = b.subscribe();
            b.novel_template("dev", 1, "a", 0);
            assert!(rx.try_recv().is_ok());
        }
        assert_eq!(b.subscribers(), 0);
        let mut rx2 = b.subscribe();
        b.novel_template("dev", 2, "b", 1);
        assert_eq!(rx2.try_recv().unwrap().params["detail"]["template_id"], 2);
    }

    #[test]
    fn publishing_with_no_subscribers_is_not_an_error() {
        let b = Broadcaster::new();
        assert_eq!(b.subscribers(), 0);
        assert!(b.novel_template("dev", 1, "a", 0));
    }

    #[test]
    fn notifications_are_addressed_to_the_device_resource() {
        let b = Broadcaster::new();
        let mut rx = b.subscribe();
        b.novel_template("rb3-ap", 1, "x", 0);
        let n = rx.try_recv().unwrap();
        assert_eq!(n.method, "notifications/resources/updated");
        assert_eq!(n.params["uri"], "conminer://device/rb3-ap");
    }
}

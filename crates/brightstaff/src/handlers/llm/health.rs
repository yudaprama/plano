use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;

/// Default cooldown applied to a backend after a 429/5xx when the upstream
/// doesn't supply a `Retry-After` header.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);
/// Upper bound on any cooldown, so a hostile/large `Retry-After` can't park a
/// backend for an unreasonable amount of time.
const MAX_COOLDOWN: Duration = Duration::from_secs(300);

/// Tracks which backend models are temporarily "unhealthy" (recently returned
/// 429 or 5xx) so alias resolution can steer new requests away from them until
/// a cooldown elapses. In-memory and process-local; no persistence, no
/// cross-replica sharing.
pub struct ModelHealthTracker {
    /// model id (`provider/model`) -> instant at which the cooldown expires.
    cooldowns: RwLock<HashMap<String, Instant>>,
    default_cooldown: Duration,
    max_cooldown: Duration,
}

impl Default for ModelHealthTracker {
    fn default() -> Self {
        Self::new(DEFAULT_COOLDOWN)
    }
}

impl ModelHealthTracker {
    pub fn new(default_cooldown: Duration) -> Self {
        Self {
            cooldowns: RwLock::new(HashMap::new()),
            default_cooldown,
            max_cooldown: MAX_COOLDOWN,
        }
    }

    /// True if the model is not currently in cooldown.
    pub fn is_available(&self, model: &str) -> bool {
        let guard = self.cooldowns.read().unwrap();
        match guard.get(model) {
            Some(until) => *until <= Instant::now(),
            None => true,
        }
    }

    /// Put a model into cooldown after a 429/5xx. `retry_after` (parsed from the
    /// upstream `Retry-After` header) overrides the default when present, capped
    /// at `max_cooldown`.
    pub fn mark_unhealthy(&self, model: &str, retry_after: Option<Duration>) {
        let cd = retry_after
            .unwrap_or(self.default_cooldown)
            .min(self.max_cooldown);
        let until = Instant::now() + cd;
        self.cooldowns
            .write()
            .unwrap()
            .insert(model.to_string(), until);
    }

    /// Clear a model's cooldown (e.g. after a successful response).
    pub fn mark_healthy(&self, model: &str) {
        self.cooldowns.write().unwrap().remove(model);
    }

    /// Order candidates for an attempt: currently-available ones first (shuffled
    /// for load spreading), then cooled-down ones (shuffled) as a last resort so
    /// the request never hard-fails just because every backend is cooling down.
    pub fn order_candidates(&self, candidates: &[String]) -> Vec<String> {
        let now = Instant::now();
        let (mut available, mut cooled): (Vec<String>, Vec<String>) = {
            let guard = self.cooldowns.read().unwrap();
            candidates
                .iter()
                .cloned()
                .partition(|m| match guard.get(m) {
                    Some(until) => *until <= now,
                    None => true,
                })
        };
        let mut rng = rand::rng();
        available.shuffle(&mut rng);
        cooled.shuffle(&mut rng);
        available.extend(cooled);
        available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_by_default() {
        let t = ModelHealthTracker::default();
        assert!(t.is_available("a/b"));
    }

    #[test]
    fn mark_unhealthy_then_recovers() {
        let t = ModelHealthTracker::new(Duration::from_millis(20));
        t.mark_unhealthy("a/b", None);
        assert!(!t.is_available("a/b"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(t.is_available("a/b"));
    }

    #[test]
    fn mark_healthy_clears_cooldown() {
        let t = ModelHealthTracker::default();
        t.mark_unhealthy("a/b", None);
        assert!(!t.is_available("a/b"));
        t.mark_healthy("a/b");
        assert!(t.is_available("a/b"));
    }

    #[test]
    fn order_puts_available_before_cooled() {
        let t = ModelHealthTracker::new(Duration::from_secs(30));
        let all = vec!["p/a".to_string(), "p/b".to_string(), "p/c".to_string()];
        t.mark_unhealthy("p/b", None);
        let ordered = t.order_candidates(&all);
        assert_eq!(ordered.len(), 3);
        // p/b (cooled) must be last.
        assert_eq!(ordered.last().unwrap(), "p/b");
        assert!(ordered[..2].contains(&"p/a".to_string()));
        assert!(ordered[..2].contains(&"p/c".to_string()));
    }

    #[test]
    fn retry_after_is_capped() {
        let t = ModelHealthTracker::new(Duration::from_secs(30));
        // A huge retry-after should be capped, but still marks unhealthy now.
        t.mark_unhealthy("a/b", Some(Duration::from_secs(10_000)));
        assert!(!t.is_available("a/b"));
    }
}

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Sliding-window limiter keyed by client address.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        RateLimiter { max, window, hits: Mutex::new(HashMap::new()) }
    }

    pub fn allow(&self, key: &str, now: Instant) -> bool {
        let mut map = self.hits.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < self.window));
        let q = map.entry(key.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= self.window) {
            q.pop_front();
        }
        if q.len() >= self.max {
            return false;
        }
        q.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_key_and_recovers() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(rl.allow("a", t0) && rl.allow("a", t0));
        assert!(!rl.allow("a", t0));
        assert!(rl.allow("b", t0));
        assert!(rl.allow("a", t0 + Duration::from_secs(61)));
    }
}

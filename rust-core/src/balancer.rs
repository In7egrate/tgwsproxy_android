use once_cell::sync::Lazy;
use parking_lot::RwLock;
use rand::{seq::SliceRandom, Rng};
use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

// Port of proxy/balancer.py::_Balancer plus a measured, per-DC route scorer.
// Metrics are process-local and explicitly reset on Android network handover;
// stale observations from the previous network must not bias route selection.
pub struct Balancer {
    domains: Vec<String>,
    dc_to_domain: HashMap<i32, String>,
    route_metrics: HashMap<(i32, String, String), RouteMetric>,
}

#[derive(Clone, Debug)]
struct RouteMetric {
    latency_ewma_ms: f64,
    success_ewma: f64,
    samples: u64,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
    circuit_open_until: Option<Instant>,
    half_open_probe_until: Option<Instant>,
    pending_validation_until: Option<Instant>,
    last_updated: Instant,
}

impl Default for RouteMetric {
    fn default() -> Self {
        Self {
            latency_ewma_ms: 1_000.0,
            success_ewma: 0.5,
            samples: 0,
            consecutive_failures: 0,
            cooldown_until: None,
            circuit_open_until: None,
            half_open_probe_until: None,
            pending_validation_until: None,
            last_updated: Instant::now(),
        }
    }
}

const EWMA_ALPHA: f64 = 0.2;
const UNKNOWN_ROUTE_SCORE: f64 = 1_000.0;
const FAILURE_PENALTY: f64 = 4.0;
const MAX_FAILURE_COOLDOWN: Duration = Duration::from_secs(120);
const CIRCUIT_BASE_COOLDOWN: Duration = Duration::from_secs(5);
const HALF_OPEN_PROBE_LEASE: Duration = Duration::from_secs(20);
const ROUTE_VALIDATION_LEASE: Duration = Duration::from_secs(15);
const MAX_ROUTE_METRICS: usize = 512;

pub static BALANCER: Lazy<RwLock<Balancer>> = Lazy::new(|| RwLock::new(Balancer::new()));

impl Balancer {
    pub fn new() -> Self {
        Self {
            domains: Vec::new(),
            dc_to_domain: HashMap::new(),
            route_metrics: HashMap::new(),
        }
    }

    pub fn update_domains_list(&mut self, domains_list: &[String]) {
        let mut current_sorted = self.domains.clone();
        current_sorted.sort();
        let mut new_sorted = domains_list.to_vec();
        new_sorted.sort();

        if current_sorted == new_sorted || domains_list.is_empty() {
            return;
        }

        self.domains = domains_list.to_vec();
        let mut rng = rand::thread_rng();
        self.dc_to_domain.clear();
        for dc_id in [1, 2, 3, 4, 5, 203] {
            if let Some(domain) = self.domains.choose(&mut rng) {
                self.dc_to_domain.insert(dc_id, domain.clone());
            }
        }
    }

    pub fn update_domain_for_dc(&mut self, dc_id: i32, domain: &str) -> bool {
        if self.dc_to_domain.get(&dc_id).map(|s| s.as_str()) == Some(domain) {
            return false;
        }
        self.dc_to_domain.insert(dc_id, domain.to_string());
        true
    }

    pub fn get_domains_for_dc(&self, dc_id: i32) -> Vec<String> {
        let mut result = Vec::new();
        let current_domain = self.dc_to_domain.get(&dc_id).cloned();

        if let Some(ref domain) = current_domain {
            result.push(domain.clone());
        }

        let mut shuffled = self.domains.clone();
        let mut rng = rand::thread_rng();
        shuffled.shuffle(&mut rng);

        for domain in shuffled {
            if Some(&domain) != current_domain.as_ref() {
                result.push(domain);
            }
        }

        result
    }

    fn metric_key(dc_id: i32, route_kind: &str, route: &str) -> (i32, String, String) {
        (
            dc_id,
            route_kind.to_ascii_lowercase(),
            route.trim().to_ascii_lowercase(),
        )
    }

    fn metric_mut(&mut self, key: (i32, String, String)) -> &mut RouteMetric {
        if !self.route_metrics.contains_key(&key)
            && self.route_metrics.len() >= MAX_ROUTE_METRICS
        {
            let oldest = self
                .route_metrics
                .iter()
                .min_by_key(|(_, metric)| metric.last_updated)
                .map(|(oldest_key, _)| oldest_key.clone());
            if let Some(oldest_key) = oldest {
                self.route_metrics.remove(&oldest_key);
            }
        }
        self.route_metrics.entry(key).or_default()
    }

    fn circuit_failure_threshold(route_kind: &str) -> u32 {
        match route_kind.to_ascii_lowercase().as_str() {
            "telegram_ws" | "tcp_fallback" => 2,
            "cf_proxy" | "cf_worker" => 3,
            _ => 3,
        }
    }

    /// Returns true when a connection attempt may start. An expired open
    /// circuit grants exactly one half-open probe; concurrent callers are
    /// rejected until that probe reports success/failure or its lease expires.
    pub fn allow_attempt(&mut self, dc_id: i32, route_kind: &str, route: &str) -> bool {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        let now = Instant::now();
        if metric.pending_validation_until.map(|until| until > now).unwrap_or(false) {
            return false;
        }

        match metric.circuit_open_until {
            Some(until) if until > now => false,
            Some(_) => {
                if metric.half_open_probe_until.map(|until| until > now).unwrap_or(false) {
                    false
                } else {
                    metric.half_open_probe_until = Some(now + HALF_OPEN_PROBE_LEASE);
                    true
                }
            }
            None => true,
        }
    }

    /// Mark a completed transport handshake as awaiting its first real upstream
    /// payload. While this lease is active, duplicate handshakes for the same
    /// route are suppressed. A response, failure, or lease expiry resolves it.
    pub fn mark_pending_validation(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.pending_validation_until = Some(Instant::now() + ROUTE_VALIDATION_LEASE);
        metric.last_updated = Instant::now();
    }

    pub fn release_validation(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        let key = Self::metric_key(dc_id, route_kind, route);
        if let Some(metric) = self.route_metrics.get_mut(&key) {
            metric.pending_validation_until = None;
            metric.last_updated = Instant::now();
        }
    }

    /// Release a half-open probe when the attempt is canceled before a
    /// transport result is available. The lease is also bounded as a safeguard.
    pub fn release_attempt(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        let key = Self::metric_key(dc_id, route_kind, route);
        if let Some(metric) = self.route_metrics.get_mut(&key) {
            metric.half_open_probe_until = None;
            metric.pending_validation_until = None;
            metric.last_updated = Instant::now();
        }
    }

    pub fn record_success(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        elapsed: Duration,
    ) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.last_updated = Instant::now();
        let sample_ms = (elapsed.as_secs_f64() * 1_000.0).clamp(1.0, 30_000.0);

        if metric.samples == 0 {
            metric.latency_ewma_ms = sample_ms;
            metric.success_ewma = 1.0;
        } else {
            metric.latency_ewma_ms =
                EWMA_ALPHA * sample_ms + (1.0 - EWMA_ALPHA) * metric.latency_ewma_ms;
            metric.success_ewma =
                EWMA_ALPHA + (1.0 - EWMA_ALPHA) * metric.success_ewma;
        }
        metric.samples = metric.samples.saturating_add(1);
        metric.consecutive_failures = 0;
        metric.cooldown_until = None;
        metric.circuit_open_until = None;
        metric.half_open_probe_until = None;
        metric.pending_validation_until = None;
    }

    pub fn record_failure(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.last_updated = Instant::now();

        if metric.samples == 0 {
            metric.success_ewma = 0.0;
        } else {
            metric.success_ewma *= 1.0 - EWMA_ALPHA;
        }
        metric.samples = metric.samples.saturating_add(1);
        metric.consecutive_failures = metric.consecutive_failures.saturating_add(1);

        let now = Instant::now();
        metric.half_open_probe_until = None;
        metric.pending_validation_until = None;
        let threshold = Self::circuit_failure_threshold(route_kind);
        if metric.consecutive_failures >= threshold {
            let shift = metric.consecutive_failures.saturating_sub(threshold).min(5);
            let base = CIRCUIT_BASE_COOLDOWN
                .as_millis()
                .saturating_mul(1u128.checked_shl(shift).unwrap_or(32) as u128)
                .min(MAX_FAILURE_COOLDOWN.as_millis()) as u64;
            // +/-20% jitter prevents a fleet of clients on the same mobile
            // carrier from reopening a failed route in lockstep.
            let jitter_percent = rand::thread_rng().gen_range(80u64..=120u64);
            let delay_ms = base.saturating_mul(jitter_percent) / 100;
            let until = now + Duration::from_millis(delay_ms);
            metric.circuit_open_until = Some(until);
            metric.cooldown_until = Some(until);
        } else {
            let shift = metric.consecutive_failures.saturating_sub(1).min(1);
            metric.cooldown_until = Some(now + Duration::from_secs(1u64 << shift));
        }
    }

    pub fn reset_route_metrics(&mut self) {
        self.route_metrics.clear();
        self.dc_to_domain.clear();
    }

    pub fn route_score(&self, dc_id: i32, route_kind: &str, route: &str) -> f64 {
        let key = Self::metric_key(dc_id, route_kind, route);
        let Some(metric) = self.route_metrics.get(&key) else {
            return UNKNOWN_ROUTE_SCORE;
        };

        let reliability_penalty = 1.0 + FAILURE_PENALTY * (1.0 - metric.success_ewma);
        let now = Instant::now();
        let cooldown_penalty = metric
            .cooldown_until
            .filter(|until| *until > now)
            .map(|_| 60_000.0)
            .unwrap_or(0.0);
        let circuit_penalty = metric
            .circuit_open_until
            .filter(|until| *until > now)
            .map(|_| 120_000.0)
            .unwrap_or(0.0);

        metric.latency_ewma_ms * reliability_penalty + cooldown_penalty + circuit_penalty
    }

    /// Stable sort: routes with equal/unseen scores retain their caller-provided
    /// order, so Telegram's documented hostname preference is preserved until
    /// enough measurements exist to justify a different order.
    pub fn rank_candidates(
        &self,
        dc_id: i32,
        route_kind: &str,
        candidates: &[String],
    ) -> Vec<String> {
        let mut ranked: Vec<(usize, f64, String)> = candidates
            .iter()
            .enumerate()
            .map(|(index, route)| {
                (
                    index,
                    self.route_score(dc_id, route_kind, route),
                    route.clone(),
                )
            })
            .collect();

        ranked.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(CmpOrdering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.into_iter().map(|(_, _, route)| route).collect()
    }

    /// Diagnostic snapshot intentionally exposes a stable route hash rather
    /// than custom Worker/CF hostnames supplied by the user.
    pub fn metrics_snapshot(&self) -> Vec<serde_json::Value> {
        let mut rows: Vec<serde_json::Value> = self
            .route_metrics
            .iter()
            .map(|((dc, kind, route), metric)| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                route.hash(&mut hasher);
                let route_id = format!("{:016x}", hasher.finish());
                let now = Instant::now();
                let cooldown_remaining_ms = metric
                    .cooldown_until
                    .filter(|until| *until > now)
                    .map(|until| until.saturating_duration_since(now).as_millis() as u64)
                    .unwrap_or(0);
                let open_remaining_ms = metric
                    .circuit_open_until
                    .filter(|until| *until > now)
                    .map(|until| until.saturating_duration_since(now).as_millis() as u64)
                    .unwrap_or(0);
                let probe_in_flight = metric.half_open_probe_until
                    .map(|until| until > now)
                    .unwrap_or(false);
                let validation_pending = metric.pending_validation_until
                    .map(|until| until > now)
                    .unwrap_or(false);
                let circuit_state = if open_remaining_ms > 0 {
                    "open"
                } else if probe_in_flight || metric.circuit_open_until.is_some() {
                    "half_open"
                } else if validation_pending {
                    "validating"
                } else {
                    "closed"
                };
                serde_json::json!({
                    "dc": dc,
                    "kind": kind,
                    "route_id": route_id,
                    "samples": metric.samples,
                    "latency_ewma_ms": metric.latency_ewma_ms.round() as u64,
                    "success_rate_permille": (metric.success_ewma * 1000.0).round() as u16,
                    "cooldown_remaining_ms": cooldown_remaining_ms,
                    "circuit_state": circuit_state,
                    "consecutive_failures": metric.consecutive_failures,
                    "open_remaining_ms": open_remaining_ms,
                    "half_open_probe_in_flight": probe_in_flight,
                    "validation_pending": validation_pending,
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            let a_key = (
                a["dc"].as_i64().unwrap_or_default(),
                a["kind"].as_str().unwrap_or_default().to_string(),
                a["route_id"].as_str().unwrap_or_default().to_string(),
            );
            let b_key = (
                b["dc"].as_i64().unwrap_or_default(),
                b["kind"].as_str().unwrap_or_default().to_string(),
                b["route_id"].as_str().unwrap_or_default().to_string(),
            );
            a_key.cmp(&b_key)
        });
        rows
    }
}

#[cfg(test)]
mod adaptive_route_tests {
    use super::*;

    #[test]
    fn measured_fast_route_beats_unknown_route() {
        let mut balancer = Balancer::new();
        balancer.record_success(2, "telegram_ws", "kws2.web.telegram.org", Duration::from_millis(75));
        let candidates = vec![
            "kws2-1.web.telegram.org".to_string(),
            "kws2.web.telegram.org".to_string(),
        ];
        let ranked = balancer.rank_candidates(2, "telegram_ws", &candidates);
        assert_eq!(ranked[0], "kws2.web.telegram.org");
    }

    #[test]
    fn repeated_failures_put_route_into_cooldown() {
        let mut balancer = Balancer::new();
        balancer.record_failure(4, "cf_proxy", "worker.example");
        let score = balancer.route_score(4, "cf_proxy", "worker.example");
        assert!(score > 60_000.0);
    }

    #[test]
    fn metrics_are_isolated_by_dc_and_route_kind() {
        let mut balancer = Balancer::new();
        balancer.record_success(2, "telegram_ws", "kws2.web.telegram.org", Duration::from_millis(80));
        let score_same = balancer.route_score(2, "telegram_ws", "kws2.web.telegram.org");
        let score_other_dc = balancer.route_score(4, "telegram_ws", "kws2.web.telegram.org");
        let score_other_kind = balancer.route_score(2, "cf_proxy", "kws2.web.telegram.org");
        assert!(score_same < score_other_dc);
        assert!(score_same < score_other_kind);
    }

    #[test]
    fn diagnostics_snapshot_does_not_expose_custom_route_hostname() {
        let mut balancer = Balancer::new();
        balancer.record_success(
            2,
            "cf_worker",
            "private-worker.example",
            Duration::from_millis(120),
        );
        let snapshot = serde_json::to_string(&balancer.metrics_snapshot()).unwrap();
        assert!(!snapshot.contains("private-worker.example"));
        assert!(snapshot.contains("route_id"));
    }

    #[test]
    fn route_metrics_memory_is_bounded() {
        let mut balancer = Balancer::new();
        for index in 0..(MAX_ROUTE_METRICS + 64) {
            balancer.record_failure(2, "cf_worker", &format!("worker-{index}.example"));
        }
        assert_eq!(balancer.route_metrics.len(), MAX_ROUTE_METRICS);
    }

    #[test]
    fn reset_clears_network_specific_measurements() {
        let mut balancer = Balancer::new();
        balancer.record_success(2, "telegram_ws", "kws2.web.telegram.org", Duration::from_millis(75));
        assert!(balancer.route_score(2, "telegram_ws", "kws2.web.telegram.org") < UNKNOWN_ROUTE_SCORE);
        balancer.reset_route_metrics();
        assert_eq!(
            balancer.route_score(2, "telegram_ws", "kws2.web.telegram.org"),
            UNKNOWN_ROUTE_SCORE
        );
    }

    #[test]
    fn direct_route_opens_after_repeated_failures_and_skips_attempts() {
        let mut balancer = Balancer::new();
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        assert!(!balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
    }

    #[test]
    fn half_open_allows_only_one_probe() {
        let mut balancer = Balancer::new();
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        let key = Balancer::metric_key(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.route_metrics.get_mut(&key).unwrap().circuit_open_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        assert!(!balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
    }

    #[test]
    fn successful_half_open_probe_closes_circuit() {
        let mut balancer = Balancer::new();
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        let key = Balancer::metric_key(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.route_metrics.get_mut(&key).unwrap().circuit_open_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        balancer.record_success(2, "telegram_ws", "kws2.web.telegram.org", Duration::from_millis(90));
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        assert_eq!(balancer.route_metrics.get(&key).unwrap().consecutive_failures, 0);
    }

    #[test]
    fn pending_first_byte_validation_suppresses_duplicate_handshakes() {
        let mut balancer = Balancer::new();
        balancer.mark_pending_validation(2, "telegram_ws", "kws2.web.telegram.org");
        assert!(!balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        balancer.record_success(2, "telegram_ws", "kws2.web.telegram.org", Duration::from_millis(110));
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
    }

    #[test]
    fn canceled_half_open_probe_releases_its_lease() {
        let mut balancer = Balancer::new();
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        let key = Balancer::metric_key(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.route_metrics.get_mut(&key).unwrap().circuit_open_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
        balancer.release_attempt(2, "telegram_ws", "kws2.web.telegram.org");
        assert!(balancer.allow_attempt(2, "telegram_ws", "kws2.web.telegram.org"));
    }

    #[test]
    fn stable_order_is_preserved_when_routes_are_unmeasured() {
        let balancer = Balancer::new();
        let candidates = vec!["first.example".to_string(), "second.example".to_string()];
        assert_eq!(
            balancer.rank_candidates(2, "cf_proxy", &candidates),
            candidates
        );
    }
}

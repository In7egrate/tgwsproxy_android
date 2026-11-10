use once_cell::sync::Lazy;
use parking_lot::RwLock;
use rand::seq::SliceRandom;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteAttemptKind {
    Closed,
    HalfOpen,
}

#[derive(Clone, Debug)]
pub struct RouteObservation {
    pub dc_id: i32,
    pub route_kind: String,
    pub route: String,
    pub attempt_kind: RouteAttemptKind,
    pub connected_at: Instant,
}

#[derive(Clone, Debug)]
struct RouteMetric {
    connect_latency_ewma_ms: f64,
    first_byte_latency_ewma_ms: f64,
    connect_samples: u64,
    first_byte_samples: u64,
    success_ewma: f64,
    samples: u64,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
    half_open_probe_in_flight: bool,
    in_flight_dials: u32,
    last_updated: Instant,
}

impl Default for RouteMetric {
    fn default() -> Self {
        Self {
            connect_latency_ewma_ms: 1_000.0,
            first_byte_latency_ewma_ms: 1_000.0,
            connect_samples: 0,
            first_byte_samples: 0,
            success_ewma: 0.5,
            samples: 0,
            consecutive_failures: 0,
            cooldown_until: None,
            half_open_probe_in_flight: false,
            in_flight_dials: 0,
            last_updated: Instant::now(),
        }
    }
}

const EWMA_ALPHA: f64 = 0.2;
const UNKNOWN_ROUTE_SCORE: f64 = 1_000.0;
const FAILURE_PENALTY: f64 = 4.0;
const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const CIRCUIT_BASE_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_FAILURE_COOLDOWN: Duration = Duration::from_secs(120);
const MAX_PARALLEL_ROUTE_DIALS: u32 = 1;
const OPEN_CIRCUIT_SCORE: f64 = 1_000_000_000.0;
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

    /// Reserve one dial for this (DC, transport, route). Closed routes are
    /// serialized per route; an expired Open circuit admits exactly one
    /// Half-Open recovery probe.
    pub fn begin_attempt(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
    ) -> Option<RouteAttemptKind> {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        let now = Instant::now();
        metric.last_updated = now;

        if let Some(until) = metric.cooldown_until {
            if until > now || metric.half_open_probe_in_flight {
                return None;
            }
            metric.half_open_probe_in_flight = true;
            return Some(RouteAttemptKind::HalfOpen);
        }

        if metric.half_open_probe_in_flight
            || metric.in_flight_dials >= MAX_PARALLEL_ROUTE_DIALS
        {
            return None;
        }
        metric.in_flight_dials += 1;
        Some(RouteAttemptKind::Closed)
    }

    /// Finish the dial phase. A Half-Open probe remains reserved until the
    /// first upstream response or a classified early failure/neutral close.
    pub fn record_connect_success(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        elapsed: Duration,
        attempt: RouteAttemptKind,
    ) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.last_updated = Instant::now();
        if attempt == RouteAttemptKind::Closed {
            metric.in_flight_dials = metric.in_flight_dials.saturating_sub(1);
        }
        let sample_ms = (elapsed.as_secs_f64() * 1_000.0).clamp(1.0, 30_000.0);
        metric.connect_latency_ewma_ms = if metric.connect_samples == 0 {
            sample_ms
        } else {
            EWMA_ALPHA * sample_ms
                + (1.0 - EWMA_ALPHA) * metric.connect_latency_ewma_ms
        };
        metric.connect_samples = metric.connect_samples.saturating_add(1);
    }

    /// Record handshake/connect timing for a prewarmed socket without
    /// treating the handshake as end-to-end route success.
    pub fn record_connect_sample(
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
        metric.connect_latency_ewma_ms = if metric.connect_samples == 0 {
            sample_ms
        } else {
            EWMA_ALPHA * sample_ms
                + (1.0 - EWMA_ALPHA) * metric.connect_latency_ewma_ms
        };
        metric.connect_samples = metric.connect_samples.saturating_add(1);
    }

    /// Record a successful end-to-end probe only after the first upstream
    /// application frame reaches the client, not after the WS handshake.
    pub fn record_first_byte_success(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        elapsed: Duration,
    ) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.last_updated = Instant::now();
        let sample_ms = (elapsed.as_secs_f64() * 1_000.0).clamp(1.0, 120_000.0);
        metric.first_byte_latency_ewma_ms = if metric.first_byte_samples == 0 {
            sample_ms
        } else {
            EWMA_ALPHA * sample_ms
                + (1.0 - EWMA_ALPHA) * metric.first_byte_latency_ewma_ms
        };
        metric.first_byte_samples = metric.first_byte_samples.saturating_add(1);
        if metric.samples == 0 {
            metric.success_ewma = 1.0;
        } else {
            metric.success_ewma =
                EWMA_ALPHA + (1.0 - EWMA_ALPHA) * metric.success_ewma;
        }
        metric.samples = metric.samples.saturating_add(1);
        metric.consecutive_failures = 0;
        metric.cooldown_until = None;
        metric.half_open_probe_in_flight = false;
    }

    /// Compatibility wrapper for existing callers; route success now means
    /// a first upstream application byte, not a successful handshake.
    pub fn record_success(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        elapsed: Duration,
    ) {
        self.record_first_byte_success(dc_id, route_kind, route, elapsed);
    }

    pub fn record_attempt_failure(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        attempt: RouteAttemptKind,
    ) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        if attempt == RouteAttemptKind::Closed {
            metric.in_flight_dials = metric.in_flight_dials.saturating_sub(1);
        }
        self.record_failure(dc_id, route_kind, route);
    }

    /// Release a Half-Open probe when the client closed normally before
    /// sending a request or the upstream had no opportunity to answer.
    pub fn release_probe(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        attempt: RouteAttemptKind,
    ) {
        if attempt != RouteAttemptKind::HalfOpen {
            return;
        }
        let key = Self::metric_key(dc_id, route_kind, route);
        if let Some(metric) = self.route_metrics.get_mut(&key) {
            metric.half_open_probe_in_flight = false;
            metric.last_updated = Instant::now();
        }
    }

    pub fn cancel_attempt(
        &mut self,
        dc_id: i32,
        route_kind: &str,
        route: &str,
        attempt: RouteAttemptKind,
    ) {
        let key = Self::metric_key(dc_id, route_kind, route);
        if let Some(metric) = self.route_metrics.get_mut(&key) {
            match attempt {
                RouteAttemptKind::Closed => {
                    metric.in_flight_dials = metric.in_flight_dials.saturating_sub(1);
                }
                RouteAttemptKind::HalfOpen => {
                    metric.half_open_probe_in_flight = false;
                }
            }
            metric.last_updated = Instant::now();
        }
    }

    /// A connection that passed the handshake but failed before its first
    /// upstream application response is still a route failure.
    pub fn record_early_failure(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        self.record_failure(dc_id, route_kind, route);
    }

    pub fn record_failure(&mut self, dc_id: i32, route_kind: &str, route: &str) {
        let key = Self::metric_key(dc_id, route_kind, route);
        let metric = self.metric_mut(key);
        metric.last_updated = Instant::now();
        let was_half_open = metric.half_open_probe_in_flight;
        metric.half_open_probe_in_flight = false;

        if metric.samples == 0 {
            metric.success_ewma = 0.0;
        } else {
            metric.success_ewma *= 1.0 - EWMA_ALPHA;
        }
        metric.samples = metric.samples.saturating_add(1);
        metric.consecutive_failures = metric.consecutive_failures.saturating_add(1);

        if was_half_open || metric.consecutive_failures >= CIRCUIT_FAILURE_THRESHOLD {
            let shift = metric
                .consecutive_failures
                .saturating_sub(CIRCUIT_FAILURE_THRESHOLD)
                .min(5);
            let multiplier = 1u64.checked_shl(shift).unwrap_or(32);
            let seconds = CIRCUIT_BASE_COOLDOWN
                .as_secs()
                .saturating_mul(multiplier)
                .min(MAX_FAILURE_COOLDOWN.as_secs());
            metric.cooldown_until = Some(Instant::now() + Duration::from_secs(seconds));
        }
    }

    pub fn reset_route_metrics(&mut self) {
        self.route_metrics.clear();
        self.dc_to_domain.clear();
    }

    fn route_is_available(&self, dc_id: i32, route_kind: &str, route: &str) -> bool {
        let key = Self::metric_key(dc_id, route_kind, route);
        let Some(metric) = self.route_metrics.get(&key) else {
            return true;
        };
        let now = Instant::now();
        if metric.cooldown_until.map(|until| until > now).unwrap_or(false) {
            return false;
        }
        if metric.half_open_probe_in_flight {
            return false;
        }
        metric.in_flight_dials < MAX_PARALLEL_ROUTE_DIALS
    }

    pub fn route_score(&self, dc_id: i32, route_kind: &str, route: &str) -> f64 {
        let key = Self::metric_key(dc_id, route_kind, route);
        let Some(metric) = self.route_metrics.get(&key) else {
            return UNKNOWN_ROUTE_SCORE;
        };
        let now = Instant::now();
        if metric.cooldown_until.map(|until| until > now).unwrap_or(false)
            || metric.half_open_probe_in_flight
        {
            return OPEN_CIRCUIT_SCORE;
        }

        let latency_score = if metric.first_byte_samples > 0 {
            metric.first_byte_latency_ewma_ms
        } else if metric.connect_samples > 0 {
            // A fast handshake without any successful application response
            // is not a healthy route. Keep it behind routes with real probes.
            UNKNOWN_ROUTE_SCORE + metric.connect_latency_ewma_ms
        } else {
            UNKNOWN_ROUTE_SCORE
        };
        let reliability_penalty = 1.0 + FAILURE_PENALTY * (1.0 - metric.success_ewma);
        latency_score * reliability_penalty
    }

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
                let circuit_state = if cooldown_remaining_ms > 0 {
                    "open"
                } else if metric.half_open_probe_in_flight {
                    "half_open"
                } else {
                    "closed"
                };
                let latency_source = if metric.first_byte_samples > 0 {
                    "first_byte"
                } else if metric.connect_samples > 0 {
                    "connect_only"
                } else {
                    "unknown"
                };
                serde_json::json!({
                    "dc": dc,
                    "kind": kind,
                    "route_id": route_id,
                    "samples": metric.samples,
                    "connect_samples": metric.connect_samples,
                    "first_byte_samples": metric.first_byte_samples,
                    "connect_latency_ewma_ms": metric.connect_latency_ewma_ms.round() as u64,
                    "first_byte_latency_ewma_ms": metric.first_byte_latency_ewma_ms.round() as u64,
                    "latency_ewma_ms": if metric.first_byte_samples > 0 {
                        metric.first_byte_latency_ewma_ms.round() as u64
                    } else {
                        metric.connect_latency_ewma_ms.round() as u64
                    },
                    "latency_source": latency_source,
                    "success_rate_permille": (metric.success_ewma * 1000.0).round() as u16,
                    "circuit_state": circuit_state,
                    "cooldown_remaining_ms": cooldown_remaining_ms,
                    "in_flight_dials": metric.in_flight_dials,
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
            .filter(|(_, route)| self.route_is_available(dc_id, route_kind, route))
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
    fn handshake_only_measurements_do_not_beat_unmeasured_routes() {
        let mut balancer = Balancer::new();
        let attempt = balancer
            .begin_attempt(2, "telegram_ws", "kws2.web.telegram.org")
            .unwrap();
        balancer.record_connect_success(
            2,
            "telegram_ws",
            "kws2.web.telegram.org",
            Duration::from_millis(40),
            attempt,
        );
        let candidates = vec![
            "kws2.web.telegram.org".to_string(),
            "kws2-1.web.telegram.org".to_string(),
        ];
        let ranked = balancer.rank_candidates(2, "telegram_ws", &candidates);
        assert_eq!(ranked[0], "kws2-1.web.telegram.org");
        let snapshot = serde_json::to_string(&balancer.metrics_snapshot()).unwrap();
        assert!(snapshot.contains("connect_only"));
        assert!(snapshot.contains("\"first_byte_samples\":0"));
    }

    #[test]
    fn repeated_failures_open_the_circuit_and_remove_the_route_from_candidates() {
        let mut balancer = Balancer::new();
        for _ in 0..CIRCUIT_FAILURE_THRESHOLD {
            balancer.record_failure(4, "cf_proxy", "worker.example");
        }
        let score = balancer.route_score(4, "cf_proxy", "worker.example");
        assert!(score >= OPEN_CIRCUIT_SCORE);
        assert!(balancer
            .rank_candidates(4, "cf_proxy", &["worker.example".to_string()])
            .is_empty());
    }

    #[test]
    fn half_open_allows_exactly_one_recovery_probe() {
        let mut balancer = Balancer::new();
        for _ in 0..CIRCUIT_FAILURE_THRESHOLD {
            balancer.record_failure(2, "telegram_ws", "kws2.web.telegram.org");
        }
        let key = Balancer::metric_key(2, "telegram_ws", "kws2.web.telegram.org");
        balancer.route_metrics.get_mut(&key).unwrap().cooldown_until =
            Some(Instant::now() - Duration::from_millis(1));

        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            Some(RouteAttemptKind::HalfOpen)
        );
        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            None
        );
        balancer.record_first_byte_success(
            2,
            "telegram_ws",
            "kws2.web.telegram.org",
            Duration::from_millis(180),
        );
        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            Some(RouteAttemptKind::Closed)
        );
    }

    #[test]
    fn closed_route_dials_are_serialized() {
        let mut balancer = Balancer::new();
        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            Some(RouteAttemptKind::Closed)
        );
        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            None
        );
        balancer.record_attempt_failure(
            2,
            "telegram_ws",
            "kws2.web.telegram.org",
            RouteAttemptKind::Closed,
        );
        assert_eq!(
            balancer.begin_attempt(2, "telegram_ws", "kws2.web.telegram.org"),
            Some(RouteAttemptKind::Closed)
        );
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
    fn stable_order_is_preserved_when_routes_are_unmeasured() {
        let balancer = Balancer::new();
        let candidates = vec!["first.example".to_string(), "second.example".to_string()];
        assert_eq!(
            balancer.rank_candidates(2, "cf_proxy", &candidates),
            candidates
        );
    }
}

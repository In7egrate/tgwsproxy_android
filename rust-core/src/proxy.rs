use crate::cfproxy::*;
use crate::config::*;
use crate::crypto::*;
use crate::faketls;
use crate::ws::*;
use crate::balancer::{RouteAttemptKind, RouteObservation};
use crate::{ldebug, linfo, lwarn};
use once_cell::sync::Lazy;
use byteorder::{ByteOrder, LittleEndian};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fronting flag — proxy/pool.py::_WsPool.try_fronting_first (начинается true)
// ---------------------------------------------------------------------------

static TRY_FRONTING_FIRST: AtomicBool = AtomicBool::new(true);
static NEXT_WS_SESSION_ID: AtomicI64 = AtomicI64::new(1);
// Bound simultaneous direct Telegram WS dials across all client sessions.
// Per-route breaker permits below serialize attempts to the same DC/hostname.
static DIRECT_WS_DIALS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(4)));

fn try_direct_ws_dial_slot() -> Option<OwnedSemaphorePermit> {
    DIRECT_WS_DIALS.clone().try_acquire_owned().ok()
}

pub fn ws_pool_fronting_first() -> bool {
    TRY_FRONTING_FIRST.load(Ordering::Relaxed)
}

pub fn set_ws_pool_fronting_first(v: bool) {
    TRY_FRONTING_FIRST.store(v, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

pub fn resolve_configured_target(dc: i32, is_media: bool) -> Option<String> {
    let map = DC_OPT.read();
    if is_media {
        if let Some(t) = map.get(&(-dc)) {
            if !t.is_empty() {
                return Some(t.clone());
            }
        }
    }
    if let Some(t) = map.get(&dc) {
        if !t.is_empty() {
            return Some(t.clone());
        }
    }
    None
}

pub fn resolve_fallback_target(dc: i32, _is_media: bool, is_test: bool) -> String {
    if is_test {
        DC_TEST_IPS
            .get(&dc)
            .map(|s| s.to_string())
            .unwrap_or_default()
    } else {
        DC_DEFAULT_IPS
            .get(&dc)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

pub fn ws_domains(dc: i32, is_media: bool) -> Vec<String> {
    let mut effective_dc = dc;
    if let Some(o) = DC_OVERRIDES.get(&dc) {
        effective_dc = *o;
    }
    if is_media {
        vec![
            format!("kws{}-1.web.telegram.org", effective_dc),
            format!("kws{}.web.telegram.org", effective_dc),
        ]
    } else {
        vec![
            format!("kws{}.web.telegram.org", effective_dc),
            format!("kws{}-1.web.telegram.org", effective_dc),
        ]
    }
}

pub fn ws_path_for(is_test: bool) -> &'static str {
    if is_test {
        WS_PATH_TEST
    } else {
        WS_PATH
    }
}

pub fn media_tag(is_media: bool) -> &'static str {
    if is_media {
        " media"
    } else {
        ""
    }
}

pub fn is_media_int(b: bool) -> i32 {
    if b {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// WsPool — порт proxy/pool.py::_WsPool
// refilling-backoff (1s..120s), rotation протухших, fronting-first.
// ---------------------------------------------------------------------------

// Idle WebSocket pool budget is global, not per DC/media slot. This caps
// prewarmed sockets even when the user configures a large per-slot target.
const MAX_TOTAL_WS_POOL_CONNECTIONS: usize = 24;
const MAX_TOTAL_CF_WORKER_POOL_CONNECTIONS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DcSlot {
    pub dc: i32,
    pub is_media: i32,
}

pub struct PoolEntry {
    pub ws: RawWebSocket,
    pub route: String,
    pub created: f64, // monotonic, как time.monotonic() в оригинале
    // Permit bounds the total number of idle pooled sockets across all DC/media slots.
    _budget_permit: OwnedSemaphorePermit,
}

struct SlotState {
    generation: u64,
    queue: Mutex<VecDeque<PoolEntry>>,
    refilling: AtomicI32,
}

pub struct WsPool {
    slots: Mutex<HashMap<DcSlot, Arc<SlotState>>>,
    rotating: Mutex<HashMap<DcSlot, u64>>,
    refill_failures: Mutex<HashMap<DcSlot, (u32, f64)>>,
    generation: AtomicU64,
    // Global idle-socket budget shared by every DC and media slot.
    budget: Arc<Semaphore>,
    cancel_token: CancellationToken,
}

impl WsPool {
    pub fn new(cancel_token: CancellationToken) -> WsPool {
        WsPool {
            slots: Mutex::new(HashMap::new()),
            rotating: Mutex::new(HashMap::new()),
            refill_failures: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            budget: Arc::new(Semaphore::new(MAX_TOTAL_WS_POOL_CONNECTIONS)),
            cancel_token,
        }
    }

    async fn get_slot(&self, slot: DcSlot, generation: u64) -> Option<Arc<SlotState>> {
        let (current, stale) = {
            let mut map = self.slots.lock().await;
            if self.generation.load(Ordering::Acquire) != generation {
                return None;
            }
            let state = map.entry(slot).or_insert_with(|| {
                Arc::new(SlotState {
                    generation,
                    queue: Mutex::new(VecDeque::with_capacity(16)),
                    refilling: AtomicI32::new(0),
                })
            });
            if state.generation != generation {
                let replacement = Arc::new(SlotState {
                    generation,
                    queue: Mutex::new(VecDeque::with_capacity(16)),
                    refilling: AtomicI32::new(0),
                });
                let old = std::mem::replace(state, replacement.clone());
                (replacement, Some(old))
            } else {
                (state.clone(), None)
            }
        };

        // A slot from the previous network must not keep idle TCP sockets alive
        // after a new generation has replaced it in the map.
        if let Some(old) = stale {
            let sockets = {
                let mut queue = old.queue.lock().await;
                queue.drain(..).map(|entry| entry.ws).collect::<Vec<_>>()
            };
            let mut closing = JoinSet::new();
            for socket in sockets {
                closing.spawn(async move { socket.close().await; });
            }
            while closing.join_next().await.is_some() {}
        }
        Some(current)
    }

    pub async fn get(
        self: &Arc<Self>,
        dc: i32,
        is_media: bool,
        target_ip: String,
        domains: Vec<String>,
    ) -> Option<(RawWebSocket, String)> {
        let slot = DcSlot {
            dc,
            is_media: is_media_int(is_media),
        };
        let generation = self.generation.load(Ordering::Acquire);
        let state = match self.get_slot(slot, generation).await {
            Some(state) => state,
            None => return None,
        };
        let current_state = {
            let slots = self.slots.lock().await;
            self.generation.load(Ordering::Acquire) == generation
                && slots.get(&slot).map(|current| Arc::ptr_eq(current, &state)).unwrap_or(false)
        };
        if !current_state {
            return None;
        }
        let now = now_monotonic();

        let mut ws: Option<(RawWebSocket, String)> = None;
        let mut stale_sockets = Vec::new();
        {
            let mut q = state.queue.lock().await;
            while let Some(entry) = q.pop_front() {
                let age = now - entry.created;
                if age > WS_POOL_REUSE_MAX_AGE || entry.ws.is_closed() {
                    stale_sockets.push(entry.ws);
                    continue;
                }
                ldebug!(
                    "WS pool hit DC{}{} (age={:.1}s, left={})",
                    dc,
                    if is_media { "m" } else { "" },
                    age,
                    q.len()
                );
                ws = Some((entry.ws, entry.route));
                break;
            }
        }
        let mut closing = JoinSet::new();
        for socket in stale_sockets {
            closing.spawn(async move { socket.close().await; });
        }
        while closing.join_next().await.is_some() {}

        if self.generation.load(Ordering::Acquire) != generation {
            if let Some((socket, _route)) = ws.take() {
                socket.close().await;
            }
            return None;
        }
        if ws.is_some() {
            STATS.pool_hits.fetch_add(1, Ordering::Relaxed);
            record_diagnostic(&format!("ws_pool_hit dc={} media={}", dc, is_media as u8));
            self.report_success_generation(dc, is_media, generation).await;
        } else {
            STATS.pool_misses.fetch_add(1, Ordering::Relaxed);
            record_diagnostic(&format!("ws_pool_miss dc={} media={}", dc, is_media as u8));
        }
        self.schedule_refill_generation(slot, target_ip, domains, generation).await;
        ws
    }

    async fn schedule_refill(
        self: &Arc<Self>,
        slot: DcSlot,
        target_ip: String,
        domains: Vec<String>,
    ) {
        let generation = self.generation.load(Ordering::Acquire);
        self.schedule_refill_generation(slot, target_ip, domains, generation).await;
    }

    async fn schedule_refill_generation(
        self: &Arc<Self>,
        slot: DcSlot,
        target_ip: String,
        domains: Vec<String>,
        generation: u64,
    ) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        // Backoff как в оригинале: не раньше _refill_after.
        {
            let failures = self.refill_failures.lock().await;
            if let Some((_, after)) = failures.get(&slot) {
                if now_monotonic() < *after {
                    return;
                }
            }
        }
        let state = match self.get_slot(slot, generation).await {
            Some(state) => state,
            None => return,
        };
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        if state
            .refilling
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        // pool_size <= 0 — преконнект отключён (как pool_size=0 в оригинале).
        if POOL_SIZE.load(Ordering::Relaxed) <= 0 {
            state.refilling.store(0, Ordering::SeqCst);
            return;
        }
        let pool = self.clone();
        spawn_refill_task(pool, slot, state, target_ip, domains, generation);
    }

    async fn refill(
        self: Arc<Self>,
        slot: DcSlot,
        state: Arc<SlotState>,
        target_ip: String,
        domains: Vec<String>,
        generation: u64,
    ) {
        let _guard = RefillGuard {
            state: state.clone(),
        };
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let current_state = {
            let slots = self.slots.lock().await;
            self.generation.load(Ordering::Acquire) == generation
                && slots.get(&slot).map(|current| Arc::ptr_eq(current, &state)).unwrap_or(false)
        };
        if !current_state {
            return;
        }
        let cur_len = state.queue.lock().await.len();
        let pool_sz = POOL_SIZE.load(Ordering::Relaxed).max(0) as usize;
        if cur_len >= pool_sz || pool_sz == 0 {
            return;
        }
        let needed = pool_sz - cur_len;

        let mut handles = tokio::task::JoinSet::new();
        for _ in 0..needed {
            let target_ip = target_ip.clone();
            let domains = domains.clone();
            let cancel = self.cancel_token.clone();
            let budget = self.budget.clone();
            let pool = self.clone();
            handles.spawn(async move {
                let permit = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = budget.acquire_owned() => result.ok(),
                };
                let permit = match permit {
                    Some(permit) => permit,
                    None => return None,
                };
                if pool.generation.load(Ordering::Acquire) != generation {
                    return None;
                }
                let socket = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = connect_one_ws_with_domain(&target_ip, &domains) => result,
                };
                socket.map(|(ws, route)| (ws, route, permit))
            });
        }

        let mut connected = 0;
        // Completion-order handling: a slow TLS attempt cannot hold ready media sockets.
        while let Some(result) = handles.join_next().await {
            if let Ok(Some((ws, route, permit))) = result {
                let mut candidate = Some((ws, route, permit));
                {
                    // Lock order matches reset/close_all. If reset advances the
                    // generation while this lock is held, reset drains this entry
                    // immediately afterwards; if it advanced earlier, reject it.
                    let slots = self.slots.lock().await;
                    let is_current = self.generation.load(Ordering::Acquire) == generation
                        && slots.get(&slot).map(|current| Arc::ptr_eq(current, &state)).unwrap_or(false);
                    if is_current {
                        let mut queue = state.queue.lock().await;
                        if queue.len() < 16 {
                            let (ws, route, budget_permit) =
                                candidate.take().expect("candidate socket is present");
                            queue.push_back(PoolEntry {
                                ws,
                                route,
                                created: now_monotonic(),
                                _budget_permit: budget_permit,
                            });
                            connected += 1;
                        }
                    }
                }
                if let Some((socket, _route, budget_permit)) = candidate {
                    socket.close().await;
                    drop(budget_permit);
                }
            }
        }

        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        if connected > 0 {
            self.report_success_generation(slot.dc, slot.is_media != 0, generation).await;
            self.schedule_rotation(slot, target_ip, domains, generation).await;
        } else {
            // Экспоненциальный backoff 1s..120s.
            let mut failures = self.refill_failures.lock().await;
            if self.generation.load(Ordering::Acquire) != generation {
                return;
            }
            let count = failures.get(&slot).map(|(c, _)| *c).unwrap_or(0) + 1;
            let shift = (count - 1).min(12);
            let delay = (WS_POOL_REFILL_BACKOFF_INITIAL * (2u64.pow(shift) as f64))
                .min(WS_POOL_REFILL_BACKOFF_MAX);
            failures.insert(slot, (count, now_monotonic() + delay));
            drop(failures);
            linfo!(
                "WS pool refill failed for DC{}{}, retry in {:.0}s",
                slot.dc,
                if slot.is_media != 0 { "m" } else { "" },
                delay
            );
        }
        ldebug!(
            "WS pool refilled DC{}{}: {} ready",
            slot.dc,
            if slot.is_media != 0 { "m" } else { "" },
            state.queue.lock().await.len()
        );
    }

    pub async fn report_success(&self, dc: i32, is_media: bool) {
        let generation = self.generation.load(Ordering::Acquire);
        self.report_success_generation(dc, is_media, generation).await;
    }

    async fn report_success_generation(&self, dc: i32, is_media: bool, generation: u64) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let slot = DcSlot {
            dc,
            is_media: is_media_int(is_media),
        };
        let mut failures = self.refill_failures.lock().await;
        if self.generation.load(Ordering::Acquire) == generation {
            failures.remove(&slot);
        }
    }

    async fn schedule_rotation(
        self: &Arc<Self>,
        slot: DcSlot,
        target_ip: String,
        domains: Vec<String>,
        generation: u64,
    ) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        {
            let mut rotating = self.rotating.lock().await;
            if self.generation.load(Ordering::Acquire) != generation {
                return;
            }
            if rotating.get(&slot).copied() == Some(generation) {
                return;
            }
            rotating.insert(slot, generation);
        }
        let pool = self.clone();
        let cancel = self.cancel_token.clone();
        spawn_rotate_task(pool, slot, target_ip, domains, cancel, generation);
    }

    async fn rotate(
        self: Arc<Self>,
        slot: DcSlot,
        target_ip: String,
        domains: Vec<String>,
        cancel: CancellationToken,
        generation: u64,
    ) {
        loop {
            if self.generation.load(Ordering::Acquire) != generation {
                break;
            }
            // Спим до ближайшего протухания, но не реже CHECK_INTERVAL.
            let sleep_dur = {
                let map = self.slots.lock().await;
                if self.generation.load(Ordering::Acquire) != generation {
                    None
                } else {
                    match map.get(&slot) {
                    Some(state) => {
                        let q = state.queue.lock().await;
                        if q.is_empty() {
                            None
                        } else {
                            let now = now_monotonic();
                            let min_expires = q
                                .iter()
                                .map(|e| e.created + WS_POOL_REUSE_MAX_AGE)
                                .fold(f64::INFINITY, f64::min);
                            Some((min_expires - now).clamp(0.0, WS_POOL_CHECK_INTERVAL))
                        }
                    }
                    None => None,
                    }
                }
            };
            let sleep_dur = match sleep_dur {
                Some(d) => d,
                None => break,
            };
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs_f64(sleep_dur)) => {}
            }
            if self.generation.load(Ordering::Acquire) != generation {
                break;
            }

            let now = now_monotonic();
            let expired_sockets: Vec<RawWebSocket>;
            let expired_count: usize;
            let ready_count: usize;
            {
                let map = self.slots.lock().await;
                if self.generation.load(Ordering::Acquire) != generation {
                    break;
                }
                let state = match map.get(&slot) {
                    Some(s) => s.clone(),
                    None => break,
                };
                let mut q = state.queue.lock().await;
                let mut ready = VecDeque::with_capacity(q.len());
                let mut expired = Vec::new();
                while let Some(e) = q.pop_front() {
                    if now - e.created >= WS_POOL_REUSE_MAX_AGE || e.ws.is_closed() {
                        expired.push(e.ws);
                    } else {
                        ready.push_back(e);
                    }
                }
                expired_count = expired.len();
                *q = ready;
                ready_count = q.len();
                expired_sockets = expired;
            }
            // Do not detach socket-close work from the rotation task.
            let mut closing = JoinSet::new();
            for socket in expired_sockets {
                closing.spawn(async move { socket.close().await; });
            }
            while closing.join_next().await.is_some() {}
            if expired_count > 0 {
                ldebug!(
                    "WS pool rotated DC{}{}: {} stale, {} ready",
                    slot.dc,
                    if slot.is_media != 0 { "m" } else { "" },
                    expired_count,
                    ready_count
                );
            }
            let pool_sz = POOL_SIZE.load(Ordering::Relaxed).max(0) as usize;
            if ready_count == 0 {
                break;
            }
            if ready_count < pool_sz {
                self.schedule_refill_generation(
                    slot,
                    target_ip.clone(),
                    domains.clone(),
                    generation,
                )
                .await;
            }
        }
        let mut rotating = self.rotating.lock().await;
        if rotating.get(&slot).copied() == Some(generation) {
            rotating.remove(&slot);
        }
    }

    pub async fn warmup(self: &Arc<Self>, dc_opt_map: &HashMap<i32, String>) {
        let mut ndcs = 0;
        for (dc, target_ip) in dc_opt_map {
            if target_ip.is_empty() {
                continue;
            }
            ndcs += 1;
            for is_media in [false, true] {
                let domains = ws_domains(*dc, is_media);
                let slot = DcSlot {
                    dc: *dc,
                    is_media: is_media_int(is_media),
                };
                self.schedule_refill(slot, target_ip.clone(), domains).await;
            }
        }
        linfo!("WS pool warmup started for {} DC(s)", ndcs);
    }

    pub fn budget_available(&self) -> usize {
        self.budget.available_permits()
    }

    pub async fn idle_count(&self) -> usize {
        let map = self.slots.lock().await;
        let mut count = 0;
        for s in map.values() {
            count += s.queue.lock().await.len();
        }
        count
    }

    pub async fn close_all(&self) {
        let sockets = {
            let map = self.slots.lock().await;
            let mut sockets = Vec::new();
            for state in map.values() {
                let mut queue = state.queue.lock().await;
                sockets.extend(queue.drain(..).map(|entry| entry.ws));
            }
            sockets
        };
        let mut closing = JoinSet::new();
        for socket in sockets {
            closing.spawn(async move { socket.close().await; });
        }
        while closing.join_next().await.is_some() {}
    }

    pub async fn reset(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.close_all().await;
        self.slots.lock().await.clear();
        self.rotating.lock().await.clear();
        self.refill_failures.lock().await.clear();
        set_ws_pool_fronting_first(true);
    }
}

// RefillGuard снимает флаг refilling при выходе (аналог finally: discard).
struct RefillGuard {
    state: Arc<SlotState>,
}
impl Drop for RefillGuard {
    fn drop(&mut self) {
        self.state.refilling.store(0, Ordering::SeqCst);
    }
}

// Синхронные спавн-хелперы: разрывают цикл типов async-future
// (schedule_refill → refill → schedule_rotation → rotate → schedule_refill),
// о который спотыкается rustc (E0391). Sync-fn не вкладывает future одной
// async-функции в другую — внутри только JoinHandle.
fn spawn_refill_task(
    pool: Arc<WsPool>,
    slot: DcSlot,
    state: Arc<SlotState>,
    target_ip: String,
    domains: Vec<String>,
    generation: u64,
) {
    tokio::spawn(async move {
        pool.refill(slot, state, target_ip, domains, generation).await;
    });
}

fn spawn_rotate_task(
    pool: Arc<WsPool>,
    slot: DcSlot,
    target_ip: String,
    domains: Vec<String>,
    cancel: CancellationToken,
    generation: u64,
) {
    tokio::spawn(async move {
        pool.rotate(slot, target_ip, domains, cancel, generation).await;
    });
}

// ---------------------------------------------------------------------------
// CfWorkerPool — порт proxy/pool.py::_CfWorkerPool
// ---------------------------------------------------------------------------

pub struct CfWorkerEntry {
    pub ws: RawWebSocket,
    pub created: f64,
    pub worker_domain: String,
    pub generation: u64,
    _budget_permit: OwnedSemaphorePermit,
}

pub struct CfWorkerPool {
    idle: Mutex<HashMap<i32, VecDeque<CfWorkerEntry>>>,
    // Associate each in-flight refill with the pool generation that started it.
    // A network reset invalidates old work without letting it clear a new guard.
    refilling: Mutex<HashMap<i32, u64>>,
    generation: AtomicU64,
    exhausted_until: Mutex<HashMap<String, f64>>,
    budget: Arc<Semaphore>,
    cancel_token: CancellationToken,
}

impl CfWorkerPool {
    pub fn new(cancel_token: CancellationToken) -> CfWorkerPool {
        CfWorkerPool {
            idle: Mutex::new(HashMap::new()),
            refilling: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            exhausted_until: Mutex::new(HashMap::new()),
            budget: Arc::new(Semaphore::new(MAX_TOTAL_CF_WORKER_POOL_CONNECTIONS)),
            cancel_token,
        }
    }

    pub async fn get(
        self: &Arc<Self>,
        dc: i32,
        fallback_dst: String,
        worker_domains: Vec<String>,
    ) -> Option<(RawWebSocket, String)> {
        let generation = self.generation.load(Ordering::Acquire);
        let now = now_monotonic();
        let (entry, stale_sockets) = {
            let mut map = self.idle.lock().await;
            let bucket = map.entry(dc).or_insert_with(VecDeque::new);
            let mut hit = None;
            let mut stale = Vec::new();
            while let Some(e) = bucket.pop_front() {
                if e.generation != generation
                    || now - e.created > CF_WORKER_POOL_MAX_AGE
                    || e.ws.is_closed()
                {
                    stale.push(e.ws);
                    continue;
                }
                hit = Some(e);
                break;
            }
            (hit, stale)
        };
        for socket in stale_sockets {
            socket.close().await;
        }
        let mut result = match entry {
            Some(e) => {
                STATS.cf_pool_hits.fetch_add(1, Ordering::Relaxed);
                record_diagnostic(&format!("cf_pool_hit dc={}", dc));
                ldebug!(
                    "CF worker pool hit DC{} via {} (age={:.1}s)",
                    dc,
                    e.worker_domain,
                    now - e.created
                );
                let CfWorkerEntry { ws, worker_domain, _budget_permit, .. } = e;
                drop(_budget_permit);
                Some((ws, worker_domain))
            }
            None => {
                STATS.cf_pool_misses.fetch_add(1, Ordering::Relaxed);
                record_diagnostic(&format!("cf_pool_miss dc={}", dc));
                None
            }
        };

        if self.generation.load(Ordering::Acquire) != generation {
            if let Some((socket, _)) = result.take() {
                socket.close().await;
            }
            return None;
        }

        // Refill after both a hit (the entry was consumed) and a miss (start
        // warming the pool while the caller uses the direct fallback path).
        self.refill_now_generation(dc, fallback_dst, worker_domains, generation)
            .await;
        result
    }

    pub async fn refill_now(
        self: &Arc<Self>,
        dc: i32,
        fallback_dst: String,
        worker_domains: Vec<String>,
    ) {
        let generation = self.generation.load(Ordering::Acquire);
        self.refill_now_generation(dc, fallback_dst, worker_domains, generation)
            .await;
    }

    async fn refill_now_generation(
        self: &Arc<Self>,
        dc: i32,
        fallback_dst: String,
        worker_domains: Vec<String>,
        generation: u64,
    ) {
        // Keep one implementation of the in-flight guard and actual refill.
        // The previous placeholder inserted `dc` into `refilling` but never
        // spawned a task, permanently disabling all subsequent refills.
        let should = {
            let mut r = self.refilling.lock().await;
            if self.generation.load(Ordering::Acquire) != generation || r.contains_key(&dc) {
                false
            } else {
                r.insert(dc, generation);
                true
            }
        };
        if !should {
            return;
        }
        record_diagnostic(&format!("cf_pool_refill_start dc={}", dc));
        let pool = self.clone();
        let cancel = self.cancel_token.clone();
        tokio::spawn(async move {
            if pool.generation.load(Ordering::Acquire) != generation {
                let mut refilling = pool.refilling.lock().await;
                if refilling.get(&dc).copied() == Some(generation) {
                    refilling.remove(&dc);
                }
                return;
            }
            let limit = CF_WORKER_PER_DC_LIMIT
                .min(POOL_SIZE.load(Ordering::Relaxed).max(0) as usize);
            let current_len = {
                let map = pool.idle.lock().await;
                map.get(&dc).map(|bucket| bucket.len()).unwrap_or(0)
            };
            if limit == 0 || current_len >= limit {
                record_diagnostic(&format!("cf_pool_refill_skip dc={} current={} limit={}", dc, current_len, limit));
            } else {
                let permit = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = pool.budget.clone().acquire_owned() => result.ok(),
                };
                let connected = if let Some(permit) = permit {
                    tokio::select! {
                        _ = cancel.cancelled() => None,
                        result = pool.connect_one(worker_domains, fallback_dst, dc, generation) => {
                            result.map(|(ws, domain)| (ws, domain, permit))
                        },
                    }
                } else {
                    None
                };

                if let Some((ws, domain, budget_permit)) = connected {
                    let mut map = pool.idle.lock().await;
                    if pool.generation.load(Ordering::Acquire) == generation {
                        let bucket = map.entry(dc).or_insert_with(VecDeque::new);
                        if bucket.len() < limit {
                            bucket.push_back(CfWorkerEntry {
                                ws,
                                created: now_monotonic(),
                                worker_domain: domain,
                                generation,
                                _budget_permit: budget_permit,
                            });
                            record_diagnostic(&format!("cf_pool_refill_ok dc={}", dc));
                            ldebug!("CF worker pool refilled DC{}: ready", dc);
                        } else {
                            drop(map);
                            ws.close().await;
                            drop(budget_permit);
                            record_diagnostic(&format!("cf_pool_refill_discard dc={}", dc));
                        }
                    } else {
                        drop(map);
                        ws.close().await;
                        drop(budget_permit);
                        record_diagnostic(&format!("cf_pool_refill_stale_generation dc={}", dc));
                    }
                } else {
                    record_diagnostic(&format!("cf_pool_refill_fail dc={}", dc));
                    ldebug!("CF worker pool refill DC{}: no connection", dc);
                }
            }

            // An old task must not remove the in-flight marker of a refill
            // already started after ResetRouteFailures().
            let mut refilling = pool.refilling.lock().await;
            if refilling.get(&dc).copied() == Some(generation) {
                refilling.remove(&dc);
            }
        });
    }

    async fn connect_one(
        &self,
        worker_domains: Vec<String>,
        fallback_dst: String,
        dc: i32,
        generation: u64,
    ) -> Option<(RawWebSocket, String)> {
        let secure = !DISABLE_SECURE.load(Ordering::Relaxed);
        let query = format!("dst={}&dc={}", fallback_dst, dc);
        let path = format!("/apiws?{}", query);
        for worker_domain in self.available_domains(dc, worker_domains).await {
            if self.generation.load(Ordering::Acquire) != generation {
                return None;
            }
            let started = Instant::now();
            match ws_connect_full_opts(&worker_domain, &worker_domain, &path, 8.0, None, Some(secure))
                .await
            {
                Ok(ws) => {
                    crate::balancer::BALANCER.write().record_success(
                        dc, "cf_worker", &worker_domain, started.elapsed()
                    );
                    return Some((ws, worker_domain));
                }
                Err(exc) => {
                    crate::balancer::BALANCER.write().record_failure(
                        dc, "cf_worker", &worker_domain
                    );
                    self.report_failure_generation(&worker_domain, &exc, generation).await;
                }
            }
        }
        None
    }

    pub async fn available_domains(&self, dc: i32, worker_domains: Vec<String>) -> Vec<String> {
        let now = now_unix_f64();
        let mut exhausted = self.exhausted_until.lock().await;
        let mut domains = Vec::new();
        let mut seen = HashSet::new();
        for raw in worker_domains {
            let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
            if !crate::config::is_valid_domain(&domain) || !seen.insert(domain.clone()) {
                continue;
            }
            if let Some(until) = exhausted.get(&domain) {
                if *until > now {
                    continue;
                }
            }
            exhausted.remove(&domain);
            domains.push(domain);
        }
        drop(exhausted);
        // random.shuffle как в оригинале.
        {
            use rand::seq::SliceRandom;
            let mut rng = rand::thread_rng();
            domains.shuffle(&mut rng);
        }
        crate::balancer::BALANCER.read().rank_candidates(dc, "cf_worker", &domains)
    }

    pub async fn report_failure(&self, worker_domain: &str, exc: &WsError) {
        let generation = self.generation.load(Ordering::Acquire);
        self.report_failure_generation(worker_domain, exc, generation).await;
    }

    async fn report_failure_generation(
        &self,
        worker_domain: &str,
        exc: &WsError,
        generation: u64,
    ) {
        if self.generation.load(Ordering::Acquire) != generation
            || exc.handshake().map(|h| h.status_code) != Some(429)
        {
            return;
        }
        // Worker hostnames (usually *.workers.dev or a custom domain) are not
        // the encoded *.co.uk CF-proxy domains handled by normalize_cf_domain.
        let domain = worker_domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if !crate::config::is_valid_domain(&domain) {
            return;
        }
        let retry_after = retry_after_delay(exc);
        let delay = if retry_after.is_zero() {
            CFPROXY_429_COOLDOWN
        } else {
            retry_after.min(CFPROXY_429_MAX_COOLDOWN)
        };
        let until = now_unix_f64() + delay.as_secs_f64();
        let mut exhausted = self.exhausted_until.lock().await;
        if self.generation.load(Ordering::Acquire) == generation {
            exhausted.insert(domain.clone(), until);
            linfo!("CF worker domain {} rate-limited; cooldown {:.0}s", domain, delay.as_secs_f64());
        }
    }

    pub async fn warmup(self: &Arc<Self>, dc_opt_map: &HashMap<i32, String>) {
        let worker_domains = CF_WORKER_DOMAINS.read().clone();
        if worker_domains.is_empty() {
            return;
        }
        // CF worker греем только для DC, которых НЕТ в прямых редиректах.
        let fallbacks: Vec<(i32, String)> = DC_DEFAULT_IPS
            .iter()
            .filter(|(dc, _)| !dc_opt_map.contains_key(dc))
            .map(|(dc, ip)| (*dc, ip.to_string()))
            .collect();
        if fallbacks.is_empty() {
            return;
        }
        for (dc, fallback_dst) in &fallbacks {
            self.refill_now(*dc, fallback_dst.clone(), worker_domains.clone())
                .await;
        }
        linfo!("CF worker pool warmup started for {} DC(s)", fallbacks.len());
    }

    pub fn budget_available(&self) -> usize {
        self.budget.available_permits()
    }

    pub async fn close_all(&self) {
        let sockets = {
            let mut map = self.idle.lock().await;
            let mut sockets = Vec::new();
            for bucket in map.values_mut() {
                sockets.extend(bucket.drain(..).map(|entry| entry.ws));
            }
            sockets
        };
        let mut closing = JoinSet::new();
        for socket in sockets {
            closing.spawn(async move { socket.close().await; });
        }
        while closing.join_next().await.is_some() {}
    }

    pub async fn reset(&self) {
        // Invalidate in-flight refill tasks before draining idle sockets. Tasks
        // from the previous generation may finish, but cannot repopulate this pool.
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.close_all().await;
        self.idle.lock().await.clear();
        self.refilling.lock().await.clear();
        self.exhausted_until.lock().await.clear();
    }
}

// ---------------------------------------------------------------------------
// Shared pools (WS + CF worker) — один Arc на соединение.
// ---------------------------------------------------------------------------

pub struct ProxyPools {
    pub ws: Arc<WsPool>,
    pub cf_worker: Arc<CfWorkerPool>,
}

impl ProxyPools {
    pub fn new(cancel_token: CancellationToken) -> Self {
        ProxyPools {
            ws: Arc::new(WsPool::new(cancel_token.clone())),
            cf_worker: Arc::new(CfWorkerPool::new(cancel_token)),
        }
    }

    pub async fn warmup(&self, dc_opt_map: &HashMap<i32, String>) {
        self.ws.warmup(dc_opt_map).await;
        self.cf_worker.warmup(dc_opt_map).await;
    }

    pub fn budget_snapshot(&self) -> (usize, usize) {
        (self.ws.budget_available(), self.cf_worker.budget_available())
    }

    pub async fn close_all(&self) {
        self.ws.close_all().await;
        self.cf_worker.close_all().await;
    }

    pub async fn reset(&self) {
        self.ws.reset().await;
        self.cf_worker.reset().await;
    }
}

// ---------------------------------------------------------------------------
// HTTP transport detection
// ---------------------------------------------------------------------------

pub fn is_http_transport(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    &data[..4] == b"POST"
        || &data[..3] == b"GET"
        || &data[..4] == b"HEAD"
        || (data.len() >= 7 && &data[..7] == b"OPTIONS")
}

// ---------------------------------------------------------------------------
// Bridge WS — generic core + TcpStream wrapper.
// Сессионная статистика как в bridge_ws_reencrypt оригинала.
// ---------------------------------------------------------------------------

pub async fn bridge_ws<R, W>(
    conn_read: R,
    conn_write: W,
    ws: RawWebSocket,
    label: String,
    dc: i32,
    is_media: bool,
    mut splitter: Option<MsgSplitter>,
    mut clt_dec: TrackedStream,
    mut clt_enc: TrackedStream,
    mut tg_enc: TrackedStream,
    mut tg_dec: TrackedStream,
    cancel_token: CancellationToken,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let dc_tag = format!("DC{}{}", dc, if is_media { "m" } else { "" });
    let session_id = NEXT_WS_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let ws = Arc::new(ws);
    let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));
    let route_observation = Arc::new(Mutex::new(route_observation));
    let first_request_at = Arc::new(Mutex::new(None::<Instant>));
    // A CancellationToken is level-triggered: if one relay direction exits
    // before the other starts waiting, cancellation is still observed.
    // Notify::notify_waiters() was edge-triggered and could be missed here,
    // leaving the opposite half-open TCP socket alive until the 120s read timeout.
    let session_cancel = CancellationToken::new();
    let start = now_monotonic();
    record_diagnostic(&format!("ws_session_start session={} dc={} media={}", session_id, dc, is_media as u8));

    let mut conn_read = conn_read;
    let mut conn_write = conn_write;

    // ping keepalive (мобильное улучшение поверх оригинала).
    let ws_ping = ws.clone();
    let la_ping = last_activity.clone();
    let cancel_ping = session_cancel.clone();
    let cancel_token_ping = cancel_token.clone();
    let close_reason_ping = Arc::new(Mutex::new(String::new()));
    let close_reason_ping_task = close_reason_ping.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(BRIDGE_PING_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel_token_ping.cancelled() => return,
                _ = cancel_ping.cancelled() => return,
                _ = interval.tick() => {
                    let idle = la_ping.lock().await.elapsed();
                    if idle > BRIDGE_PING_INTERVAL {
                        match ws_ping.send_ping().await {
                            Ok(rtt) => record_diagnostic(&format!(
                                "ws_ping_ok session={} rtt_ms={}", session_id, rtt.as_millis()
                            )),
                            Err(error) => {
                                let reason = match &error {
                                    WsError::Timeout => "timeout",
                                    WsError::Canceled => "canceled",
                                    WsError::Handshake(_) => "handshake",
                                    WsError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => "io_timeout",
                                    WsError::Io(_) => "io",
                                    WsError::Other(_) => "other",
                                };
                                record_diagnostic(&format!(
                                    "ws_ping_fail session={} reason={}", session_id, reason
                                ));
                                *close_reason_ping_task.lock().await =
                                    format!("upstream: ping_{}", reason);
                                cancel_ping.cancel();
                                return;
                            }
                        }
                    }
                }
            }
        }
    });

    // up: client -> ws
    let ws_up = ws.clone();
    let la_up = last_activity.clone();
    let cancel_up = session_cancel.clone();
    let cancel_token_up = cancel_token.clone();
    let up_bytes = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let up_packets = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let up_bytes_c = up_bytes.clone();
    let up_packets_c = up_packets.clone();
    let first_request_up = first_request_at.clone();
    let close_reason_up = Arc::new(Mutex::new(String::new()));
    let close_reason_up_c = close_reason_up.clone();
    let up_task = tokio::spawn(async move {
        let mut buf = vec![0u8; WS_BRIDGE_CHUNK_SIZE];
        loop {
            let read_res = tokio::select! {
                _ = cancel_token_up.cancelled() => break,
                _ = cancel_up.cancelled() => break,
                r = tokio::time::timeout(BRIDGE_READ_TIMEOUT, conn_read.read(&mut buf)) => r,
            };
            let n = match read_res {
                Ok(Ok(0)) => {
                    // EOF: flush splitter tail
                    if let Some(sp) = splitter.as_mut() {
                        let tail = sp.flush();
                        if !tail.is_empty() {
                            let r = if tail.len() > 1 {
                                ws_up.send_batch(&tail).await
                            } else {
                                ws_up.send(&tail[0]).await
                            };
                            if r.is_err() {
                                *close_reason_up_c.lock().await =
                                    "upstream: send_failed".to_string();
                                break;
                            }
                        }
                    }
                    // A clean TCP FIN from Telegram is not an upstream WS CLOSE.
                    *close_reason_up_c.lock().await = "client: tcp_eof".to_string();
                    break;
                }
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    *close_reason_up_c.lock().await =
                        format!("client: {}", e.kind());
                    break;
                }
                Err(_) => {
                    *close_reason_up_c.lock().await = "client: timeout".to_string();
                    break;
                }
            };

            let chunk = &mut buf[..n];
            STATS.bytes_up.fetch_add(n as i64, Ordering::Relaxed);
            if is_media { STATS.media_bytes_up.fetch_add(n as i64, Ordering::Relaxed); }
            up_bytes_c.fetch_add(n as i64, Ordering::Relaxed);
            up_packets_c.fetch_add(1, Ordering::Relaxed);
            *la_up.lock().await = std::time::Instant::now();

            clt_dec.xor(chunk);
            tg_enc.xor(chunk);

            let (send_err, sent_payload) = {
                if let Some(sp) = splitter.as_mut() {
                    let parts = sp.split(chunk);
                    if parts.len() > 1 {
                        let failed = ws_up.send_batch(&parts).await.is_err();
                        (failed, !failed)
                    } else if parts.len() == 1 {
                        let failed = ws_up.send(&parts[0]).await.is_err();
                        (failed, !failed)
                    } else {
                        (false, false)
                    }
                } else {
                    let failed = ws_up.send(chunk).await.is_err();
                    (failed, !failed)
                }
            };
            if send_err {
                *close_reason_up_c.lock().await = "upstream: send_failed".to_string();
                break;
            }
            if sent_payload {
                let mut first = first_request_up.lock().await;
                if first.is_none() {
                    *first = Some(Instant::now());
                }
            }
        }
        let reason = close_reason_up_c.lock().await.clone();
        record_diagnostic(&format!(
            "ws_direction_end session={} direction=client_to_ws reason={}",
            session_id,
            if reason.is_empty() { "canceled" } else { &reason }
        ));
        cancel_up.cancel();
    });

    // down: ws -> client
    let ws_down = ws.clone();
    let la_down = last_activity.clone();
    let cancel_down = session_cancel.clone();
    let cancel_token_down = cancel_token.clone();
    let down_bytes = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let down_packets = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let down_bytes_c = down_bytes.clone();
    let down_packets_c = down_packets.clone();
    let route_observation_down = route_observation.clone();
    let first_request_down = first_request_at.clone();
    let close_reason_down = Arc::new(Mutex::new(String::new()));
    let close_reason_down_c = close_reason_down.clone();
    let down_task = tokio::spawn(async move {
        loop {
            let recv_res = tokio::select! {
                _ = cancel_token_down.cancelled() => break,
                _ = cancel_down.cancelled() => break,
                r = ws_down.recv() => r,
            };
            let mut data = match recv_res {
                Ok(d) => d,
                Err(error) => {
                    let reason = match error {
                        WsError::Timeout => "upstream: read_timeout",
                        WsError::Canceled => "upstream: canceled",
                        WsError::Handshake(_) => "upstream: handshake_error",
                        WsError::Other(ref text) if text.starts_with("ws_close_frame:") => {
                            if text == "ws_close_frame:normal" {
                                "upstream: ws_close_normal"
                            } else if text == "ws_close_frame:going_away" {
                                "upstream: ws_close_going_away"
                            } else {
                                "upstream: ws_close_other"
                            }
                        }
                        WsError::Io(ref e) => match e.kind() {
                            std::io::ErrorKind::UnexpectedEof => "upstream: tcp_eof",
                            std::io::ErrorKind::ConnectionReset => "upstream: tcp_reset",
                            std::io::ErrorKind::ConnectionAborted => "upstream: tcp_abort",
                            std::io::ErrorKind::BrokenPipe => "upstream: broken_pipe",
                            std::io::ErrorKind::TimedOut => "upstream: io_timeout",
                            _ => "upstream: io_error",
                        },
                        WsError::Other(_) => "upstream: protocol_error",
                    };
                    *close_reason_down_c.lock().await = reason.to_string();
                    break;
                }
            };
            let n = data.len();
            if n > 0 {
                let observation = route_observation_down.lock().await.take();
                if let Some(observation) = observation {
                    let request_elapsed = {
                        let first_request = first_request_down.lock().await;
                        first_request.as_ref().map(|started| started.elapsed())
                    };
                    let elapsed = request_elapsed
                        .unwrap_or_else(|| observation.connected_at.elapsed());
                    crate::balancer::BALANCER.write().record_first_byte_success(
                        observation.dc_id,
                        &observation.route_kind,
                        &observation.route,
                        elapsed,
                    );
                    record_diagnostic(&format!(
                        "route_first_byte dc={} kind={} latency_ms={}",
                        observation.dc_id,
                        observation.route_kind,
                        elapsed.as_millis()
                    ));
                }
            }
            STATS.bytes_down.fetch_add(n as i64, Ordering::Relaxed);
            if is_media { STATS.media_bytes_down.fetch_add(n as i64, Ordering::Relaxed); }
            down_bytes_c.fetch_add(n as i64, Ordering::Relaxed);
            down_packets_c.fetch_add(1, Ordering::Relaxed);
            *la_down.lock().await = std::time::Instant::now();

            tg_dec.xor(&mut data);
            clt_enc.xor(&mut data);
            if conn_write.write_all(&data).await.is_err() {
                *close_reason_down_c.lock().await = "client: write_failed".to_string();
                break;
            }
        }
        let reason = close_reason_down_c.lock().await.clone();
        record_diagnostic(&format!(
            "ws_direction_end session={} direction=ws_to_client reason={}",
            session_id,
            if reason.is_empty() { "canceled" } else { &reason }
        ));
        cancel_down.cancel();
    });

    let _ = up_task.await;
    let _ = down_task.await;
    session_cancel.cancel();
    ping_task.abort();

    let close_reason = {
        let up_r = close_reason_up.lock().await.clone();
        let down_r = close_reason_down.lock().await.clone();
        let ping_r = close_reason_ping.lock().await.clone();
        if !down_r.is_empty() {
            down_r
        } else if !up_r.is_empty() {
            up_r
        } else if !ping_r.is_empty() {
            ping_r
        } else {
            "normal".to_string()
        }
    };
    if let Some(observation) = route_observation.lock().await.take() {
        let request_was_sent = first_request_at.lock().await.is_some();
        let upstream_failed = close_reason.starts_with("upstream:")
            && close_reason != "upstream: canceled";
        let send_failed = close_reason.contains("send_failed");
        if (request_was_sent && upstream_failed) || send_failed {
            crate::balancer::BALANCER.write().record_early_failure(
                observation.dc_id,
                &observation.route_kind,
                &observation.route,
            );
            record_diagnostic(&format!(
                "route_probe_failed dc={} kind={} reason=early_close",
                observation.dc_id, observation.route_kind
            ));
        } else {
            crate::balancer::BALANCER.write().release_probe(
                observation.dc_id,
                &observation.route_kind,
                &observation.route,
                observation.attempt_kind,
            );
            record_diagnostic(&format!(
                "route_probe_neutral dc={} kind={} reason={}",
                observation.dc_id, observation.route_kind, close_reason
            ));
        }
    }
    let elapsed = now_monotonic() - start;
    record_diagnostic(&format!(
        "ws_session_end session={} dc={} media={} reason={} duration_ms={} up_bytes={} down_bytes={}",
        session_id,
        dc,
        is_media as u8,
        close_reason,
        (elapsed * 1000.0) as u64,
        up_bytes.load(Ordering::Relaxed),
        down_bytes.load(Ordering::Relaxed),
    ));
    linfo!(
        "[{}] {} WS session closed ({}): ^{}{} ({} pkts) v{}{} ({} pkts) in {:.1}s",
        label,
        dc_tag,
        close_reason,
        human_bytes(up_bytes.load(Ordering::Relaxed)),
        "",
        up_packets.load(Ordering::Relaxed),
        human_bytes(down_bytes.load(Ordering::Relaxed)),
        "",
        down_packets.load(Ordering::Relaxed),
        elapsed
    );

    ws.close().await;
}

/// Совместимость: bridge по TcpStream (разрезаем внутри).
pub async fn bridge_ws_stream(
    conn: TcpStream,
    ws: RawWebSocket,
    label: String,
    dc: i32,
    is_media: bool,
    splitter: Option<MsgSplitter>,
    clt_dec: TrackedStream,
    clt_enc: TrackedStream,
    tg_enc: TrackedStream,
    tg_dec: TrackedStream,
    cancel_token: CancellationToken,
) {
    let (conn_read, conn_write) = tokio::io::split(conn);
    bridge_ws(
        conn_read,
        conn_write,
        ws,
        label,
        dc,
        is_media,
        splitter,
        clt_dec,
        clt_enc,
        tg_enc,
        tg_dec,
        cancel_token,
        None,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Bridge TCP — generic core + wrapper.
// ---------------------------------------------------------------------------

pub async fn bridge_tcp<CR, CW>(
    mut c_read: CR,
    mut c_write: CW,
    mut remote: TcpStream,
    clt_dec: TrackedStream,
    clt_enc: TrackedStream,
    tg_enc: TrackedStream,
    tg_dec: TrackedStream,
    cancel_token: CancellationToken,
    route_observation: Option<RouteObservation>,
) where
    CR: AsyncReadExt + Unpin + Send + 'static,
    CW: AsyncWriteExt + Unpin + Send + 'static,
{
    let (mut r_read, mut r_write) = remote.split();

    let clt_dec = Arc::new(Mutex::new(clt_dec));
    let clt_enc = Arc::new(Mutex::new(clt_enc));
    let tg_enc = Arc::new(Mutex::new(tg_enc));
    let tg_dec = Arc::new(Mutex::new(tg_dec));

    // A CancellationToken is level-triggered: if one relay direction exits
    // before the other starts waiting, cancellation is still observed.
    // Notify::notify_waiters() was edge-triggered and could be missed here,
    // leaving the opposite half-open TCP socket alive until the 120s read timeout.
    let session_cancel = CancellationToken::new();

    let cancel_up = session_cancel.clone();
    let cancel_token_up = cancel_token.clone();
    let clt_dec_up = clt_dec.clone();
    let tg_enc_up = tg_enc.clone();
    let up = async move {
        let mut buf = vec![0u8; 131072];
        loop {
            let n = tokio::select! {
                _ = cancel_token_up.cancelled() => break,
                _ = cancel_up.cancelled() => break,
                r = tokio::time::timeout(BRIDGE_READ_TIMEOUT, c_read.read(&mut buf)) => match r {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => n,
                    _ => break,
                },
            };
            let chunk = &mut buf[..n];
            STATS.bytes_up.fetch_add(n as i64, Ordering::Relaxed);
            clt_dec_up.lock().await.xor(chunk);
            tg_enc_up.lock().await.xor(chunk);
            if r_write.write_all(chunk).await.is_err() {
                break;
            }
        }
        cancel_up.cancel();
    };

    let cancel_down = session_cancel.clone();
    let cancel_token_down = cancel_token.clone();
    let tg_dec_down = tg_dec.clone();
    let clt_enc_down = clt_enc.clone();
    let down = async move {
        let mut buf = vec![0u8; 131072];
        loop {
            let n = tokio::select! {
                _ = cancel_token_down.cancelled() => break,
                _ = cancel_down.cancelled() => break,
                r = tokio::time::timeout(BRIDGE_READ_TIMEOUT, r_read.read(&mut buf)) => match r {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => n,
                    _ => break,
                },
            };
            let chunk = &mut buf[..n];
            STATS.bytes_down.fetch_add(n as i64, Ordering::Relaxed);
            tg_dec_down.lock().await.xor(chunk);
            clt_enc_down.lock().await.xor(chunk);
            if c_write.write_all(chunk).await.is_err() {
                break;
            }
        }
        cancel_down.cancel();
    };

    tokio::join!(up, down);
    session_cancel.cancel();
    let _ = (clt_dec, clt_enc, tg_enc, tg_dec);
}

pub async fn bridge_tcp_stream(
    client: TcpStream,
    remote: TcpStream,
    clt_dec: TrackedStream,
    clt_enc: TrackedStream,
    tg_enc: TrackedStream,
    tg_dec: TrackedStream,
    cancel_token: CancellationToken,
) {
    let (c_read, c_write) = tokio::io::split(client);
    bridge_tcp(
        c_read,
        c_write,
        remote,
        clt_dec,
        clt_enc,
        tg_enc,
        tg_dec,
        cancel_token,
    )
    .await;
}

// ---------------------------------------------------------------------------
// TCP fallback — порт bridge._tcp_fallback
// ---------------------------------------------------------------------------

pub async fn tcp_fallback<R, W>(
    conn_read: R,
    conn_write: W,
    dc: i32,
    dst: &str,
    port: u16,
    init: &[u8],
    label: String,
    ctx_streams: (TrackedStream, TrackedStream, TrackedStream, TrackedStream),
    cancel_token: CancellationToken,
) -> bool
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let addr = match dst.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{}:{}", dst, port),
    };
    let tcp_started = Instant::now();
    let mut remote =
        match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr)).await {
            Ok(Ok(r)) => {
                crate::balancer::BALANCER.write().record_success(
                    dc, "tcp_fallback", &addr, tcp_started.elapsed()
                );
                record_diagnostic(&format!("tcp_fallback_connect_ok duration_ms={}", tcp_started.elapsed().as_millis()));
                r
            }
            Ok(Err(e)) => {
                crate::balancer::BALANCER.write().record_failure(dc, "tcp_fallback", &addr);
                record_diagnostic(&format!("tcp_fallback_connect_fail reason={:?}", e.kind()));
                lwarn!("[{}] TCP fallback to {}:{} failed: {}", label, dst, port, e);
                return false;
            }
            Err(_) => {
                crate::balancer::BALANCER.write().record_failure(dc, "tcp_fallback", &addr);
                record_diagnostic("tcp_fallback_connect_fail reason=timeout");
                lwarn!("[{}] TCP fallback to {}:{} failed: timeout", label, dst, port);
                return false;
            }
        };
    let _ = remote.set_nodelay(true);

    STATS.connections_tcp_fallback.fetch_add(1, Ordering::Relaxed);
    if remote.write_all(init).await.is_err() {
        record_diagnostic("tcp_fallback_init_write_fail");
        return false;
    }
    let (clt_dec, clt_enc, tg_enc, tg_dec) = ctx_streams;
    bridge_tcp(
        conn_read,
        conn_write,
        remote,
        clt_dec,
        clt_enc,
        tg_enc,
        tg_dec,
        cancel_token,
    )
    .await;
    true
}

// ---------------------------------------------------------------------------
// Cfproxy fallback — мобильная версия (параллель + 429 cooldown + DoH),
// путь и secure как в оригинале.
// ---------------------------------------------------------------------------

async fn try_cfproxy_base_domain(
    dc: i32,
    base_domain: &str,
    ws_path: &str,
    cancel_token: &CancellationToken,
) -> (Option<RawWebSocket>, String, Option<RouteObservation>) {
    let base_domain = normalize_cf_domain(base_domain);
    if base_domain.is_empty() {
        return (None, String::new(), None);
    }
    let remaining = cfproxy_429_cooldown_remaining(&base_domain);
    if remaining > Duration::ZERO {
        ldebug!(
            " CF skip {}: 429 cooldown {:.0}s",
            base_domain,
            remaining.as_secs_f64().ceil()
        );
        return (None, String::new(), None);
    }
    let _permit = match acquire_cfproxy_attempt_slot().await {
        Some(p) => p,
        None => return (None, String::new(), None),
    };
    let attempt_kind = match crate::balancer::BALANCER
        .write()
        .begin_attempt(dc, "cf_proxy", &base_domain)
    {
        Some(kind) => kind,
        None => {
            record_diagnostic("route_circuit_skip kind=cf_proxy");
            return (None, String::new(), None);
        }
    };

    let domain = format!("kws{}.{}", dc, base_domain);
    ldebug!(" CF try {}", domain);
    let started = Instant::now();
    let result = tokio::select! {
        _ = cancel_token.cancelled() => {
            crate::balancer::BALANCER.write().cancel_attempt(
                dc, "cf_proxy", &base_domain, attempt_kind
            );
            return (None, String::new(), None);
        }
        result = cf_connect_domain(&domain, ws_path, 5.0) => result,
    };
    let (ws, resolved_ip, err) = result;
    if let Some(e) = err {
        if matches!(e, WsError::Canceled) {
            crate::balancer::BALANCER.write().cancel_attempt(
                dc, "cf_proxy", &base_domain, attempt_kind
            );
        } else {
            crate::balancer::BALANCER.write().record_attempt_failure(
                dc, "cf_proxy", &base_domain, attempt_kind
            );
        }
        let reason = match &e {
            WsError::Timeout => "timeout",
            WsError::Canceled => "canceled",
            WsError::Handshake(_) => "handshake",
            WsError::Io(_) => "io",
            WsError::Other(_) => "other",
        };
        record_diagnostic(&format!("cf_connect_fail dc={} reason={}", dc, reason));
        if is_http_status_error(&e, 429) {
            mark_cfproxy_429_cooldown(&base_domain, &e);
        }
        if !resolved_ip.is_empty() {
            log_cf_conn_error(
                &format!(" CF fail {} via {}: {}", domain, resolved_ip, e.compact()),
                &e,
            );
        } else {
            log_cf_conn_error(&format!(" CF fail {}: {}", domain, e.compact()), &e);
        }
        return (None, String::new(), None);
    }

    crate::balancer::BALANCER.write().record_connect_success(
        dc, "cf_proxy", &base_domain, started.elapsed(), attempt_kind
    );
    clear_cfproxy_429_cooldown(&base_domain);
    record_diagnostic(&format!("cf_connect_ok dc={} connect_ms={}", dc, started.elapsed().as_millis()));
    if !resolved_ip.is_empty() {
        ldebug!(" CF ok {} via {}", domain, resolved_ip);
    } else {
        ldebug!(" CF ok {} via hostname", domain);
    }
    let observation = RouteObservation {
        dc_id: dc,
        route_kind: "cf_proxy".to_string(),
        route: base_domain.clone(),
        attempt_kind,
        connected_at: started,
    };
    (ws, base_domain, Some(observation))
}

// Только устанавливает WS-соединение через CF, НЕ трогая conn.
async fn cfproxy_acquire_ws(
    dc: i32,
    is_media: bool,
    ws_path: &str,
    cancel_token: &CancellationToken,
) -> Option<(RawWebSocket, String, RouteObservation)> {
    let (enabled, domains) = {
        let cfg = CFPROXY.read();
        (
            CFPROXY_ENABLED.load(Ordering::Relaxed),
            cfg.domains.clone(),
        )
    };
    if !enabled || domains.is_empty() {
        return None;
    }

    let candidates = crate::balancer::BALANCER.read().get_domains_for_dc(dc);
    let ordered = crate::balancer::BALANCER
        .read()
        .rank_candidates(dc, "cf_proxy", &candidates);
    if ordered.is_empty() {
        return None;
    }

    let m_tag = if is_media { " media" } else { "" };
    ldebug!(" CF fallback DC{}{}: {} домен(ов)", dc, m_tag, ordered.len());

    let mut ws: Option<RawWebSocket> = None;
    let mut chosen_domain = String::new();
    let mut chosen_observation: Option<RouteObservation> = None;

    let sem = Arc::new(tokio::sync::Semaphore::new(
        CFPROXY_FALLBACK_PARALLEL.max(1).min(ordered.len()),
    ));
    let mut attempts = JoinSet::new();
    let mut attempt_cancellations = Vec::new();
    for base_domain in ordered.into_iter().filter(|d| !d.is_empty()) {
        let sem = sem.clone();
        let cancel = cancel_token.child_token();
        attempt_cancellations.push(cancel.clone());
        let path = ws_path.to_string();
        attempts.spawn(async move {
            let _permit = tokio::select! {
                _ = cancel.cancelled() => return None,
                result = sem.acquire_owned() => result.ok()?,
            };
            let (candidate, domain, observation) =
                try_cfproxy_base_domain(dc, &base_domain, &path, &cancel).await;
            candidate.zip(observation).map(|(socket, observation)| {
                (socket, domain, observation)
            })
        });
    }

    let mut canceled = false;
    let mut winner_found = false;
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                canceled = true;
                for token in &attempt_cancellations { token.cancel(); }
                break;
            }
            result = attempts.join_next() => {
                match result {
                    Some(Ok(Some((candidate, domain, observation)))) => {
                        ws = Some(candidate);
                        chosen_domain = domain;
                        chosen_observation = Some(observation);
                        winner_found = true;
                        for token in &attempt_cancellations { token.cancel(); }
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    if !winner_found && !canceled {
        for token in &attempt_cancellations { token.cancel(); }
    }
    while let Some(result) = attempts.join_next().await {
        if let Ok(Some((extra, _, observation))) = result {
            crate::balancer::BALANCER.write().release_probe(
                observation.dc_id,
                &observation.route_kind,
                &observation.route,
                observation.attempt_kind,
            );
            extra.close().await;
        }
    }
    if canceled {
        if let Some(socket) = ws.take() {
            socket.close().await;
        }
        if let Some(observation) = chosen_observation.take() {
            crate::balancer::BALANCER.write().release_probe(
                observation.dc_id,
                &observation.route_kind,
                &observation.route,
                observation.attempt_kind,
            );
        }
        return None;
    }

    match (ws, chosen_observation) {
        (Some(w), Some(observation)) => {
            if !chosen_domain.is_empty()
                && crate::balancer::BALANCER.write().update_domain_for_dc(dc, &chosen_domain)
            {
                linfo!(" CF домен для DC{} -> {}", dc, chosen_domain);
            }
            Some((w, chosen_domain, observation))
        }
        (Some(w), None) => {
            w.close().await;
            None
        }
        _ => {
            lwarn!(" CF fallback DC{}{}: все CF домены недоступны", dc, m_tag);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// doFallback — порт bridge.do_fallback: cf_worker -> cf -> tcp.
// CF не "съедает" conn при провале: тот же conn уходит в TCP fallback.
// halves передаются дальше только в успешную ветвь: worker-ws и cf-ws
// добываются БЕЗ halves, затем halves уходят в bridge/tcp.
// ---------------------------------------------------------------------------

pub async fn do_fallback<R, W>(
    pools: &ProxyPools,
    conn_read: R,
    conn_write: W,
    relay_init: &[u8],
    label: String,
    dc: i32,
    is_test: bool,
    is_media: bool,
    splitter: Option<MsgSplitter>,
    clt_dec: &TrackedStream,
    clt_enc: &TrackedStream,
    tg_enc: &TrackedStream,
    tg_dec: &TrackedStream,
    cancel_token: CancellationToken,
) -> bool
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let m_tag = if is_media { " media" } else { "" };
    let fallback_dst = resolve_fallback_target(dc, is_media, is_test);
    let use_cf = CFPROXY_ENABLED.load(Ordering::Relaxed) && !is_test;
    let worker_domains = CF_WORKER_DOMAINS.read().clone();
    let ws_path = ws_path_for(is_test);
    let secure_port = if DISABLE_SECURE.load(Ordering::Relaxed) { 80 } else { 443 };
    let use_worker = !worker_domains.is_empty() && !fallback_dst.is_empty() && !is_test;

    // Шаг 1: worker-ws (без halves) — порт _cfproxy_worker_fallback.
    if use_worker {
        record_diagnostic(&format!("route_attempt kind=cf_worker dc={} media={}", dc, is_media as u8));
        let pooled = pools
            .cf_worker
            .get(dc, fallback_dst.clone(), worker_domains.clone())
            .await;
        let mut worker_observation: Option<RouteObservation> = None;
        let worker_ws = if let Some((ws, worker_domain)) = pooled {
            let attempt_kind = crate::balancer::BALANCER
                .write()
                .begin_attempt(dc, "cf_worker", &worker_domain);
            if let Some(attempt_kind) = attempt_kind {
                worker_observation = Some(RouteObservation {
                    dc_id: dc,
                    route_kind: "cf_worker".to_string(),
                    route: worker_domain.clone(),
                    attempt_kind,
                    connected_at: Instant::now(),
                });
                linfo!(
                    "[{}] DC{}{} -> CF worker pool hit via {} for {}",
                    label, dc, m_tag, worker_domain, fallback_dst
                );
                Some(ws)
            } else {
                ws.close().await;
                record_diagnostic("route_circuit_skip kind=cf_worker pool=1");
                None
            }
        } else {
            let secure = !DISABLE_SECURE.load(Ordering::Relaxed);
            let query = format!("dst={}&dc={}", fallback_dst, dc);
            let path = format!("/apiws?{}", query);
            let mut found = None;
            for worker_domain in pools.cf_worker.available_domains(dc, worker_domains.clone()).await {
                let attempt_kind = match crate::balancer::BALANCER
                    .write()
                    .begin_attempt(dc, "cf_worker", &worker_domain)
                {
                    Some(kind) => kind,
                    None => continue,
                };
                let _dial_permit = match try_direct_ws_dial_slot() {
                    Some(permit) => permit,
                    None => {
                        crate::balancer::BALANCER.write().cancel_attempt(
                            dc, "cf_worker", &worker_domain, attempt_kind
                        );
                        record_diagnostic("direct_ws_dial_budget_saturated kind=cf_worker");
                        continue;
                    }
                };
                linfo!(
                    "[{}] DC{}{} -> trying CF worker {} for {}",
                    label, dc, m_tag, worker_domain, fallback_dst
                );
                let route_started = Instant::now();
                let attempt = tokio::select! {
                    _ = cancel_token.cancelled() => {
                        crate::balancer::BALANCER.write().cancel_attempt(
                            dc, "cf_worker", &worker_domain, attempt_kind
                        );
                        return false;
                    }
                    result = ws_connect_full_opts(
                        &worker_domain,
                        &worker_domain,
                        &path,
                        10.0,
                        None,
                        Some(secure),
                    ) => result,
                };
                match &attempt {
                    Ok(_) => crate::balancer::BALANCER.write().record_connect_success(
                        dc, "cf_worker", &worker_domain, route_started.elapsed(), attempt_kind
                    ),
                    Err(WsError::Canceled) => crate::balancer::BALANCER.write().cancel_attempt(
                        dc, "cf_worker", &worker_domain, attempt_kind
                    ),
                    Err(_) => crate::balancer::BALANCER.write().record_attempt_failure(
                        dc, "cf_worker", &worker_domain, attempt_kind
                    ),
                }
                match attempt {
                    Ok(ws) => {
                        worker_observation = Some(RouteObservation {
                            dc_id: dc,
                            route_kind: "cf_worker".to_string(),
                            route: worker_domain.clone(),
                            attempt_kind,
                            connected_at: route_started,
                        });
                        found = Some(ws);
                        break;
                    }
                    Err(exc) => {
                        pools.cf_worker.report_failure(&worker_domain, &exc).await;
                        lwarn!(
                            "[{}] DC{}{} CF worker {} failed: {}",
                            label, dc, m_tag, worker_domain, exc.compact()
                        );
                    }
                }
            }
            found
        };
        if let Some(ws) = worker_ws {
            STATS.connections_cfproxy.fetch_add(1, Ordering::Relaxed);
            if ws.send(relay_init).await.is_ok() {
                bridge_ws(
                    conn_read,
                    conn_write,
                    ws,
                    label,
                    dc,
                    is_media,
                    None,
                    clt_dec.clone_state(),
                    clt_enc.clone_state(),
                    tg_enc.clone_state(),
                    tg_dec.clone_state(),
                    cancel_token,
                    worker_observation,
                )
                .await;
                return true;
            }
            if let Some(observation) = worker_observation.take() {
                crate::balancer::BALANCER.write().record_early_failure(
                    observation.dc_id, &observation.route_kind, &observation.route
                );
            }
            ws.close().await;
        }
    }

    // Шаг 2: cf-ws (без halves).
    if use_cf {
        record_diagnostic(&format!("route_attempt kind=cf_proxy dc={} media={}", dc, is_media as u8));
        if let Some((ws, _chosen_domain, observation)) =
            cfproxy_acquire_ws(dc, is_media, ws_path, &cancel_token).await
        {
            STATS.connections_cfproxy.fetch_add(1, Ordering::Relaxed);
            linfo!(" DC{}{} подключен через CF", dc, m_tag);

            if ws.send(relay_init).await.is_ok() {
                bridge_ws(
                    conn_read,
                    conn_write,
                    ws,
                    label,
                    dc,
                    is_media,
                    splitter,
                    clt_dec.clone_state(),
                    clt_enc.clone_state(),
                    tg_enc.clone_state(),
                    tg_dec.clone_state(),
                    cancel_token,
                    Some(observation),
                )
                .await;
                return true;
            }
            crate::balancer::BALANCER.write().record_early_failure(
                observation.dc_id, &observation.route_kind, &observation.route
            );
            ws.close().await;
            // CF умер сразу после хендшейка — пробуем TCP на том же conn.
        }
        // CF не дал ws — halves НЕ тронуты, идём в TCP fallback ниже.
    }

    // Шаг 3: tcp.
    record_diagnostic(&format!("route_attempt kind=tcp_fallback dc={} media={}", dc, is_media as u8));
    if !fallback_dst.is_empty() {
        linfo!(
            "[{}] DC{}{} -> TCP fallback to {}:{}",
            label, dc, m_tag, fallback_dst, secure_port
        );
        return tcp_fallback(
            conn_read,
            conn_write,
            dc,
            &fallback_dst,
            secure_port,
            relay_init,
            label,
            (
                clt_dec.clone_state(),
                clt_enc.clone_state(),
                tg_enc.clone_state(),
                tg_dec.clone_state(),
            ),
            cancel_token,
        )
        .await;
    }

    false
}

// ---------------------------------------------------------------------------
// Client serving — порт tg_ws_proxy._handle_client (dd + ee/FakeTLS).
// ---------------------------------------------------------------------------

async fn read_proxy_protocol_line(
    conn: &mut TcpStream,
    cancel_token: &CancellationToken,
) -> Option<String> {
    let mut line = Vec::with_capacity(108);
    let mut byte = [0u8; 1];
    let res = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = tokio::select! {
                _ = cancel_token.cancelled() => return Err(()),
                result = conn.read(&mut byte) => result.map_err(|_| ())?,
            };
            if n == 0 {
                return Err(());
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 108 {
                break;
            }
        }
        Ok::<(), ()>(())
    })
    .await;
    match res {
        Ok(Ok(())) => Some(String::from_utf8_lossy(&line).to_string()),
        _ => None,
    }
}

pub async fn handle_client(
    pools: Arc<ProxyPools>,
    mut conn: TcpStream,
    cancel_token: CancellationToken,
) {
    STATS.connections_total.fetch_add(1, Ordering::Relaxed);
    STATS.connections_active.fetch_add(1, Ordering::Relaxed);
    struct ActiveGuard;
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            // run_proxy joins/aborts every client task before STATS.reset(), so
            // this decrement always belongs to the same service generation.
            STATS.connections_active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _guard = ActiveGuard;

    let peer = conn
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut label = peer;

    let _ = conn.set_nodelay(true);

    let current_secret = PROXY_SECRET.read().clone();
    let secret_bytes = hex::decode(&current_secret).unwrap_or_default();

    // PROXY protocol (proxy_config.proxy_protocol в оригинале).
    if PROXY_PROTOCOL.load(Ordering::Relaxed) {
        match read_proxy_protocol_line(&mut conn, &cancel_token).await {
            Some(line) => {
                let text = line.trim().to_string();
                if text.starts_with("PROXY ") {
                    let parts: Vec<&str> = text.split_whitespace().collect();
                    if parts.len() >= 6 {
                        label = format!("{}:{}", parts[2], parts[4]);
                    }
                    ldebug!("[{}] PROXY protocol: {}", label, text);
                } else {
                    ldebug!("[{}] expected PROXY header, got: {:?}", label, &text[..text.len().min(60)]);
                }
            }
            None => {
                ldebug!("[{}] disconnected during PROXY header", label);
                return;
            }
        }
    }

    // FakeTLS (ee-secret): первый байт решает всё — как _read_client_init.
    let masking = FAKE_TLS_DOMAIN.read().clone();
    if !masking.is_empty() {
        let mut first = [0u8; 1];
        let first_read = tokio::select! {
            _ = cancel_token.cancelled() => return,
            result = tokio::time::timeout(Duration::from_secs(10), conn.read_exact(&mut first)) => result,
        };
        match first_read {
            Ok(Ok(_)) => {}
            _ => {
                ldebug!("[{}] client disconnected before handshake", label);
                return;
            }
        }
        if first[0] == faketls::TLS_RECORD_HANDSHAKE {
            let handshake_result = tokio::select! {
                _ = cancel_token.cancelled() => return,
                result = faketls::server_handshake(conn, first[0], &secret_bytes, &masking, &label) => result,
            };
            match handshake_result {
                Some(plain) => {
                    serve_core(
                        &pools,
                        plain.read,
                        plain.write,
                        label,
                        secret_bytes,
                        cancel_token,
                    )
                    .await;
                }
                None => {}
            }
            return;
        } else {
            ldebug!("[{}] non-TLS byte 0x{:02X} -> HTTP redirect", label, first[0]);
            let redirect = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                masking
            );
            let _ = conn.write_all(redirect.as_bytes()).await;
            return;
        }
    }

    let (conn_read, conn_write) = tokio::io::split(conn);
    serve_core(
        &pools,
        conn_read,
        conn_write,
        label,
        secret_bytes,
        cancel_token,
    )
    .await;
}

/// Ядро обслуживания соединения: 64-байтный handshake → маршрут → bridge.
/// Generic над транспортом (TcpStream-halves или FakeTLS-duplex-halves).
async fn serve_core<R, W>(
    pools: &ProxyPools,
    mut conn_read: R,
    mut conn_write: W,
    label: String,
    secret_bytes: Vec<u8>,
    cancel_token: CancellationToken,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    // 64-байтный handshake
    let mut handshake = [0u8; 64];
    let handshake_result = tokio::select! {
        _ = cancel_token.cancelled() => return,
        result = tokio::time::timeout(Duration::from_secs(10), conn_read.read_exact(&mut handshake)) => result,
    };
    match handshake_result {
        Ok(Ok(_)) => {}
        _ => {
            ldebug!("[{}] client disconnected before handshake", label);
            return;
        }
    }

    if is_http_transport(&handshake) {
        STATS.connections_http_reject.fetch_add(1, Ordering::Relaxed);
        let _ = conn_write
            .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
            .await;
        let _ = conn_write.shutdown().await;
        return;
    }

    // _try_handshake: key = SHA256(prekey + secret).
    let clt_dec_prekey = &handshake[SKIP_LEN..SKIP_LEN + PREKEY_LEN];
    let clt_dec_iv = &handshake[SKIP_LEN + PREKEY_LEN..SKIP_LEN + PREKEY_LEN + IV_LEN];
    let mut hash_dec = Sha256::new();
    hash_dec.update(clt_dec_prekey);
    hash_dec.update(&secret_bytes);
    let mut clt_decryptor = new_aes_ctr(&hash_dec.finalize(), clt_dec_iv);

    let mut decrypted = handshake;
    clt_decryptor.xor(&mut decrypted);

    let proto_tag = &decrypted[PROTO_TAG_POS..PROTO_TAG_POS + 4];
    let proto = LittleEndian::read_u32(proto_tag);
    if !valid_proto(proto) {
        STATS.connections_bad.fetch_add(1, Ordering::Relaxed);
        lwarn!("[{}] bad handshake (wrong secret or proto)", label);
        return;
    }

    let dc_raw = LittleEndian::read_u16(&decrypted[DC_IDX_POS..DC_IDX_POS + 2]) as i16;
    let mut dc = dc_raw as i32;
    if dc < 0 {
        dc = -dc;
    }
    let is_media = dc_raw < 0;

    // Test DC: proxy_config.force_test_dc или dc >= 10000.
    let mut is_test_dc = FORCE_TEST_DC.load(Ordering::Relaxed) || dc >= 10000;
    if dc >= 10000 {
        linfo!("[{}] test DC{} -> DC{}", label, dc, dc - 10000);
        dc -= 10000;
    }
    // force_test_dc тянет весь трафик на тестовые IP/путь.
    if FORCE_TEST_DC.load(Ordering::Relaxed) {
        is_test_dc = true;
    }
    let m_tag = if is_media { " media" } else { "" };

    let proto_int = proto; // u32 tag; MsgSplitter маппит через proto_tag_to_type
    let dc_idx = if is_media { -dc } else { dc };

    ldebug!(
        "[{}] handshake ok: DC{}{} proto=0x{:08X}{}",
        label,
        dc,
        m_tag,
        proto_int,
        if is_test_dc { " test" } else { "" }
    );

    // relayInit генерация (1-в-1 c Go + RESERVED из utils.py).
    let mut relay_init = [0u8; 64];
    loop {
        rand::thread_rng().fill_bytes(&mut relay_init);
        if relay_init[0] == RESERVED_FIRST_BYTE {
            continue;
        }
        if is_reserved_start(&relay_init[..4]) {
            continue;
        }
        if relay_init[4] == 0 && relay_init[5] == 0 && relay_init[6] == 0 && relay_init[7] == 0 {
            continue;
        }
        break;
    }

    let mut tg_dec_prekey_and_iv = [0u8; 48];
    for i in 0..48 {
        tg_dec_prekey_and_iv[i] = relay_init[8 + 47 - i];
    }

    let mut tg_encryptor = new_aes_ctr(&relay_init[8..40], &relay_init[40..56]);
    let tg_decryptor = new_aes_ctr(&tg_dec_prekey_and_iv[..32], &tg_dec_prekey_and_iv[32..]);

    let mut dc_bytes = [0u8; 2];
    LittleEndian::write_u16(&mut dc_bytes, dc_idx as u16);

    let mut tail_plain = [0u8; 8];
    tail_plain[0..4].copy_from_slice(proto_tag);
    tail_plain[4..6].copy_from_slice(&dc_bytes);
    rand::thread_rng().fill_bytes(&mut tail_plain[6..8]);

    let mut encrypted_full = relay_init;
    tg_encryptor.xor(&mut encrypted_full);

    let mut keystream_tail = [0u8; 8];
    for i in 0..8 {
        keystream_tail[i] = encrypted_full[56 + i] ^ relay_init[56 + i];
        relay_init[56 + i] = tail_plain[i] ^ keystream_tail[i];
    }

    // clt_encryptor: prekey/iv = reverse(client_dec_prekey_iv), key=SHA256+secret.
    let mut clt_enc_prekey_and_iv = [0u8; 48];
    for i in 0..48 {
        clt_enc_prekey_and_iv[i] = handshake[8 + 47 - i];
    }
    let mut hash_enc = Sha256::new();
    hash_enc.update(&clt_enc_prekey_and_iv[..32]);
    hash_enc.update(&secret_bytes);
    let clt_encryptor = new_aes_ctr(&hash_enc.finalize(), &clt_enc_prekey_and_iv[32..]);

    let dc_key = DcKey::new(dc, is_media, is_test_dc);
    let now = now_monotonic();
    let ws_path = ws_path_for(is_test_dc);

    let target_opt = resolve_configured_target(dc, is_media);
    let dc_configured = target_opt.is_some();
    let target = target_opt.unwrap_or_default();

    let blacklisted = WS_BLACKLIST.read().get(&dc_key).copied().unwrap_or(false);
    let ip_cooldown_active = if target.is_empty() {
        false
    } else {
        IP_FAIL_UNTIL.read().get(&target).map(|until| now < *until).unwrap_or(false)
    };
    let is_any_cf_fallback =
        CFPROXY_ENABLED.load(Ordering::Relaxed) || !CF_WORKER_DOMAINS.read().is_empty();

    // Fallback если DC нет в конфиге, WS в чёрном списке или IP на cooldown —
    // порт ветки из _handle_client оригинала.
    let mut pooled_ws: Option<RawWebSocket> = None;
    let mut pooled_observation: Option<RouteObservation> = None;
    let mut use_pooled_direct = false;
    if !dc_configured || blacklisted || (ip_cooldown_active && is_any_cf_fallback) {
        if !dc_configured {
            linfo!("[{}] DC{} not in config -> fallback", label, dc);
        } else if blacklisted {
            linfo!("[{}] DC{}{} WS blacklisted -> fallback", label, dc, m_tag);
        } else {
            // Шанс пулу при случайном таймауте: пробуем забрать готовый WS.
            if !is_test_dc {
                let domains = ws_domains(dc, is_media);
                if let Some((ws, route)) = pools
                    .ws
                    .get(dc, is_media, target.clone(), domains)
                    .await
                {
                    if let Some(attempt_kind) = crate::balancer::BALANCER
                        .write()
                        .begin_attempt(dc, "telegram_ws", &route)
                    {
                        pooled_observation = Some(RouteObservation {
                            dc_id: dc,
                            route_kind: "telegram_ws".to_string(),
                            route,
                            attempt_kind,
                            connected_at: Instant::now(),
                        });
                        pooled_ws = Some(ws);
                    } else {
                        ws.close().await;
                    }
                }
            }
            if pooled_ws.is_none() {
                linfo!(
                    "[{}] DC{}{} IP {} is in cooldown -> fallback",
                    label, dc, m_tag, target
                );
            } else {
                linfo!(
                    "[{}] DC{}{} WS connect to {} was timed out, but pool hit -> using WS",
                    label, dc, m_tag, target
                );
                use_pooled_direct = true;
            }
        }

        if !use_pooled_direct {
            let splitter = MsgSplitter::new(&relay_init, proto_int);
            let ok = do_fallback(
                pools,
                conn_read,
                conn_write,
                &relay_init,
                label.clone(),
                dc,
                is_test_dc,
                is_media,
                splitter,
                &clt_decryptor,
                &clt_encryptor,
                &tg_encryptor,
                &tg_decryptor,
                cancel_token,
            )
            .await;
            if !ok {
                lwarn!("[{}] DC{}{} no fallback available", label, dc, m_tag);
            }
            return;
        }
    }

    let fail_until = DC_FAIL_UNTIL.read().get(&dc_key).copied().unwrap_or(0.0);
    let ws_timeout = if now < fail_until {
        WS_FAIL_TIMEOUT
    } else {
        WS_DIRECT_TIMEOUT
    };

    let direct_candidates = ws_domains(dc, is_media);
    let domains = crate::balancer::BALANCER
        .read()
        .rank_candidates(dc, "telegram_ws", &direct_candidates);
    // pooled_ws уже может держать WS из cooldown-ветки.
    let mut ws_opt = pooled_ws;
    let mut route_observation: Option<RouteObservation> = pooled_observation;
    let mut ws_failed_redirect = false;
    let mut ws_timed_out = false;
    let mut all_ws_attempts_timed_out = true;
    let mut all_redirects = true;

    if ws_opt.is_none() && !is_test_dc {
        if let Some((ws, route)) = pools
            .ws
            .get(dc, is_media, target.clone(), domains.clone())
            .await
        {
            if let Some(attempt_kind) = crate::balancer::BALANCER
                .write()
                .begin_attempt(dc, "telegram_ws", &route)
            {
                route_observation = Some(RouteObservation {
                    dc_id: dc,
                    route_kind: "telegram_ws".to_string(),
                    route,
                    attempt_kind,
                    connected_at: Instant::now(),
                });
                ws_opt = Some(ws);
                linfo!("[{}] DC{}{} -> pool hit via {}", label, dc, m_tag, target);
            } else {
                ws.close().await;
            }
        }
    }

    if ws_opt.is_none() {
        for domain in &domains {
            let url = format!("wss://{}{} via {}", domain, ws_path, target);
            linfo!("[{}] DC{}{} -> {}", label, dc, m_tag, url);
            let attempt_kind = match crate::balancer::BALANCER
                .write()
                .begin_attempt(dc, "telegram_ws", domain)
            {
                Some(kind) => kind,
                None => {
                    record_diagnostic(&format!(
                        "route_circuit_skip dc={} kind=telegram_ws",
                        dc
                    ));
                    continue;
                }
            };
            let _dial_permit = match try_direct_ws_dial_slot() {
                Some(permit) => permit,
                None => {
                    crate::balancer::BALANCER.write().cancel_attempt(
                        dc, "telegram_ws", domain, attempt_kind
                    );
                    record_diagnostic("direct_ws_dial_budget_saturated");
                    continue;
                }
            };
            let route_started = Instant::now();
            let attempt = ws_connect_full_opts(&target, domain, ws_path, ws_timeout, None, None).await;
            match &attempt {
                Ok(_) => crate::balancer::BALANCER.write().record_connect_success(
                    dc, "telegram_ws", domain, route_started.elapsed(), attempt_kind
                ),
                Err(WsError::Canceled) => crate::balancer::BALANCER.write().cancel_attempt(
                    dc, "telegram_ws", domain, attempt_kind
                ),
                Err(_) => crate::balancer::BALANCER.write().record_attempt_failure(
                    dc, "telegram_ws", domain, attempt_kind
                ),
            }
            match attempt {
                Ok(ws) => {
                    route_observation = Some(RouteObservation {
                        dc_id: dc,
                        route_kind: "telegram_ws".to_string(),
                        route: domain.clone(),
                        attempt_kind,
                        connected_at: route_started,
                    });
                    record_diagnostic(&format!("ws_direct_connect_ok dc={} media={}", dc, is_media as u8));
                    all_redirects = false;
                    ws_opt = Some(ws);
                    break;
                }
                Err(WsError::Handshake(h)) if h.is_redirect() => {
                    all_ws_attempts_timed_out = false;
                    record_diagnostic(&format!("ws_direct_connect_fail dc={} media={} reason=redirect status={}", dc, is_media as u8, h.status_code));
                    STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                    ws_failed_redirect = true;
                    lwarn!(
                        "[{}] DC{}{} got {} from {} -> {}",
                        label,
                        dc,
                        m_tag,
                        h.status_code,
                        domain,
                        if h.location.is_empty() { "?" } else { &h.location }
                    );
                    continue;
                }
                Err(WsError::Handshake(h)) => {
                    all_ws_attempts_timed_out = false;
                    record_diagnostic(&format!("ws_direct_connect_fail dc={} media={} reason=handshake status={}", dc, is_media as u8, h.status_code));
                    STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                    all_redirects = false;
                    lwarn!(
                        "[{}] DC{}{} WS handshake: {}",
                        label, dc, m_tag, h.status_line
                    );
                    continue;
                }
                Err(WsError::Timeout) => {
                    record_diagnostic(&format!("ws_direct_connect_fail dc={} media={} reason=timeout", dc, is_media as u8));
                    STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                    ws_timed_out = true;
                    // A timeout is not evidence that all Telegram WS hostnames
                    // redirect. Continue to the alternate domain and prevent
                    // the DC from being permanently classified as redirect-only.
                    all_redirects = false;
                    lwarn!(
                        "[{}] DC{}{} WS connect timed out via {}",
                        label, dc, m_tag, domain
                    );
                    continue;
                }
                Err(exc) => {
                    all_ws_attempts_timed_out = false;
                    record_diagnostic(&format!("ws_direct_connect_fail dc={} media={} reason={}", dc, is_media as u8, match &exc { WsError::Canceled => "canceled", WsError::Io(_) => "io", WsError::Timeout => "timeout", WsError::Handshake(_) => "handshake", WsError::Other(_) => "other" }));
                    STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                    all_redirects = false;
                    lwarn!(
                        "[{}] DC{}{} WS connect failed: {}",
                        label, dc, m_tag, exc.compact()
                    );
                    continue;
                }
            }
        }
    } else {
        all_redirects = false;
        all_ws_attempts_timed_out = false;
    }

    // WS failed -> fallback (порт оригинала 1-в-1).
    if ws_opt.is_none() {
        if ws_timed_out && all_ws_attempts_timed_out && !target.is_empty() {
            IP_FAIL_UNTIL
                .write()
                .insert(target.clone(), now + IP_FAIL_COOLDOWN);
            linfo!(
                "[{}] DC{}{} WS connect to {} timed out, cooldown for {}s",
                label, dc, m_tag, target, IP_FAIL_COOLDOWN as i64
            );
        }

        if ws_failed_redirect && all_redirects {
            WS_BLACKLIST.write().insert(dc_key, true);
            lwarn!("[{}] DC{}{} blacklisted for WS (all 302)", label, dc, m_tag);
        } else if ws_failed_redirect {
            DC_FAIL_UNTIL.write().insert(dc_key, now + DC_FAIL_COOLDOWN);
        } else {
            DC_FAIL_UNTIL.write().insert(dc_key, now + DC_FAIL_COOLDOWN);
            linfo!(
                "[{}] DC{}{} WS failed for {}s",
                label, dc, m_tag, DC_FAIL_COOLDOWN as i64
            );
        }

        let splitter_fb = MsgSplitter::new(&relay_init, proto_int);
        let ok = do_fallback(
            pools,
            conn_read,
            conn_write,
            &relay_init,
            label.clone(),
            dc,
            is_test_dc,
            is_media,
            splitter_fb,
            &clt_decryptor,
            &clt_encryptor,
            &tg_encryptor,
            &tg_decryptor,
            cancel_token,
        )
        .await;
        if ok {
            linfo!("[{}] DC{}{} fallback closed", label, dc, m_tag);
        }
        return;
    }

    if !target.is_empty() {
        IP_FAIL_UNTIL.write().remove(&target);
    }
    pools.ws.report_success(dc, is_media).await;
    STATS.connections_ws.fetch_add(1, Ordering::Relaxed);

    let splitter = MsgSplitter::new(&relay_init, proto_int);
    if splitter.is_some() {
        ldebug!(
            "[{}] MsgSplitter activated for proto 0x{:08X}",
            label, proto_int
        );
    }

    // send direct init (с retry на свежий WS как мобильное улучшение).
    let mut ws = ws_opt.take().unwrap();
    let mut send_ok = ws.send(&relay_init).await.is_ok();
    if send_ok {
        ldebug!(" direct relayInit sent DC{}{}", dc, m_tag);
    } else {
        lwarn!(" direct relayInit write fail DC{}{}: closed", dc, m_tag);
        ws.close().await;
        if let Some(observation) = route_observation.take() {
            crate::balancer::BALANCER.write().record_early_failure(
                observation.dc_id, &observation.route_kind, &observation.route
            );
        }

        DC_FAIL_UNTIL.write().insert(dc_key, now + DC_FAIL_COOLDOWN);

        lwarn!(" direct retry fresh ws DC{}{}", dc, m_tag);
        let (retry_ws, retry_failed_redirect, retry_all_redirects, retry_observation) =
            connect_direct_ws(dc, &target, &domains, ws_timeout, ws_path, &cancel_token).await;
        match retry_ws {
            None => {
                if retry_failed_redirect && retry_all_redirects {
                    WS_BLACKLIST.write().insert(dc_key, true);
                    lwarn!(" DC{}{} заблокирован (302)", dc, m_tag);
                }
                lwarn!(" direct fallback DC{}{}", dc, m_tag);
                let splitter_fb = MsgSplitter::new(&relay_init, proto_int);
                do_fallback(
                    pools,
                    conn_read,
                    conn_write,
                    &relay_init,
                    label,
                    dc,
                    is_test_dc,
                    is_media,
                    splitter_fb,
                    &clt_decryptor,
                    &clt_encryptor,
                    &tg_encryptor,
                    &tg_decryptor,
                    cancel_token,
                )
                .await;
                return;
            }
            Some(rws) => {
                route_observation = retry_observation;
                if rws.send(&relay_init).await.is_err() {
                    if let Some(observation) = route_observation.take() {
                        crate::balancer::BALANCER.write().record_early_failure(
                            observation.dc_id, &observation.route_kind, &observation.route
                        );
                    }
                    lwarn!(" direct relayInit write fail DC{}{}: closed", dc, m_tag);
                    rws.close().await;
                    lwarn!(" direct fallback DC{}{}", dc, m_tag);
                    let splitter_fb = MsgSplitter::new(&relay_init, proto_int);
                    do_fallback(
                        pools,
                        conn_read,
                        conn_write,
                        &relay_init,
                        label,
                        dc,
                        is_test_dc,
                        is_media,
                        splitter_fb,
                        &clt_decryptor,
                        &clt_encryptor,
                        &tg_encryptor,
                        &tg_decryptor,
                        cancel_token,
                    )
                    .await;
                    return;
                }
                ws = rws;
                send_ok = true;
            }
        }
    }
    let _ = send_ok;

    DC_FAIL_UNTIL.write().remove(&dc_key);
    bridge_ws(
        conn_read,
        conn_write,
        ws,
        label,
        dc,
        is_media,
        splitter,
        clt_decryptor,
        clt_encryptor,
        tg_encryptor,
        tg_decryptor,
        cancel_token,
        route_observation,
    )
    .await;
}

// connectDirectWS — прямой перебор доменов (тихий, для retry-пути).
pub async fn connect_direct_ws(
    dc: i32,
    target: &str,
    domains: &[String],
    timeout: f64,
    ws_path: &str,
    cancel_token: &CancellationToken,
) -> (
    Option<RawWebSocket>,
    bool,
    bool,
    Option<RouteObservation>,
) {
    if domains.is_empty() {
        return (None, false, false, None);
    }
    let candidates = crate::balancer::BALANCER
        .read()
        .rank_candidates(dc, "telegram_ws", domains);
    let mut ws_failed_redirect = false;
    let mut all_redirects = true;

    for dom in &candidates {
        let attempt_kind = match crate::balancer::BALANCER
            .write()
            .begin_attempt(dc, "telegram_ws", dom)
        {
            Some(kind) => kind,
            None => continue,
        };
        let _dial_permit = match try_direct_ws_dial_slot() {
            Some(permit) => permit,
            None => {
                crate::balancer::BALANCER.write().cancel_attempt(
                    dc, "telegram_ws", dom, attempt_kind
                );
                continue;
            }
        };
        let started = Instant::now();
        let attempt = tokio::select! {
            _ = cancel_token.cancelled() => {
                crate::balancer::BALANCER.write().cancel_attempt(
                    dc, "telegram_ws", dom, attempt_kind
                );
                return (None, ws_failed_redirect, false, None);
            }
            result = ws_connect_full_opts(target, dom, ws_path, timeout, None, None) => result,
        };
        match &attempt {
            Ok(_) => crate::balancer::BALANCER.write().record_connect_success(
                dc, "telegram_ws", dom, started.elapsed(), attempt_kind
            ),
            Err(WsError::Canceled) => crate::balancer::BALANCER.write().cancel_attempt(
                dc, "telegram_ws", dom, attempt_kind
            ),
            Err(_) => crate::balancer::BALANCER.write().record_attempt_failure(
                dc, "telegram_ws", dom, attempt_kind
            ),
        }
        match attempt {
            Ok(ws) => {
                let observation = RouteObservation {
                    dc_id: dc,
                    route_kind: "telegram_ws".to_string(),
                    route: dom.clone(),
                    attempt_kind,
                    connected_at: started,
                };
                return (Some(ws), ws_failed_redirect, false, Some(observation));
            }
            Err(WsError::Handshake(h)) => {
                STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                if h.is_redirect() {
                    ws_failed_redirect = true;
                } else {
                    all_redirects = false;
                }
            }
            Err(WsError::Timeout) => {
                STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                all_redirects = false;
            }
            Err(WsError::Canceled) => {
                return (None, ws_failed_redirect, false, None);
            }
            Err(_) => {
                STATS.ws_errors.fetch_add(1, Ordering::Relaxed);
                all_redirects = false;
            }
        }
    }
    (None, ws_failed_redirect, all_redirects, None)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run_proxy(
    pools: Arc<ProxyPools>,
    host: String,
    port: u16,
    dc_opt_map: HashMap<i32, String>,
    cancel_root: CancellationToken,
    listener: TcpListener,
) -> std::io::Result<()> {
    const MAX_CLIENT_CONNECTIONS: usize = 128;

    {
        let mut m = DC_OPT.write();
        *m = dc_opt_map.clone();
    }

    let refresh_handle = start_cfproxy_refresh(cancel_root.clone());

    {
        let p = pools.clone();
        let map = dc_opt_map.clone();
        let cancel = cancel_root.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = p.warmup(&map) => {},
            }
        });
    }

    linfo!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    linfo!("  TG WS Proxy запущен");
    linfo!("  Адрес: {}:{}", host, port);
    linfo!("  Target DC IPs:");
    let mut dcs: Vec<i32> = dc_opt_map.keys().copied().collect();
    dcs.sort();
    for dc in dcs {
        if let Some(ip) = dc_opt_map.get(&dc) {
            linfo!("    DC{}: {}", dc, ip);
        }
    }
    if CFPROXY_ENABLED.load(Ordering::Relaxed) {
        linfo!("  CF proxy:      enabled");
    }
    if !CF_WORKER_DOMAINS.read().is_empty() {
        linfo!(
            "  CF worker:     enabled ({})",
            CF_WORKER_DOMAINS.read().join(", ")
        );
    }
    if DISABLE_SECURE.load(Ordering::Relaxed) {
        linfo!("  No secure:     enabled (port 80 for CF proxy/worker)");
    }
    if !FAKE_TLS_DOMAIN.read().is_empty() {
        linfo!("  Fake TLS:      {}", FAKE_TLS_DOMAIN.read().clone());
    }

    let cancel_stats = cancel_root.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel_stats.cancelled() => return,
                _ = interval.tick() => {
                    linfo!(" {}", STATS.summary_ru());
                }
            }
        }
    });

    let mut clients = JoinSet::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => crate::mark_listener_heartbeat(),
            _ = cancel_root.cancelled() => break,
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result {
                    ldebug!("client task ended with join error: {}", error);
                }
            }
            accept = listener.accept() => {
                match accept {
                    Ok((conn, peer)) => {
                        if clients.len() >= MAX_CLIENT_CONNECTIONS {
                            lwarn!("rejecting client {}: connection limit {} reached", peer, MAX_CLIENT_CONNECTIONS);
                            drop(conn);
                            continue;
                        }
                        let p = pools.clone();
                        let cancel = cancel_root.child_token();
                        clients.spawn(async move {
                            handle_client(p, conn, cancel).await;
                        });
                    }
                    Err(error) => {
                        lwarn!("TCP accept failed: {}", error);
                        tokio::select! {
                            _ = cancel_root.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                }
            }
        }
    }

    drop(listener);
    cancel_root.cancel();

    // Give cancellation-aware handshakes/bridges a short grace period. Anything
    // still blocked is aborted and joined before the caller resets global stats.
    let drained = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(result) = clients.join_next().await {
            if let Err(error) = result {
                ldebug!("client task ended during shutdown: {}", error);
            }
        }
    })
    .await;
    if drained.is_err() {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }

    let _ = tokio::time::timeout(Duration::from_secs(1), refresh_handle).await;
    pools.close_all().await;
    Ok(())
}

pub fn parse_cidr_pool(cidrs_str: &str) -> HashMap<i32, String> {
    let mut result = HashMap::new();
    if cidrs_str.trim().is_empty() {
        return result;
    }
    for pair in cidrs_str.split(',') {
        // Split only once: IPv6 literals contain additional colons.
        if let Some((dc_raw, ip_raw)) = pair.split_once(':') {
            let dc_raw = dc_raw.trim();
            let ip_raw = ip_raw.trim().trim_start_matches('[').trim_end_matches(']');
            if let Ok(dc) = dc_raw.parse::<i32>() {
                if !ip_raw.is_empty() {
                    if let Ok(ip) = ip_raw.parse::<std::net::IpAddr>() {
                        result.insert(dc, ip.to_string());
                    }
                }
            }
        }
    }
    result
}


#[cfg(test)]
mod ws_pool_generation_tests {
    use super::*;

    #[tokio::test]
    async fn ws_idle_pool_has_a_global_budget() {
        let pool = WsPool::new(CancellationToken::new());
        assert_eq!(pool.budget_available(), MAX_TOTAL_WS_POOL_CONNECTIONS);
    }

    #[tokio::test]
    async fn reset_invalidates_refills_started_by_the_previous_network_generation() {
        let pool = Arc::new(WsPool::new(CancellationToken::new()));
        let old_generation = pool.generation.load(Ordering::Acquire);
        pool.reset().await;
        assert_eq!(pool.generation.load(Ordering::Acquire), old_generation + 1);

        pool.schedule_refill_generation(
            DcSlot { dc: 2, is_media: 0 },
            "149.154.167.51".to_string(),
            vec!["kws2.web.telegram.org".to_string()],
            old_generation,
        )
        .await;

        assert!(pool.slots.lock().await.is_empty());
        assert_eq!(pool.idle_count().await, 0);
    }
}

#[cfg(test)]
mod cf_worker_pool_tests {
    use super::*;
    use crate::ws::WsHandshakeError;

    #[tokio::test]
    async fn worker_domain_normalization_deduplicates_hosts() {
        let pool = CfWorkerPool::new(CancellationToken::new());
        let domains = pool
            .available_domains(2, vec![
                "Worker.Example.".to_string(),
                "worker.example".to_string(),
                "invalid".to_string(),
            ])
            .await;
        assert_eq!(domains, vec!["worker.example".to_string()]);
    }

    #[tokio::test]
    async fn worker_429_applies_cooldown_to_workers_dev_or_custom_domains() {
        let pool = CfWorkerPool::new(CancellationToken::new());
        let error = WsError::Handshake(WsHandshakeError {
            status_code: 429,
            status_line: "HTTP/1.1 429 Too Many Requests".to_string(),
            headers: HashMap::new(),
            location: String::new(),
        });
        pool.report_failure("WORKER.example.", &error).await;
        let available = pool
            .available_domains(2, vec!["worker.example".to_string()])
            .await;
        assert!(available.is_empty());
    }

    #[tokio::test]
    async fn worker_idle_pool_has_a_global_budget() {
        let pool = CfWorkerPool::new(CancellationToken::new());
        assert_eq!(pool.budget_available(), MAX_TOTAL_CF_WORKER_POOL_CONNECTIONS);
    }

    #[tokio::test]
    async fn worker_pool_reset_advances_generation() {
        let pool = CfWorkerPool::new(CancellationToken::new());
        let before = pool.generation.load(Ordering::Acquire);
        pool.reset().await;
        assert_eq!(pool.generation.load(Ordering::Acquire), before + 1);
    }
}

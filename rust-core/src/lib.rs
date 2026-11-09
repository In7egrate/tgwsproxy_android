pub mod cfproxy;
pub mod config;
pub mod crypto;
pub mod faketls;
pub mod proxy;
pub mod ws;
pub mod balancer;

use config::*;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use proxy::{parse_cidr_pool, run_proxy, ProxyPools};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

// Глобальный рантайм — никогда не дропается
static RUNTIME: OnceCell<Runtime> = OnceCell::new();

struct ProxyState {
    pools: Arc<ProxyPools>,
    handle: tokio::task::JoinHandle<()>,
    monitor_handle: tokio::task::JoinHandle<()>,
    cancel_tasks: CancellationToken,
}

static STATE: OnceCell<Mutex<Option<ProxyState>>> = OnceCell::new();
static LAST_HEARTBEAT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_LISTENER_HEARTBEAT_MS: AtomicU64 = AtomicU64::new(0);

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn mark_listener_heartbeat() {
    LAST_LISTENER_HEARTBEAT_MS.store(epoch_millis(), Ordering::Release);
}

fn state_cell() -> &'static Mutex<Option<ProxyState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        // Многопоточный рантайм, кол-во воркеров адекватно мобиле
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(std::thread::available_parallelism()
                .map(|n| n.get()).unwrap_or(2).clamp(2, 4))
            .thread_name("tgwsproxy-rt")
            .enable_all()
            .build()
            .expect("failed to build global tokio runtime")
    })
}

fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/// # Safety
/// Указатели должны быть валидными C-строками (или null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn StartProxy(
    c_host: *const c_char,
    port: c_int,
    c_dc_ips: *const c_char,
    c_secret: *const c_char,
    verbose: c_int,
) -> c_int {
    let cell = state_cell();
    let mut guard = cell.lock();

    if guard.is_some() {
        return -1;
    }

    let host = cstr_to_string(c_host);
    let go_port = port as u16;
    let dc_ips_str = cstr_to_string(c_dc_ips);
    let secret_str = cstr_to_string(c_secret);
    let is_verbose = verbose != 0;

    init_logging(is_verbose);
    cfproxy::clear_cfproxy_429_cooldowns();

    if secret_str.len() == 32 {
        if hex::decode(&secret_str).is_ok() {
            *PROXY_SECRET.write() = secret_str.clone();
        }
    }

    cfproxy::init_cfproxy_domains();

    let dc_opt_map: HashMap<i32, String> = parse_cidr_pool(&dc_ips_str);

    let rt = runtime();
    let cancel_tasks = CancellationToken::new();
    let lag_cancel = cancel_tasks.clone();
    LAST_HEARTBEAT_MS.store(epoch_millis(), Ordering::Release);
    LAST_LISTENER_HEARTBEAT_MS.store(epoch_millis(), Ordering::Release);
    let monitor_handle = rt.spawn(async move {
        use std::time::{Duration, Instant};
        loop {
            let target = Instant::now() + Duration::from_secs(1);
            tokio::select! {
                _ = lag_cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let lag = Instant::now().saturating_duration_since(target).as_millis() as i64;
                    STATS.event_loop_lag_ms.store(lag, Ordering::Relaxed);
                    LAST_HEARTBEAT_MS.store(epoch_millis(), Ordering::Release);
                }
            }
        }
    });
    let pools = Arc::new(ProxyPools::new(cancel_tasks.clone()));

    // Канал готовности: ждём успешного bind перед возвратом
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let pools_task = pools.clone();
    let host_task = host.clone();
    let map_task = dc_opt_map.clone();
    let cancel_root = cancel_tasks.clone();

    let handle = rt.spawn(async move {
        // Предварительный bind для сигнала готовности
        let addr = format!("{}:{}", host_task, go_port);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                let _ = tx.send(Ok(()));
                if let Err(e) =
                    run_proxy(pools_task, host_task, go_port, map_task, cancel_root, listener).await
                {
                    lerror!("listen on {}: {}", addr, e);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("listen on {}: {}", addr, e)));
            }
        }
    });

    // Ждём результат bind
    match rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            cancel_tasks.cancel();
            monitor_handle.abort();
            handle.abort();
            LAST_HEARTBEAT_MS.store(0, Ordering::Release);
            LAST_LISTENER_HEARTBEAT_MS.store(0, Ordering::Release);
            return -3;
        }
        Err(_) => {
            cancel_tasks.cancel();
            monitor_handle.abort();
            handle.abort();
            LAST_HEARTBEAT_MS.store(0, Ordering::Release);
            LAST_LISTENER_HEARTBEAT_MS.store(0, Ordering::Release);
            return -3;
        }
    }

    *guard = Some(ProxyState {
        pools,
        handle,
        monitor_handle,
        cancel_tasks,
    });

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn StopProxy() -> c_int {
    let cell = state_cell();
    let mut guard = cell.lock();

    let state = match guard.take() {
        Some(s) => s,
        None => return -1,
    };

    // Graceful shutdown — cancel first, then join the listener/client tasks
    // before resetting counters. Dropping a timed-out JoinHandle would detach it.
    linfo!("StopProxy: cancelling all tasks");
    state.cancel_tasks.cancel();

    let rt = runtime();
    let ProxyState { pools, handle, monitor_handle, .. } = state;
    let mut proxy_handle = handle;
    let mut monitor_handle = monitor_handle;
    rt.block_on(async move {
        linfo!("StopProxy: waiting for proxy tasks to finish (max 3s)");
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut proxy_handle)
            .await
            .is_err()
        {
            proxy_handle.abort();
            let _ = proxy_handle.await;
        }
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut monitor_handle)
            .await
            .is_err()
        {
            monitor_handle.abort();
            let _ = monitor_handle.await;
        }
        linfo!("StopProxy: closing pool connections");
        pools.close_all().await;
        pools.reset().await;
        linfo!("StopProxy: done");
    });

    LAST_HEARTBEAT_MS.store(0, Ordering::Release);
    LAST_LISTENER_HEARTBEAT_MS.store(0, Ordering::Release);
    STATS.reset();
    WS_BLACKLIST.write().clear();
    DC_FAIL_UNTIL.write().clear();
    IP_FAIL_UNTIL.write().clear();
    cfproxy::clear_cfproxy_429_cooldowns();
    proxy::set_ws_pool_fronting_first(true);

    linfo!("StopProxy: exit");
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn SetPoolSize(size: c_int) {
    // Оригинал: pool_size = max(0, N); 0 отключает преконнект пула.
    let mut n = size;
    if n < 0 {
        n = 0;
    }
    if n > 16 {
        n = 16;
    }
    POOL_SIZE.store(n, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetBufferSizeKb(kb: c_int) {
    // proxy --buf-kb: размер SO_RCVBUF/SO_SNDBUF, минимум 4KB.
    let mut n = kb;
    if n < 4 {
        n = 4;
    }
    if n > 4096 {
        n = 4096;
    }
    BUFFER_SIZE.store(n * 1024, Ordering::Relaxed);
    RECV_BUF.store(n * 1024, Ordering::Relaxed);
    SEND_BUF.store(n * 1024, Ordering::Relaxed);
}

/// # Safety
/// `c_cache_dir` — валидная C-строка или null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfProxyCacheDir(c_cache_dir: *const c_char) {
    let dir = cstr_to_string(c_cache_dir);
    CFPROXY.write().cache_dir = dir.trim().to_string();
}

/// # Safety
/// `c_user_domain` — валидная C-строка или null (можно списком через , ; пробел).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfProxyConfig(
    enabled: c_int,
    _priority: c_int,
    c_user_domain: *const c_char,
) {
    CFPROXY_ENABLED.store(enabled != 0, Ordering::Relaxed);
    let user_domain = cstr_to_string(c_user_domain);
    // Поддержка нескольких доменов как --cfproxy-domain (repeatable).
    let list = coerce_domain_list_str(&user_domain);
    let mut cfg = CFPROXY.write();
    cfg.user_domain = user_domain.trim().to_string();
    if !list.is_empty() {
        cfg.domains = list.clone();
        cfg.active = list[0].clone();
        crate::balancer::BALANCER
            .write()
            .update_domains_list(&cfg.domains);
    }
}

/// # Safety
/// `c_domains` — валидная C-строка или null (--cfproxy-worker-domain, repeatable).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfWorkerDomains(c_domains: *const c_char) {
    let raw = cstr_to_string(c_domains);
    let domains = normalize_domain_pool(&coerce_domain_list_str(&raw));
    *CF_WORKER_DOMAINS.write() = domains;
}

/// # Safety
/// `c_domain` — валидная C-строка или null (--fake-tls-domain, ee-secret).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetFakeTlsDomain(c_domain: *const c_char) {
    let d = cstr_to_string(c_domain);
    *FAKE_TLS_DOMAIN.write() = d.trim().to_string();
}

#[unsafe(no_mangle)]
pub extern "C" fn SetDisableSecure(v: c_int) {
    // --no-secure: порт 80 для CF proxy/worker.
    DISABLE_SECURE.store(v != 0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetForceTestDc(v: c_int) {
    // --force-test-dc: весь трафик на тестовые DC.
    FORCE_TEST_DC.store(v != 0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetProxyProtocol(v: c_int) {
    // --proxy-protocol: принимать PROXY protocol v1.
    PROXY_PROTOCOL.store(v != 0, Ordering::Relaxed);
}

/// # Safety
/// `c_secret` — валидная C-строка или null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetSecret(c_secret: *const c_char) {
    let s = cstr_to_string(c_secret);
    if s.len() != 32 {
        return;
    }
    if hex::decode(&s).is_err() {
        return;
    }
    *PROXY_SECRET.write() = s;
}

#[unsafe(no_mangle)]
pub extern "C" fn GetStats() -> *mut c_char {
    let s = STATS.summary();
    CString::new(s).unwrap_or_default().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn GetSecretWithPrefix() -> *mut c_char {
    // ee-secret при включённом FakeTLS: ee{secret}{domain_hex} — как в оригинале.
    let sec = PROXY_SECRET.read().clone();
    let ftls = FAKE_TLS_DOMAIN.read().clone();
    let full = if ftls.is_empty() {
        format!("dd{}", sec)
    } else {
        format!("ee{}{}", sec, ftls.as_bytes().iter().map(|b| format!("{:02x}", b)).collect::<String>())
    };
    CString::new(full).unwrap_or_default().into_raw()
}

/// # Safety
/// `p` должен быть указателем, ранее возвращённым из этой библиотеки.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn FreeString(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(p);
    }
}

// Diagnostic APIs added for the Kotlin memory-aware controller.
#[unsafe(no_mangle)]
pub extern "C" fn ResetRouteFailures() {
    // A network handover invalidates the RTT/failure history measured on the
    // previous network, while preserving the configured domain list.
    crate::balancer::BALANCER.write().reset_route_metrics();
    WS_BLACKLIST.write().clear();
    DC_FAIL_UNTIL.write().clear();
    IP_FAIL_UNTIL.write().clear();
    cfproxy::clear_cfproxy_429_cooldowns();
    let maybe = {
        let guard = state_cell().lock();
        guard.as_ref().map(|state| state.pools.clone())
    };
    if let Some(pools) = maybe {
        let map = DC_OPT.read().clone();
        runtime().spawn(async move {
            pools.close_all().await;
            pools.reset().await;
            pools.warmup(&map).await;
        });
    }
}

/// Returns 1 only while the native listener task and its runtime heartbeat are alive.
#[unsafe(no_mangle)]
pub extern "C" fn GetProxyHealth() -> c_int {
    let guard = state_cell().lock();
    let state = match guard.as_ref() {
        Some(state) => state,
        None => return 0,
    };
    if state.handle.is_finished() || state.monitor_handle.is_finished() {
        return 0;
    }
    let now = epoch_millis();
    let runtime_heartbeat = LAST_HEARTBEAT_MS.load(Ordering::Acquire);
    let listener_heartbeat = LAST_LISTENER_HEARTBEAT_MS.load(Ordering::Acquire);
    if runtime_heartbeat == 0
        || listener_heartbeat == 0
        || now.saturating_sub(runtime_heartbeat) > 5_000
        || now.saturating_sub(listener_heartbeat) > 5_000
    {
        return 0;
    }
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn GetStatsJson() -> *mut c_char {
    use serde_json::json;
    let n = |v: &std::sync::atomic::AtomicI64| v.load(Ordering::Relaxed);
    let pool_budget = {
        let guard = state_cell().lock();
        guard.as_ref().map(|state| state.pools.budget_snapshot())
    };
    let stats = json!({
        "total": n(&STATS.connections_total),
        "active": n(&STATS.connections_active),
        "ws": n(&STATS.connections_ws),
        "tcp_fallback": n(&STATS.connections_tcp_fallback),
        "cfproxy": n(&STATS.connections_cfproxy),
        "ws_errors": n(&STATS.ws_errors),
        "bytes_up": n(&STATS.bytes_up),
        "bytes_down": n(&STATS.bytes_down),
        "media_bytes_up": n(&STATS.media_bytes_up),
        "media_bytes_down": n(&STATS.media_bytes_down),
        "pool_hits": n(&STATS.pool_hits),
        "pool_misses": n(&STATS.pool_misses),
        "cf_pool_hits": n(&STATS.cf_pool_hits),
        "cf_pool_misses": n(&STATS.cf_pool_misses),
        "tcp_connect_ms": n(&STATS.tcp_connect_ms),
        "tls_handshake_ms": n(&STATS.tls_handshake_ms),
        "ws_handshake_ms": n(&STATS.ws_handshake_ms),
        "ws_setup_last_ms": n(&STATS.ws_setup_last_ms),
        "ws_setup_count": n(&STATS.ws_setup_count),
        "ws_ping_rtt_ms": n(&STATS.ws_ping_rtt_ms),
        "ws_ping_samples": n(&STATS.ws_ping_samples),
        "ws_ping_timeouts": n(&STATS.ws_ping_timeouts),
        "ws_setup_avg_ms": if n(&STATS.ws_setup_count) == 0 { 0 }
            else { n(&STATS.ws_setup_total_ms) / n(&STATS.ws_setup_count) },
        "pool_config": POOL_SIZE.load(Ordering::Relaxed),
        "pool_budget": pool_budget.map(|(ws_available, worker_available)| json!({
            "ws_idle_available": ws_available,
            "cf_worker_idle_available": worker_available,
        })),
        "route_metrics": crate::balancer::BALANCER.read().metrics_snapshot(),
        "buffer_size": BUFFER_SIZE.load(Ordering::Relaxed),
        "event_loop_lag_ms": n(&STATS.event_loop_lag_ms),
        "recent_events": diagnostics_snapshot(),
    });
    CString::new(stats.to_string()).unwrap_or_default().into_raw()
}

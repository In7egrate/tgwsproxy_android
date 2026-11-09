package com.amurcanov.tgwsproxy

import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONObject

object ProxyRuntime {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var monitor: Job? = null
    private val running = MutableStateFlow(false)
    val isRunning: StateFlow<Boolean> = running
    private val verified = MutableStateFlow(false)
    val isVerifiedRunning: StateFlow<Boolean> = verified
    private val statistics = MutableStateFlow("{}")
    val statsJson: StateFlow<String> = statistics

    @Synchronized fun startMonitoring(context: Context) {
        if (monitor?.isActive == true) return
        val app = context.applicationContext
        monitor = scope.launch {
            val adaptive = AutoPoolController()
            var nextAuto = System.currentTimeMillis() + 60_000L
            var nextLog = System.currentTimeMillis() + 15_000L
            var lastNativeSeq = 0L
            var previousRunning: Boolean? = null
            while (isActive) {
                val active = ProxyService.isRunning
                running.value = active
                if (previousRunning != active) {
                    DiagnosticStore.record(app, "service_state running=$active")
                    previousRunning = active
                }
                val nativeHealthy = active && runCatching {
                    JSONObject(RustBridge.status(app)).optString("state") == "running"
                }.getOrDefault(false)
                if (!isActive) break
                verified.value = nativeHealthy
                val raw = if (nativeHealthy) runCatching { RustBridge.stats(app) }
                    .getOrDefault("{}") else "{}"
                if (!isActive) break
                statistics.value = raw
                val stats = runCatching { JSONObject(raw) }.getOrDefault(JSONObject())
                val events = stats.optJSONArray("recent_events")
                if (events != null) {
                    for (i in 0 until events.length()) {
                        val event = events.optJSONObject(i) ?: continue
                        val seq = event.optLong("seq", 0L)
                        if (seq > lastNativeSeq) {
                            DiagnosticStore.record(app, "native_event ${event.toString()}")
                            lastNativeSeq = seq
                        }
                    }
                }
                val now = System.currentTimeMillis()
                if (verified.value && now >= nextAuto && AutoProxyProfile.isAuto(app)) {
                    nextAuto = now + 60_000L
                    val s = stats
                    val selected = adaptive.tick(app, s.optLong("pool_hits"), s.optLong("pool_misses"),
                        s.optLong("ws_errors"), s.optInt("active"))
                    RustBridge.setPoolSize(selected)
                }
                if (verified.value && now >= nextLog) {
                    nextLog = now + 15_000L
                    DiagnosticStore.record(app, "metrics active=${stats.optInt("active")}" +
                        " ws=${stats.optLong("ws")} errors=${stats.optLong("ws_errors")}" +
                        " pool=${stats.optLong("pool_hits")}/${stats.optLong("pool_misses")}" +
                        " cf_pool=${stats.optLong("cf_pool_hits")}/${stats.optLong("cf_pool_misses")}" +
                        " tcp=${stats.optLong("tcp_connect_ms")}" +
                        " tls=${stats.optLong("tls_handshake_ms")}" +
                        " ws_setup=${stats.optLong("ws_setup_last_ms")}" +
                        " ws_ping_rtt_ms=${stats.optLong("ws_ping_rtt_ms")}" +
                        " ws_ping_samples=${stats.optLong("ws_ping_samples")}" +
                        " ws_ping_timeouts=${stats.optLong("ws_ping_timeouts")}" +
                        " loop_lag=${stats.optLong("event_loop_lag_ms")}")
                }
                delay(2000L)
            }
        }
    }

    @Synchronized fun stopMonitoring() {
        monitor?.cancel()
        monitor = null
        running.value = false
        verified.value = false
        statistics.value = "{}"
    }
}

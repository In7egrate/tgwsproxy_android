package com.amurcanov.tgwsproxy

import android.app.ActivityManager
import android.content.Context
import android.os.Debug

/** Auto-mode limits memory; network counters decide whether to expand the WS pool. */
object AutoProxyProfile {
    private const val KEY = "simple_settings_auto"
    private const val GIB = 1024L * 1024 * 1024

    fun isAuto(context: Context) = context.getSharedPreferences(ProxyConfigStore.PREFS, 0)
        .getBoolean(KEY, true)

    fun setMode(context: Context, on: Boolean) {
        context.getSharedPreferences(ProxyConfigStore.PREFS, 0).edit().putBoolean(KEY, on).apply()
    }

    fun choose(context: Context): AutoParameters {
        val am = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val mem = ActivityManager.MemoryInfo().also { am.getMemoryInfo(it) }
        val low = mem.lowMemory || mem.availMem < mem.threshold * 2
        val buffer = when {
            low || mem.totalMem < 4 * GIB -> 128
            mem.totalMem >= 12 * GIB && mem.availMem > 2 * GIB -> 512
            else -> 256
        }
        val maxPool = if (low || mem.totalMem < 4 * GIB) 6 else if (mem.totalMem < 8 * GIB) 8 else 12
        val logMb = if (context.filesDir.usableSpace < 1 * GIB) 2 else 5
        return AutoParameters(6, buffer, logMb, maxPool, low)
    }

    fun apply(context: Context): String? {
        if (!isAuto(context)) return null
        val cfg = ProxyConfigStore.load(context)
        val prefs = context.getSharedPreferences(ProxyConfigStore.PREFS, 0)
        if (!prefs.getBoolean("v3_manual_snapshot_saved", false)) {
            prefs.edit()
                .putInt("v3_manual_port", cfg.port)
                .putInt("v3_manual_pool", cfg.poolSize.coerceAtLeast(6))
                .putInt("v3_manual_buffer", cfg.bufKb)
                .putInt("v3_manual_log", cfg.logMaxMb)
                .putBoolean("v3_manual_snapshot_saved", true)
                .apply()
        }
        val p = choose(context)
        // Auto mode does not use a saved manual port: the agreed default is 1443.
        return ProxyConfigStore.save(context, cfg.host, "1443", cfg.secret,
            cfg.dcIp.joinToString("\n"), p.poolSize.toString(),
            p.bufKb.toString(), p.logMaxMb.toString(), false,
            cfg.cfproxy, cfg.cfproxyUserDomainEnabled,
            cfg.cfproxyUserDomains.joinToString(","), cfg.cfWorkerEnabled,
            cfg.cfWorkerDomains.joinToString(","), cfg.forceTestDc)
    }
}

data class AutoParameters(val poolSize: Int, val bufKb: Int, val logMaxMb: Int,
                          val maxPool: Int, val lowMemory: Boolean)

/** Incremental counters + hysteresis; only increases if there is useful load. */
class AutoPoolController {
    private var prevHits = 0L
    private var prevMisses = 0L
    private var prevErrors = 0L
    private var previous = false
    private var stableWindows = 0
    private var current = 6

    fun tick(context: Context, hits: Long, misses: Long, errors: Long, active: Int): Int {
        if (!AutoProxyProfile.isAuto(context)) return current
        val p = AutoProxyProfile.choose(context)
        if (!previous) {
            prevHits = hits; prevMisses = misses; prevErrors = errors
            previous = true
            current = 6
            return current
        }
        val dh = (hits - prevHits).coerceAtLeast(0)
        val dm = (misses - prevMisses).coerceAtLeast(0)
        val de = (errors - prevErrors).coerceAtLeast(0)
        prevHits = hits; prevMisses = misses; prevErrors = errors
        val attempts = dh + dm
        var wanted = current
        if (p.lowMemory || p.maxPool < current) {
            wanted = 6
            stableWindows = 0
        } else if (active > 0 && attempts >= 12 && de <= 2 && dm * 100 >= attempts * 30) {
            stableWindows++
            if (stableWindows >= 3) {
                wanted = (current + 2).coerceAtMost(p.maxPool)
                stableWindows = 0
            }
        } else {
            stableWindows = 0
            if (attempts == 0L && active == 0 && current > 6) wanted = (current - 2).coerceAtLeast(6)
        }
        current = wanted.coerceIn(6, 12)
        return current
    }
}

package com.amurcanov.tgwsproxy

import android.content.Context
import android.util.Log
import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.Pointer
import org.json.JSONObject

/** C ABI of the pinned amurcanov Rust core, with the v3 telemetry extension. */
interface RustApi : Library {
    fun StartProxy(host: String, port: Int, dcIps: String, secret: String, verbose: Int): Int
    fun StopProxy(): Int
    fun SetPoolSize(value: Int)
    fun SetBufferSizeKb(value: Int)
    fun SetCfProxyCacheDir(path: String)
    fun SetCfProxyConfig(enabled: Int, priority: Int, domains: String)
    fun SetCfWorkerDomains(domains: String)
    fun SetFakeTlsDomain(domain: String)
    fun SetForceTestDc(value: Int)
    fun SetProxyProtocol(value: Int)
    fun SetDisableSecure(value: Int)
    fun ResetRouteFailures()
    fun GetStatsJson(): Pointer?
    fun GetProxyHealth(): Int
    fun FreeString(ptr: Pointer)
}

object RustBridge {
    private val native: RustApi by lazy {
        Native.load("tgwsproxy", RustApi::class.java) as RustApi
    }
    @Volatile private var live: Boolean = false
    @Volatile private var failure: String = ""
    private var lastConfig = JSONObject()
    private val startedAt = java.util.concurrent.atomic.AtomicLong(0)

    @Synchronized fun start(context: Context, configJson: String): String {
        if (live) {
            val current = status(context)
            if (JSONObject(current).optString("state") == "running") return current
            // A Kotlin-side `live` flag can outlive a failed native listener.
            // Stop the stale native state before attempting a clean rebind.
            runCatching { native.StopProxy() }
            live = false
            startedAt.set(0)
        }
        return try {
            val cfg = JSONObject(configJson)
            val pool = cfg.optInt("pool_size", 6).coerceIn(6, 16)
            val buffer = cfg.optInt("buf_kb", 256).coerceIn(64, 1024)
            val secret = cfg.optString("secret")
            require(Regex("[0-9a-fA-F]{32}").matches(secret)) { "Неверный MTProto secret" }
            val port = cfg.optInt("port", 1443)
            require(port in 1024..65535) { "Порт должен быть от 1024 до 65535" }
            val dcLines = cfg.optJSONArray("dc_ip")
            val targets = buildList {
                if (dcLines != null) for (i in 0 until dcLines.length()) add(dcLines.optString(i))
            }.filter { it.isNotBlank() }.joinToString(",")
            native.SetPoolSize(pool)
            native.SetBufferSizeKb(buffer)
            native.SetCfProxyCacheDir(context.cacheDir.absolutePath)
            native.SetCfProxyConfig(
                if (cfg.optBoolean("cfproxy", true)) 1 else 0,
                1,
                cfg.optJSONArray("cfproxy_user_domain")?.let { a ->
                    (0 until a.length()).joinToString(",") { a.optString(it) }
                }.orEmpty()
            )
            native.SetCfWorkerDomains(cfg.optJSONArray("cfproxy_worker_domain")?.let { a ->
                (0 until a.length()).joinToString(",") { a.optString(it) }
            }.orEmpty())
            // FakeTLS is intentionally not exposed by Android settings until
            // its ClientHello profile and end-to-end compatibility are verified.
            native.SetFakeTlsDomain("")
            native.SetForceTestDc(if (cfg.optBoolean("force_test_dc", false)) 1 else 0)
            native.SetProxyProtocol(0)
            // HTTPS / WSS by default. No plaintext downgrade.
            native.SetDisableSecure(0)
            val startedNs = System.nanoTime()
            val result = native.StartProxy(cfg.optString("host", "127.0.0.1"), port,
                targets, secret, if (cfg.optBoolean("verbose", false)) 1 else 0)
            if (result != 0) {
                live = false
                failure = "Rust StartProxy returned $result"
            } else {
                live = true
                failure = ""
                startedAt.set(System.currentTimeMillis())
                lastConfig = cfg
                DiagnosticStore.record(context, "rust_bind_ms=" + ((System.nanoTime() - startedNs) / 1_000_000L))
            }
            status(context)
        } catch (t: Throwable) {
            live = false
            failure = t.message ?: t.javaClass.simpleName
            Log.e("TGWS-Rust", "Failed to initialize core", t)
            status(context)
        }
    }

    @Synchronized fun stop(): String {
        if (live) runCatching { native.StopProxy() }
        live = false
        startedAt.set(0)
        return "{\"state\":\"stopped\"}"
    }

    fun status(@Suppress("UNUSED_PARAMETER") context: Context): String {
        val nativeHealthy = live && runCatching { native.GetProxyHealth() == 1 }.getOrDefault(false)
        return JSONObject().apply {
            put("state", when {
                nativeHealthy -> "running"
                live || failure.isNotEmpty() -> "error"
                else -> "stopped"
            })
            put("error", failure.ifEmpty {
                if (live && !nativeHealthy) "Native listener/runtime heartbeat is unhealthy" else ""
            })
            put("native_healthy", nativeHealthy)
            put("uptime", if (live) (System.currentTimeMillis() - startedAt.get()) / 1000 else 0)
        }.toString()
    }

    fun stats(@Suppress("UNUSED_PARAMETER") context: Context): String {
        if (!live) return "{}"
        val ptr = runCatching { native.GetStatsJson() }.getOrNull() ?: return "{}"
        return try { ptr.getString(0, "UTF-8") } finally { native.FreeString(ptr) }
    }

    /** Clear stale IP/DC route backoff after network transitions. */
    fun onNetworkChange(description: String): String {
        if (live) {
            runCatching { native.ResetRouteFailures() }
                .onFailure { Log.w("TGWS-Rust", "Route reset failed: $description", it) }
        }
        return statusPlaceholder()
    }

    private fun statusPlaceholder(): String = if (live) "running" else "stopped"
    fun selftest(context: Context): String = JSONObject().apply {
        put("engine", "Rust / Tokio")
        put("rust_library_loaded", runCatching { native.GetStatsJson()?.let {
            native.FreeString(it); true
        } ?: false }.getOrDefault(false))
        val current = JSONObject(status(context))
        put("state", current.optString("state"))
        put("native_healthy", current.optBoolean("native_healthy"))
    }.toString()
    fun diagnose(context: Context): String = JSONObject().apply {
        put("status", JSONObject(status(context)))
        put("stats", JSONObject(stats(context)))
    }.toString()
    fun dumpStacks(context: Context): String = diagnose(context)

    fun setPoolSize(size: Int) { if (live) native.SetPoolSize(size.coerceIn(6, 12)) }
}

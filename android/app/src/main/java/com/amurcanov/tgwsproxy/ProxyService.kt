package com.amurcanov.tgwsproxy

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.LinkProperties
import android.net.NetworkRequest
import java.net.Inet4Address
import java.net.Inet6Address
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import org.json.JSONObject

/**
 * Foreground-сервис, внутри которого живёт нативное Rust/Tokio-ядро прокси.
 *
 * Почему foreground: прокси держит слушающий сокет и пул WebSocket-соединений.
 * Обычный фоновый процесс Android убьёт за минуты, в Doze — быстрее.
 *
 * Почему тип specialUse: у dataSync на Android 15 суточный лимит 6 часов, для
 * постоянно работающего прокси это не годится; connectedDevice не подходит по
 * смыслу — внешнего устройства нет.
 */
class ProxyService : Service() {

    companion object {
        const val ACTION_START = "com.tgwsproxy.android.START"
        const val ACTION_STOP = "com.tgwsproxy.android.STOP"
        const val ACTION_RESTART = "com.tgwsproxy.android.RESTART"

        private const val TAG = "TgWsProxy"
        private const val CHANNEL_ID = "proxy_status"
        private const val NOTIF_ID = 1
        private const val NOTIF_REFRESH_MS = 5000L

        /**
         * Срок, на который берётся wake lock. Продлевается тикером вдвое чаще,
         * поэтому разрыва не возникает, а система не видит «вечного» лока.
         */
        private const val WAKELOCK_TTL_MS = 10 * 60 * 1000L

        /** Как часто продлевать — вдвое чаще срока, чтобы не было разрыва. */
        private const val WAKELOCK_RENEW_MS = 4 * 60 * 1000L

        /**
         * Пауза между попытками поднять туннель. Проверка живёт в том же
         * тикере, что и уведомление, то есть срабатывает каждые пять секунд;
         * без паузы неудачная попытка повторялась бы двенадцать раз в минуту.
         */
        private const val TUNNEL_RETRY_MS = 60 * 1000L

        @Volatile
        var isRunning: Boolean = false
            private set
    }

    private var wakeLock: PowerManager.WakeLock? = null
    private var worker: HandlerThread? = null
    private var workerHandler: Handler? = null
    private val ui = Handler(android.os.Looper.getMainLooper())
    private var notifTicker: Runnable? = null
    private var netCallback: ConnectivityManager.NetworkCallback? = null
    private var defaultNetwork: Network? = null
    private var lastNetworkFingerprint: String? = null
    private var lastRouteFingerprint: String? = null
    private var lastLinkFingerprint: String? = null
    private var lastWakeLockAcquire = 0L
    private var lastTunnelAttempt = 0L
    private var serviceStartAttempts = 0

    override fun onBind(intent: Intent?): IBinder? = null

    /** Уведомление сервиса должно быть на том же языке, что и интерфейс. */
    override fun attachBaseContext(base: Context) {
        super.attachBaseContext(AppLocale.wrap(base))
    }

    override fun onCreate() {
        super.onCreate()
        worker = HandlerThread("proxy-control").also { it.start() }
        workerHandler = Handler(worker!!.looper)
        registerNetworkCallback()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            ProxyConfigStore.setShouldRun(this, false)
            Restarter.cancelWatchdog(this)
            stopNotifTicker()
            stopForegroundCompat()
            workerHandler?.post {
                stopProxy()
                ProxyRuntime.stopMonitoring()
                ui.post { stopSelf() }
            }
            return START_NOT_STICKY
        }

        val restart = intent?.action == ACTION_RESTART
        // A cold ACTION_RESTART must also satisfy the foreground-service start
        // deadline; previously this branch returned before startForeground().
        startForegroundCompat(buildNotification(getString(R.string.ark_notif_starting)))
        ProxyConfigStore.setShouldRun(this, true)
        Restarter.scheduleWatchdog(this)
        acquireWakeLock()
        ProxyConfigStore.migrateLogIfNeeded(this)
        ProxyRuntime.startMonitoring(applicationContext)

        // Apply the adaptive profile once per service start/restart.
        if (AutoProxyProfile.isAuto(this)) {
            val error = AutoProxyProfile.apply(this)
            if (error != null) Log.w(TAG, "Auto profile not applied: $error")
        }
        val cfg = ProxyConfigStore.load(this)
        workerHandler?.post {
            if (restart) stopProxy()
            startProxy(cfg)
        }
        return START_STICKY
    }

    /**
     * Смахивание карточки из «Недавних» на многих прошивках убивает процесс
     * вместе с foreground-сервисом. Ставим будильник, который поднимет прокси
     * обратно — но только если пользователь не выключал его сам.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        if (ProxyConfigStore.shouldRun(this)) {
            Log.i(TAG, "Задача удалена из «Недавних» — планируем подъём")
            Restarter.scheduleKick(this)
        }
        super.onTaskRemoved(rootIntent)
    }

    private fun startProxy(cfg: ProxyConfig) {
        val startStartedAt = android.os.SystemClock.elapsedRealtime()
        serviceStartAttempts = (serviceStartAttempts + 1).coerceAtMost(Int.MAX_VALUE)
        if (isRunning) {
            val healthy = runCatching {
                JSONObject(RustBridge.status(this)).optString("state") == "running"
            }.getOrDefault(false)
            if (healthy) {
                ui.post {
                    updateNotification(getString(R.string.ark_notif_listening, cfg.host, cfg.port))
                    startNotifTicker()
                }
                return
            }
            Log.w(TAG, "Kotlin service flag is running but native health check failed; restarting core")
            stopProxy()
        }
        DiagnosticStore.record(
            this,
            "service_start_requested attempt=$serviceStartAttempts",
        )
        acquireWakeLock()
        try {
            val status = RustBridge.start(this, cfg.toJson())
            val json = JSONObject(status)
            val state = json.optString("state")
            isRunning = state == "running"

            if (isRunning) {
                DiagnosticStore.record(
                    this,
                    "service_started attempt=$serviceStartAttempts elapsed_ms=${android.os.SystemClock.elapsedRealtime() - startStartedAt}",
                )
                Log.i(TAG, "Прокси запущен на ${cfg.host}:${cfg.port}")
                ui.post { maybeRaiseTunnel() }
                ui.post {
                    updateNotification(
                        getString(R.string.ark_notif_listening, cfg.host, cfg.port)
                    )
                    startNotifTicker()
                }
            } else {
                val err = json.optString("error").ifEmpty { state }
                DiagnosticStore.record(
                    this,
                    "service_start_failed state=${state.take(40)} error_class=${classifyStartError(err, state)} attempt=$serviceStartAttempts elapsed_ms=${android.os.SystemClock.elapsedRealtime() - startStartedAt}",
                )
                Log.e(TAG, "Прокси не запустился: $err")
                releaseWakeLock()
                ui.post { updateNotification(err) }
            }
        } catch (t: Throwable) {
            DiagnosticStore.record(
                this,
                "service_start_exception type=${t.javaClass.simpleName} attempt=$serviceStartAttempts elapsed_ms=${android.os.SystemClock.elapsedRealtime() - startStartedAt}",
            )
            Log.e(TAG, "Ошибка запуска прокси", t)
            isRunning = false
            releaseWakeLock()
            ui.post { updateNotification(t.message ?: "ошибка") }
        }
    }

    private fun stopProxy() {
        DiagnosticStore.record(this, "service_stop_requested")
        try {
            RustBridge.stop()
        } catch (t: Throwable) {
            Log.e(TAG, "Ошибка остановки прокси", t)
        } finally {
            isRunning = false
            releaseWakeLock()
            // Туннель существует только ради работающего прокси: VPN-значок
            // при остановленном прокси вводил бы в заблуждение.
            ui.post { TunnelService.stop(this) }
        }
    }

    override fun onDestroy() {
        stopNotifTicker()
        unregisterNetworkCallback()
        // All native start/stop operations run on one HandlerThread. Queue the
        // final stop behind any pending start/restart instead of racing it from
        // the main thread, then let quitSafely drain that queue.
        val handler = workerHandler
        val thread = worker
        if (handler != null) {
            handler.post { stopProxy() }
            thread?.quitSafely()
        } else {
            stopProxy()
        }
        worker = null
        workerHandler = null
        ProxyRuntime.stopMonitoring()
        super.onDestroy()
    }

    // Смена физического/виртуального сетевого транспорта.

    /**
     * The default-network callback follows the network actually used by this
     * process. Keep only transport/capability metadata: never export IPs,
     * interface names, SSIDs, BSSIDs, or Network.toString().
     */
    private fun registerNetworkCallback() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                defaultNetwork = network
                updateNetworkState(network, "available")
            }

            override fun onLost(network: Network) {
                if (defaultNetwork != network) return
                val active = cm.activeNetwork
                defaultNetwork = active
                updateNetworkState(active, "lost")
            }

            override fun onCapabilitiesChanged(
                network: Network,
                caps: NetworkCapabilities,
            ) {
                if (defaultNetwork != null && defaultNetwork != network) return
                defaultNetwork = network
                updateNetworkState(network, "capabilities", caps)
            }

            override fun onLinkPropertiesChanged(
                network: Network,
                linkProperties: LinkProperties,
            ) {
                if (defaultNetwork != null && defaultNetwork != network) return
                defaultNetwork = network
                updateLinkProperties(network, linkProperties)
            }
        }
        netCallback = cb
        try {
            cm.registerDefaultNetworkCallback(cb)
            val active = cm.activeNetwork
            defaultNetwork = active
            updateNetworkState(active, "initial")
            if (active != null) {
                cm.getLinkProperties(active)?.let { updateLinkProperties(active, it) }
            }
        } catch (t: Throwable) {
            Log.w(TAG, "Не удалось подписаться на смену сети", t)
            netCallback = null
            DiagnosticStore.record(
                this,
                "network_callback_registration_failed type=${t.javaClass.simpleName}",
            )
        }
    }

    private fun unregisterNetworkCallback() {
        val cb = netCallback ?: return
        netCallback = null
        try {
            val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
            cm.unregisterNetworkCallback(cb)
        } catch (t: Throwable) {
            Log.w(TAG, "Не удалось отписаться от смены сети", t)
        }
    }

    private fun updateNetworkState(
        network: Network?,
        reason: String,
        suppliedCapabilities: NetworkCapabilities? = null,
    ) {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val caps = suppliedCapabilities ?: network?.let { cm.getNetworkCapabilities(it) }
        val transports = mutableListOf<String>()
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true) transports += "cellular"
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true) transports += "wifi"
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true) transports += "vpn"
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) == true) transports += "ethernet"
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_BLUETOOTH) == true) transports += "bluetooth"

        val transport = transports.joinToString("+").ifEmpty { "unknown" }
        val internet = caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true
        val validated = caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
        val metered = caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) != true
        val notRoaming = if (Build.VERSION.SDK_INT >= 28) {
            caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_ROAMING) == true
        } else {
            null
        }
        val vpn = caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true
        val descriptor = buildString {
            append("transport=").append(transport)
            append(" internet=").append(if (internet) 1 else 0)
            append(" validated=").append(if (validated) 1 else 0)
            append(" metered=").append(if (metered) 1 else 0)
            append(" vpn=").append(if (vpn) 1 else 0)
            if (notRoaming != null) append(" not_roaming=").append(if (notRoaming) 1 else 0)
            if (vpn) append(" underlying=not_exposed")
        }

        // Keep the full capability fingerprint for diagnostics, but reset
        // route pools only when the default Network or its transport/VPN path
        // changes. Validation and metering updates alone do not kill sockets.
        val networkKey = network?.hashCode() ?: -1
        val routeFingerprint = "$networkKey|$transport|vpn=${if (vpn) 1 else 0}"
        val diagnosticFingerprint = "$routeFingerprint|internet=$internet|validated=$validated|metered=$metered|not_roaming=$notRoaming"
        if (diagnosticFingerprint == lastNetworkFingerprint) return
        val routeChanged = routeFingerprint != lastRouteFingerprint
        lastNetworkFingerprint = diagnosticFingerprint
        lastRouteFingerprint = routeFingerprint

        DiagnosticStore.record(this, "network_event=$reason $descriptor route_changed=${if (routeChanged) 1 else 0}")
        Log.i(TAG, "Network changed: $reason $descriptor")
        if (isRunning && routeChanged) {
            workerHandler?.post {
                runCatching { RustBridge.onNetworkChange("$reason $descriptor") }
                    .onFailure {
                        DiagnosticStore.record(
                            this,
                            "network_handover_reset_failed type=${it.javaClass.simpleName}",
                        )
                    }
            }
        }
    }

    /**
     * LinkProperties may change during a cellular handover even when Android
     * keeps the same Network object. Export only shape/count metadata, never
     * addresses, DNS values, interface names, or proxy details.
     */
    private fun updateLinkProperties(network: Network, properties: LinkProperties) {
        val ipv4 = properties.linkAddresses.count { it.address is Inet4Address }
        val ipv6 = properties.linkAddresses.count { it.address is Inet6Address }
        val defaultRoutes = properties.routes.count { it.isDefaultRoute }
        val routeCount = properties.routes.size
        val dnsCount = properties.dnsServers.size
        val summary = "ipv4=$ipv4 ipv6=$ipv6 default_routes=$defaultRoutes routes=$routeCount dns_count=$dnsCount mtu=${properties.mtu}"
        val networkKey = network.hashCode().toString()
        val fingerprint = "$networkKey|$ipv4|$ipv6|$defaultRoutes|$routeCount|$dnsCount|${properties.mtu}"
        val previous = lastLinkFingerprint
        if (fingerprint == previous) return
        lastLinkFingerprint = fingerprint
        DiagnosticStore.record(this, "network_link_properties $summary")
        val changedOnSameNetwork = previous != null &&
            previous.substringBefore('|') == networkKey
        if (isRunning && changedOnSameNetwork) {
            workerHandler?.post {
                runCatching { RustBridge.onNetworkChange("link_properties_changed") }
                    .onFailure {
                        DiagnosticStore.record(
                            this,
                            "network_handover_reset_failed type=${it.javaClass.simpleName}",
                        )
                    }
            }
        }
    }

    private fun classifyStartError(error: String, state: String): String {
        val value = "$state $error".lowercase()
        return when {
            "address already in use" in value || "eaddrinuse" in value || "bind" in value -> "bind"
            "permission" in value || "denied" in value -> "permission"
            "native library" in value || "unsatisfiedlink" in value -> "native_library"
            "secret" in value || "configuration" in value || "invalid config" in value -> "configuration"
            else -> "native_start_failed"
        }
    }

    // живое уведомление

    private fun startNotifTicker() {
        stopNotifTicker()
        val r = object : Runnable {
            override fun run() {
                if (!isRunning) return
                renewWakeLock()
                maybeRaiseTunnel()
                val statsRaw = runCatching { RustBridge.stats(this@ProxyService) }.getOrNull()
                val stats = statsRaw?.let { runCatching { JSONObject(it) }.getOrNull() }
                if (stats != null) {
                    updateNotification(
                        getString(R.string.ark_stats_traffic,
                            human(stats.optLong("bytes_up")),
                            human(stats.optLong("bytes_down"))) +
                            " · ${stats.optInt("active")}/${stats.optInt("total")}"
                    )
                }
                ui.postDelayed(this, NOTIF_REFRESH_MS)
            }
        }
        notifTicker = r
        ui.postDelayed(r, NOTIF_REFRESH_MS)
    }

    private fun stopNotifTicker() {
        notifTicker?.let { ui.removeCallbacks(it) }
        notifTicker = null
    }

    private fun human(n: Long): String {
        var v = n.toDouble()
        for (unit in listOf("Б", "КБ", "МБ", "ГБ")) {
            if (kotlin.math.abs(v) < 1024) return "%.1f %s".format(v, unit)
            v /= 1024
        }
        return "%.1f ТБ".format(v)
    }

    // туннель живучести

    /**
     * Поднимает собственный туннель, если это уместно.
     *
     * Проверка живёт в тикере, а не в колбэке сети, намеренно. Когда чужой VPN
     * вытесняет наш, события приходят вплотную: сначала onRevoke, затем падение
     * нашей сети, только потом подъём чужой. Реакция на «VPN упал» в этот
     * момент увидела бы, что VPN нет, и подняла бы туннель обратно — вытеснив
     * только что запущенный чужой обходчик. Пятисекундная проверка застаёт
     * систему уже в устоявшемся состоянии.
     */
    private fun maybeRaiseTunnel() {
        if (!ProxyConfigStore.tunnelEnabled(this)) {
            // Пользователь мог выключить настройку при уже поднятом туннеле:
            // тогда значок VPN остался бы висеть вопреки выключенной галочке.
            if (TunnelService.isActive) TunnelService.stop(this)
            return
        }
        if (TunnelService.isActive) return

        val now = System.currentTimeMillis()
        if (now - lastTunnelAttempt < TUNNEL_RETRY_MS) return

        if (TunnelService.foreignVpnActive(this)) {
            // Не ошибка, а штатный режим: под чужим туннелем процесс защищён
            // ровно так же, а вытеснять чужой обходчик мы не вправе.
            return
        }
        if (!TunnelService.consentGranted(this)) {
            // Согласие спрашивается из экрана настроек: системный диалог
            // показывается только из Activity, из сервиса его не вызвать.
            Log.i(TAG, "Туннель включён, но согласия нет — пропускаем")
            lastTunnelAttempt = now
            return
        }
        lastTunnelAttempt = now
        TunnelService.start(this)
    }

    // wake lock

    /**
     * Wake lock с обновлением по таймауту вместо бессрочного.
     *
     * Бессрочный `acquire()` Android помечает флагом LONG, и менеджеры питания
     * прошивок (на Infinix — Phone Master) такие локи снимают. По логам это
     * выглядело так: `REL TgWsProxy::proxy`, дальше процессор засыпает, прокси
     * замирает на минуты, в очереди приёма копятся соединения Telegram, и всё
     * оживает только при следующем ACQ. Моменты REL/ACQ совпадали с провалами
     * в логе прокси один в один.
     *
     * Поэтому берём лок с ограниченным сроком и продлеваем его тикером: с точки
     * зрения системы это обычное поведение, а не «висящий вечно» лок.
     */
    private fun acquireWakeLock() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        val lock = wakeLock ?: pm.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK, "TgWsProxy::proxy"
        ).also {
            it.setReferenceCounted(false)
            wakeLock = it
        }
        try {
            lock.acquire(WAKELOCK_TTL_MS)
            lastWakeLockAcquire = System.currentTimeMillis()
        } catch (t: Throwable) {
            Log.w(TAG, "Не удалось взять wake lock", t)
        }
    }

    /** Проверяет, что лок всё ещё наш, и продлевает. Зовётся из тикера. */
    private fun renewWakeLock() {
        if (!isRunning) return
        val lock = wakeLock
        val lost = lock == null || !lock.isHeld
        if (lost) {
            Log.w(TAG, "Wake lock не держится — берём заново")
        } else if (System.currentTimeMillis() - lastWakeLockAcquire < WAKELOCK_RENEW_MS) {
            return
        }
        acquireWakeLock()
    }

    private fun releaseWakeLock() {
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
    }

    // уведомление

    private fun startForegroundCompat(notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIF_ID, notification)
        }
    }

    @Suppress("DEPRECATION")
    private fun stopForegroundCompat() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_REMOVE)
        } else {
            stopForeground(true)
        }
    }

    private fun updateNotification(text: String) {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.notify(NOTIF_ID, buildNotification(text))
    }

    private fun buildNotification(text: String): Notification {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            // IMPORTANCE_LOW: уведомление постоянное, звенеть оно не должно.
            val channel = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.ark_notif_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            )
            channel.setShowBadge(false)
            nm.createNotificationChannel(channel)
        }

        val openIntent = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this, 1,
            Intent(this, ProxyService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )

        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }

        return builder
            .setContentTitle(getString(R.string.ark_notif_title))
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(android.graphics.Color.WHITE)
            .setLargeIcon(android.graphics.BitmapFactory.decodeResource(resources, R.drawable.ic_notification_large))
            .setOngoing(true)
            .setContentIntent(openIntent)
            .addAction(
                Notification.Action.Builder(
                    android.graphics.drawable.Icon.createWithResource(
                        this, R.drawable.ic_notification
                    ),
                    getString(R.string.ark_notif_stop),
                    stopIntent,
                ).build()
            )
            .build()
    }
}

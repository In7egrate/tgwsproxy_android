package com.amurcanov.tgwsproxy

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.widget.Toast
import org.json.JSONArray

/**
 * Enumerate apps which can actually handle tg://proxy, not all apps on device.
 * Discovery on first run is persisted. Rescanning before applying prevents stale entries.
 * Android package visibility limits apply: this is NOT a promise to detect every fork.
 */
data class TgClient(val packageName: String, val label: String, val httpsOnly: Boolean = false)

object TgClientRegistry {
    private const val PREF = "telegram_clients"
    private const val FIRST_SCAN = "scanned"
    private const val SAVED = "packages"
    private const val LAST = "last_client"
    private val sampleUri = Uri.parse("tg://proxy?server=127.0.0.1&port=1443&secret=dd00000000000000000000000000000000")
    private val httpsUri = Uri.parse("https://t.me/proxy?server=127.0.0.1&port=1443&secret=dd00000000000000000000000000000000")

    // Known forks supply visibility hints, but registration as a URI handler is verified.
    private val known = listOf(
        "org.telegram.messenger", "org.telegram.messenger.web", "org.telegram.messenger.beta",
        "com.radolyn.ayugram", "com.exteragram.messenger", "org.telegram.plus",
        "ir.ilmili.telegraph", "org.telegram.BifToGram", "tw.nekomimi.nekogram",
        "xyz.nextalone.nagram", "uz.unnarsx.cherrygram", "org.telegram.mdgram",
        "org.forkclient.messenger.beta", "app.nicegram", "top.qwq2333.nullgram",
        "com.iMe.android", "ru.dahl.messenger", "com.scriptsaz.litegram",
        "org.thunderdog.challegram"
    )

    @Suppress("DEPRECATION")
    fun scan(context: Context): List<TgClient> {
        val pm = context.packageManager
        val intent = Intent(Intent.ACTION_VIEW, sampleUri).addCategory(Intent.CATEGORY_BROWSABLE)
        val handlers = if (Build.VERSION.SDK_INT >= 33) {
            pm.queryIntentActivities(intent, PackageManager.ResolveInfoFlags.of(PackageManager.MATCH_DEFAULT_ONLY.toLong()))
        } else {
            pm.queryIntentActivities(intent, PackageManager.MATCH_DEFAULT_ONLY)
        }
        val packages = (handlers.map { it.activityInfo.packageName } + known).distinct()
        val result = packages.mapNotNull { name ->
            // An app is offered only if its specific package resolves the tg://proxy URI.
            val direct = Intent(Intent.ACTION_VIEW, sampleUri).addCategory(Intent.CATEGORY_BROWSABLE).setPackage(name)
            val supported = if (Build.VERSION.SDK_INT >= 33) {
                pm.queryIntentActivities(direct, PackageManager.ResolveInfoFlags.of(PackageManager.MATCH_DEFAULT_ONLY.toLong())).isNotEmpty()
            } else {
                pm.queryIntentActivities(direct, PackageManager.MATCH_DEFAULT_ONLY).isNotEmpty()
            }
            // Some forks declare only HTTPS universal links for t.me/proxy.
            // The fallback is limited to the known Telegram packages, not browsers.
            val webSupported = if (!supported && name in known) {
                val webIntent = Intent(Intent.ACTION_VIEW, httpsUri)
                    .addCategory(Intent.CATEGORY_BROWSABLE).setPackage(name)
                if (Build.VERSION.SDK_INT >= 33)
                    pm.queryIntentActivities(webIntent, PackageManager.ResolveInfoFlags.of(PackageManager.MATCH_DEFAULT_ONLY.toLong())).isNotEmpty()
                else pm.queryIntentActivities(webIntent, PackageManager.MATCH_DEFAULT_ONLY).isNotEmpty()
            } else false
            if (!supported && !webSupported) return@mapNotNull null
            val label = runCatching { pm.getApplicationLabel(pm.getApplicationInfo(name, 0)).toString() }
                .getOrDefault(name)
            TgClient(name, label, httpsOnly = !supported)
        }.sortedWith(compareByDescending<TgClient> {
            it.packageName == context.getSharedPreferences(PREF, Context.MODE_PRIVATE)
                .getString(LAST, "")
        }.thenBy { it.label.lowercase() })

        val store = context.getSharedPreferences(PREF, Context.MODE_PRIVATE)
        store.edit().putBoolean(FIRST_SCAN, true)
            .putString(SAVED, JSONArray(result.map { it.packageName }).toString()).apply()
        return result
    }

    fun previous(context: Context): List<String> {
        val text = context.getSharedPreferences(PREF, Context.MODE_PRIVATE).getString(SAVED, "[]") ?: "[]"
        return runCatching {
            val arr = JSONArray(text)
            (0 until arr.length()).map { arr.getString(it) }
        }.getOrDefault(emptyList())
    }

    fun open(context: Context, client: TgClient, link: String) {
        try {
            val actualLink = if (client.httpsOnly) link.replace("tg://proxy?", "https://t.me/proxy?") else link
            val intent = Intent(Intent.ACTION_VIEW, Uri.parse(actualLink))
                .addCategory(Intent.CATEGORY_BROWSABLE)
                .setPackage(client.packageName)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(intent)
            context.getSharedPreferences(PREF, Context.MODE_PRIVATE)
                .edit().putString(LAST, client.packageName).apply()
        } catch (e: Exception) {
            Toast.makeText(context, "Клиент ${client.label} не открыл ссылку", Toast.LENGTH_LONG).show()
        }
    }
}

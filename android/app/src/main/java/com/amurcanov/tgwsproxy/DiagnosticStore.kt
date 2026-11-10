package com.amurcanov.tgwsproxy

import android.content.Context
import android.os.Build
import android.os.SystemClock
import android.util.Log
import java.io.File

/** Private, bounded diagnostic log. No packet capture or credentials. */
object DiagnosticStore {
    private val mutex = Any()
    private const val FILE_NAME = "diagnostics.txt"
    private const val PREVIOUS_FILE_NAME = "diagnostics.previous.txt"
    private const val MAX_EXPORT_CHARS = 500_000

    fun record(context: Context, message: String) {
        synchronized(mutex) {
            runCatching {
                val configuredMb = ProxyConfigStore.load(context).logMaxMb.coerceIn(1, 50)
                val maxBytes = configuredMb.toLong() * 1024L * 1024L
                val file = File(context.filesDir, FILE_NAME)
                val line = "wall_ms=${System.currentTimeMillis()} elapsed_ms=${SystemClock.elapsedRealtime()} ${safe(message)}\n"
                if (file.exists() && file.length() + line.toByteArray(Charsets.UTF_8).size > maxBytes) {
                    val backup = File(context.filesDir, PREVIOUS_FILE_NAME)
                    if (backup.exists()) backup.delete()
                    file.renameTo(backup)
                }
                file.appendText(line, Charsets.UTF_8)
            }.onFailure { Log.w("TGWS", "Diagnostics write failed", it) }
        }
    }

    /** Called only after the user taps Export in Settings. */
    fun exportText(context: Context): String {
        val liveStats = runCatching { RustBridge.stats(context) }.getOrDefault("{}")
        return synchronized(mutex) {
        val current = readTail(File(context.filesDir, FILE_NAME))
        val previous = readTail(File(context.filesDir, PREVIOUS_FILE_NAME))
        val version = runCatching {
            @Suppress("DEPRECATION")
            context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "unknown"
        }.getOrDefault("unknown")
        buildString {
            appendLine("TG WS Proxy diagnostic export")
            appendLine("created_wall_ms=${System.currentTimeMillis()}")
            appendLine("created_elapsed_ms=${SystemClock.elapsedRealtime()}")
            appendLine("app_package=${context.packageName}")
            appendLine("app_version=$version")
            appendLine("app_version_code=${BuildConfig.VERSION_CODE}")
            appendLine("android_release=${Build.VERSION.RELEASE}")
            appendLine("android_api=${Build.VERSION.SDK_INT}")
            appendLine("device=${safe(Build.MANUFACTURER)} ${safe(Build.MODEL)}")
            appendLine("privacy=No message payloads, MTProto secrets, tokens, IP addresses, or hostnames are intentionally recorded.")
            appendLine("--- live native stats and recent event ring ---")
            appendLine(liveStats)
            appendLine("--- previous log (tail) ---")
            append(previous)
            appendLine()
            appendLine("--- current log (tail) ---")
            append(current)
            appendLine()
        }
        }
    }

    private fun readTail(file: File): String {
        if (!file.isFile) return "(empty)\n"
        val text = runCatching { file.readText(Charsets.UTF_8) }.getOrDefault("(read failed)\n")
        return if (text.length > MAX_EXPORT_CHARS) {
            "[truncated: showing last $MAX_EXPORT_CHARS characters]\n" + text.takeLast(MAX_EXPORT_CHARS)
        } else text
    }

    private fun safe(value: String): String = value
        .replace('\r', ' ')
        .replace('\n', ' ')
        .take(2048)
}

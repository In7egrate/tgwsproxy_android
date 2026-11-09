package com.amurcanov.tgwsproxy

import android.content.Context
import android.content.Intent
import android.widget.Toast
import androidx.core.content.ContextCompat

/** Uses only the Arkomiko service and its canonical persisted settings. */
object ProxyController {
    suspend fun startFromSavedSettings(context: Context, showInvalidPortToast: Boolean = true): Boolean {
        return try {
            ContextCompat.startForegroundService(context,
                Intent(context, ProxyService::class.java).setAction(ProxyService.ACTION_START))
            true
        } catch (t: Exception) {
            if (showInvalidPortToast) Toast.makeText(context, t.message ?: "Ошибка запуска", Toast.LENGTH_LONG).show()
            false
        }
    }

    fun stop(context: Context) {
        context.startService(Intent(context, ProxyService::class.java).setAction(ProxyService.ACTION_STOP))
    }

    fun restart(context: Context) {
        ContextCompat.startForegroundService(context,
            Intent(context, ProxyService::class.java).setAction(ProxyService.ACTION_RESTART))
    }
}

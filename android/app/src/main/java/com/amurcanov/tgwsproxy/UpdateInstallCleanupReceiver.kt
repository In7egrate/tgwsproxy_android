package com.amurcanov.tgwsproxy

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Removes the cached installer only after Android confirms this package was replaced. */
class UpdateInstallCleanupReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        if (intent?.action == Intent.ACTION_MY_PACKAGE_REPLACED) {
            GithubUpdateManager.cleanupInstallerFile(context.applicationContext)
        }
    }
}

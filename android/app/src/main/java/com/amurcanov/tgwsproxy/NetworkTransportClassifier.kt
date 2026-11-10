package com.amurcanov.tgwsproxy

import android.net.NetworkCapabilities
import android.os.Build

internal data class NetworkTransportSnapshot(
    val transport: String,
    val vpn: Boolean,
    val validated: Boolean,
    val metered: Boolean,
    val roaming: Boolean,
) {
    /** Changes only when the route class or VPN layer changes. */
    val routeKey: String
        get() = "$transport|vpn=$vpn"

    fun diagnostic(): String =
        "transport=$transport vpn=$vpn validated=$validated metered=$metered roaming=$roaming"
}

internal object NetworkTransportClassifier {
    fun classify(
        vpn: Boolean,
        cellular: Boolean,
        wifi: Boolean,
        ethernet: Boolean,
    ): String = when {
        vpn -> "vpn"
        cellular -> "cellular"
        wifi -> "wifi"
        ethernet -> "ethernet"
        else -> "other"
    }

    fun from(capabilities: NetworkCapabilities): NetworkTransportSnapshot {
        val vpn = capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN) ||
            !capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
        val roaming = Build.VERSION.SDK_INT >= Build.VERSION_CODES.P &&
            !capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_ROAMING)
        return NetworkTransportSnapshot(
            transport = classify(
                vpn = vpn,
                cellular = capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR),
                wifi = capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI),
                ethernet = capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET),
            ),
            vpn = vpn,
            validated = capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED),
            metered = !capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED),
            roaming = roaming,
        )
    }
}

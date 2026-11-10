package com.amurcanov.tgwsproxy

import org.junit.Assert.assertEquals
import org.junit.Test

class NetworkTransportClassifierTest {
    @Test
    fun cellularIsDistinguishedFromWifiHotspotClient() {
        assertEquals("cellular", NetworkTransportClassifier.classify(
            vpn = false, cellular = true, wifi = false, ethernet = false,
        ))
        assertEquals("wifi", NetworkTransportClassifier.classify(
            vpn = false, cellular = false, wifi = true, ethernet = false,
        ))
    }

    @Test
    fun vpnTakesPrecedenceOverUnderlyingTransportFlags() {
        assertEquals("vpn", NetworkTransportClassifier.classify(
            vpn = true, cellular = true, wifi = false, ethernet = false,
        ))
    }

    @Test
    fun unknownTransportIsReportedWithoutGuessing() {
        assertEquals("other", NetworkTransportClassifier.classify(
            vpn = false, cellular = false, wifi = false, ethernet = false,
        ))
    }
}

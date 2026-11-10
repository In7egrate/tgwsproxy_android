package com.amurcanov.tgwsproxy

import org.junit.Assert.assertEquals
import org.junit.Test

class GithubUpdateManagerTest {
    @Test fun comparesNumericComponentsInsteadOfLexicographicText() {
        assertEquals(1, GithubUpdateManager.compareVersionStrings("v3.10.0", "3.9.9"))
        assertEquals(-1, GithubUpdateManager.compareVersionStrings("3.2.9", "3.3.0"))
    }

    @Test fun ignoresPrefixAndOrdersStableAfterPrerelease() {
        assertEquals(1, GithubUpdateManager.compareVersionStrings("v3.1.0", "3.1.0-rc1"))
        assertEquals(-1, GithubUpdateManager.compareVersionStrings("v3.1.0-beta1", "3.1.0"))
        assertEquals(1, GithubUpdateManager.compareVersionStrings("release-3.1.1", "3.1.0"))
        assertEquals(1, GithubUpdateManager.compareVersionStrings("v3.1.0-rc.10", "v3.1.0-rc.2"))
        assertEquals(-1, GithubUpdateManager.compareVersionStrings("v3.1.0-rc.2", "v3.1.0-rc.10"))
        assertEquals(0, GithubUpdateManager.compareVersionStrings("v3.1.0+build.4", "3.1.0"))
        assertEquals(0, GithubUpdateManager.compareVersionStrings("v3.1.0", "3.1.0"))
    }
}

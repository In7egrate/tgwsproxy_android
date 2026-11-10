package com.amurcanov.tgwsproxy

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

sealed class UpdateCheckResult {
    data class Available(val release: GithubRelease) : UpdateCheckResult()
    data class UpToDate(val latestVersion: String) : UpdateCheckResult()
    object NoRelease : UpdateCheckResult()
    data class ReleaseWithoutApk(val version: String, val releaseUrl: String) : UpdateCheckResult()
    data class Failed(val reason: String) : UpdateCheckResult()
}

data class GithubRelease(
    val tagName: String,
    val releaseUrl: String,
    val downloadUrl: String,
    val assetName: String,
    val sha256: String?
)

enum class InstallRequestState { Launched, PermissionRequired }

object GithubUpdateManager {
    const val DEVELOPER_URL = "https://github.com/In7egrate"
    private const val REPOSITORY = "In7egrate/tgwsproxy_android"
    private const val LATEST_RELEASE_URL = "https://api.github.com/repos/$REPOSITORY/releases/latest"
    private const val PREFS = "ws_proxy_updates"
    private const val KEY_WAITING_PERMISSION = "waiting_install_permission"
    private const val KEY_APK_PATH = "pending_apk_path"
    private const val KEY_INSTALL_LAUNCHED = "installer_launched"
    private const val MAX_APK_BYTES = 150L * 1024L * 1024L

    fun checkLatest(context: Context): UpdateCheckResult {
        if (BuildConfig.DEBUG) {
            return UpdateCheckResult.Failed("Проверка обновлений доступна в release-сборке.")
        }
        val connection = openConnection(LATEST_RELEASE_URL)
        return try {
            val code = connection.responseCode
            if (code == 404) return UpdateCheckResult.NoRelease
            if (code != HttpURLConnection.HTTP_OK) {
                return UpdateCheckResult.Failed("GitHub HTTP $code")
            }
            val body = connection.inputStream.use { it.readBytes().toString(Charsets.UTF_8) }
            val json = JSONObject(body)
            val tag = json.optString("tag_name").takeIf { it.isNotBlank() }
                ?: return UpdateCheckResult.Failed("В release отсутствует tag_name")
            val releaseUrl = json.optString("html_url", "https://github.com/$REPOSITORY/releases")
            if (compareVersionStrings(tag, BuildConfig.VERSION_NAME) <= 0) {
                return UpdateCheckResult.UpToDate(tag)
            }
            val assets = json.optJSONArray("assets")
            val apkAssets = (0 until (assets?.length() ?: 0)).mapNotNull { index ->
                assets?.optJSONObject(index)?.let { asset ->
                    val name = asset.optString("name")
                    val url = asset.optString("browser_download_url")
                    if (name.endsWith(".apk", ignoreCase = true) &&
                        !name.contains("debug", ignoreCase = true) &&
                        !name.contains("unsigned", ignoreCase = true) &&
                        url.startsWith("https://")) asset else null
                }
            }
            val selected = selectApkAsset(apkAssets)
                ?: return UpdateCheckResult.ReleaseWithoutApk(tag, releaseUrl)
            val digest = selected.optString("digest").takeIf { it.startsWith("sha256:") }
                ?.substringAfter("sha256:")?.lowercase()
            UpdateCheckResult.Available(
                GithubRelease(tag, releaseUrl, selected.getString("browser_download_url"),
                    selected.getString("name"), digest)
            )
        } catch (t: Throwable) {
            UpdateCheckResult.Failed(t.message ?: t.javaClass.simpleName)
        } finally {
            connection.disconnect()
        }
    }

    internal fun compareVersionStrings(remote: String, local: String): Int {
        data class ParsedVersion(val core: List<Long>, val prerelease: String?)

        fun parse(value: String): ParsedVersion {
            // GitHub tags are not guaranteed to be exactly "v1.2.3": some
            // projects use prefixes such as "release-1.2.3". Find the first
            // numeric SemVer-like sequence instead of discarding that tag.
            val match = Regex("""(\d+(?:\.\d+)*)(?:-([0-9A-Za-z][0-9A-Za-z.-]*))?""")
                .find(value.trim())
                ?: return ParsedVersion(emptyList(), null)
            val core = match.groupValues[1].split('.').mapNotNull { it.toLongOrNull() }
            val prerelease = match.groupValues[2].takeIf { it.isNotEmpty() }
            return ParsedVersion(core, prerelease)
        }

        fun comparePrerelease(a: String, b: String): Int {
            val left = a.split('.', '-')
            val right = b.split('.', '-')
            for (index in 0 until maxOf(left.size, right.size)) {
                if (index >= left.size) return -1
                if (index >= right.size) return 1
                val x = left[index]
                val y = right[index]
                val xn = x.toLongOrNull()
                val yn = y.toLongOrNull()
                val compared = when {
                    xn != null && yn != null -> xn.compareTo(yn)
                    xn != null -> -1
                    yn != null -> 1
                    else -> x.compareTo(y)
                }
                if (compared != 0) return compared
            }
            return 0
        }

        val remoteVersion = parse(remote)
        val localVersion = parse(local)
        for (index in 0 until maxOf(remoteVersion.core.size, localVersion.core.size)) {
            val a = remoteVersion.core.getOrElse(index) { 0L }
            val b = localVersion.core.getOrElse(index) { 0L }
            if (a != b) return a.compareTo(b)
        }

        val remotePre = remoteVersion.prerelease
        val localPre = localVersion.prerelease
        if (remotePre == null && localPre != null) return 1
        if (remotePre != null && localPre == null) return -1
        if (remotePre != null && localPre != null) {
            return comparePrerelease(remotePre, localPre)
        }
        return 0
    }

    private fun selectApkAsset(assets: List<JSONObject>): JSONObject? {
        if (assets.isEmpty()) return null
        assets.firstOrNull { it.optString("name").contains("universal", ignoreCase = true) }?.let { return it }
        val supported = Build.SUPPORTED_ABIS.toList()
        for (abi in supported) {
            assets.firstOrNull { it.optString("name").contains(abi, ignoreCase = true) }?.let { return it }
        }
        return assets.singleOrNull()
    }

    fun downloadAndValidate(context: Context, release: GithubRelease): File {
        val folder = File(context.filesDir, "updates").apply { mkdirs() }
        val target = File(folder, "ws-proxy-update.apk")
        val partial = File(folder, "ws-proxy-update.apk.part")
        partial.delete()
        val connection = openConnection(release.downloadUrl)
        try {
            val code = connection.responseCode
            if (code != HttpURLConnection.HTTP_OK) error("Загрузка APK: HTTP $code")
            val finalHost = connection.url.host.lowercase()
            if (finalHost != "github.com" && !finalHost.endsWith(".githubusercontent.com")) {
                error("APK redirected to an unexpected host")
            }
            val declaredLength = connection.contentLengthLong
            if (declaredLength > MAX_APK_BYTES) error("APK превышает допустимый размер")
            connection.inputStream.use { input ->
                partial.outputStream().buffered().use { output ->
                    val buffer = ByteArray(64 * 1024)
                    var total = 0L
                    while (true) {
                        val read = input.read(buffer)
                        if (read < 0) break
                        total += read
                        if (total > MAX_APK_BYTES) error("APK превышает допустимый размер")
                        output.write(buffer, 0, read)
                    }
                }
            }
            if (partial.length() < 4L) error("Загруженный файл пуст")
            val magic = ByteArray(4)
            val magicBytes = partial.inputStream().use { input ->
                var offset = 0
                while (offset < magic.size) {
                    val count = input.read(magic, offset, magic.size - offset)
                    if (count < 0) break
                    offset += count
                }
                offset
            }
            if (magicBytes < 4 || magic[0] != 0x50.toByte() || magic[1] != 0x4b.toByte()) {
                error("Загруженный файл не является APK/ZIP")
            }
            release.sha256?.let { expected ->
                val actual = sha256File(partial)
                if (!actual.equals(expected, ignoreCase = true)) error("SHA-256 APK не совпадает с GitHub digest")
            }
            validatePackage(context, partial)
            if (target.exists() && !target.delete()) error("Не удалось заменить предыдущий APK")
            if (!partial.renameTo(target)) {
                partial.copyTo(target, overwrite = true)
                partial.delete()
            }
            return target
        } catch (t: Throwable) {
            partial.delete()
            throw t
        } finally {
            connection.disconnect()
        }
    }

    private fun validatePackage(context: Context, apk: File) {
        val pm = context.packageManager
        val flags = if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES else PackageManager.GET_SIGNATURES
        val archive = getArchiveInfo(pm, apk.absolutePath, flags)
            ?: error("Android не распознал загруженный APK")
        if (archive.packageName != context.packageName) error("APK предназначен для другого приложения")
        val archiveVersion = packageVersionCode(archive)
        val installed = getInstalledInfo(pm, context.packageName, flags)
        val installedVersion = packageVersionCode(installed)
        if (archiveVersion <= installedVersion) error("В APK нет более новой versionCode")
        val archiveSigners = signerDigests(archive)
        val installedSigners = signerDigests(installed)
        if (archiveSigners.isEmpty() || installedSigners.isEmpty() || archiveSigners.toSet() != installedSigners.toSet()) {
            error("Подпись APK не совпадает с установленной версией. Обновление заблокировано для защиты приложения.")
        }
    }

    @Suppress("DEPRECATION")
    private fun getArchiveInfo(pm: PackageManager, path: String, flags: Int): PackageInfo? =
        if (Build.VERSION.SDK_INT >= 33) pm.getPackageArchiveInfo(path, PackageManager.PackageInfoFlags.of(flags.toLong()))
        else pm.getPackageArchiveInfo(path, flags)

    @Suppress("DEPRECATION")
    private fun getInstalledInfo(pm: PackageManager, packageName: String, flags: Int): PackageInfo =
        if (Build.VERSION.SDK_INT >= 33) pm.getPackageInfo(packageName, PackageManager.PackageInfoFlags.of(flags.toLong()))
        else pm.getPackageInfo(packageName, flags)

    @Suppress("DEPRECATION")
    private fun packageVersionCode(info: PackageInfo): Long =
        if (Build.VERSION.SDK_INT >= 28) info.longVersionCode else info.versionCode.toLong()

    private fun sha256File(file: File): String {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().buffered().use { input ->
            val buffer = ByteArray(64 * 1024)
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                digest.update(buffer, 0, count)
            }
        }
        return digest.digest().toHex()
    }

    @Suppress("DEPRECATION")
    private fun signerDigests(info: PackageInfo): List<String> {
        val signatures = if (Build.VERSION.SDK_INT >= 28) {
            info.signingInfo?.apkContentsSigners?.toList().orEmpty()
        } else info.signatures?.toList().orEmpty()
        return signatures.map { MessageDigest.getInstance("SHA-256").digest(it.toByteArray()).toHex() }
    }

    fun requestInstall(context: Context, apk: File): InstallRequestState {
        require(apk.exists() && apk.length() > 0L) { "Файл обновления отсутствует" }
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.edit().putString(KEY_APK_PATH, apk.absolutePath).putBoolean(KEY_INSTALL_LAUNCHED, false).apply()
        if (Build.VERSION.SDK_INT >= 26 && !context.packageManager.canRequestPackageInstalls()) {
            prefs.edit().putBoolean(KEY_WAITING_PERMISSION, true).apply()
            val intent = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                Uri.parse("package:${context.packageName}")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(intent)
            return InstallRequestState.PermissionRequired
        }
        launchInstaller(context, apk)
        return InstallRequestState.Launched
    }

    fun resumePendingInstall(activity: Activity) {
        val prefs = activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        if (!prefs.getBoolean(KEY_WAITING_PERMISSION, false)) return
        if (Build.VERSION.SDK_INT >= 26 && !activity.packageManager.canRequestPackageInstalls()) return
        val path = prefs.getString(KEY_APK_PATH, null)
        prefs.edit().putBoolean(KEY_WAITING_PERMISSION, false).apply()
        val apk = path?.let(::File)
        if (apk != null && apk.exists()) runCatching { launchInstaller(activity, apk) }
    }

    private fun launchInstaller(context: Context, apk: File) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", apk)
        val intent = Intent(Intent.ACTION_VIEW).setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        if (context !is Activity) intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(intent)
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit().putBoolean(KEY_INSTALL_LAUNCHED, true).apply()
    }

    fun cleanupInstallerFile(context: Context) {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.getString(KEY_APK_PATH, null)?.let { runCatching { File(it).delete() } }
        File(context.filesDir, "updates/ws-proxy-update.apk.part").delete()
        prefs.edit().remove(KEY_APK_PATH).remove(KEY_WAITING_PERMISSION).remove(KEY_INSTALL_LAUNCHED).apply()
    }

    private fun openConnection(url: String): HttpURLConnection {
        val parsed = URL(url)
        require(parsed.protocol == "https" && parsed.host.lowercase() in setOf("api.github.com", "github.com")) {
            "Разрешены только GitHub HTTPS URL"
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 60_000
            requestMethod = "GET"
            setRequestProperty("Accept", "application/vnd.github+json")
            setRequestProperty("User-Agent", "WS-Proxy-Android")
            setRequestProperty("X-GitHub-Api-Version", "2022-11-28")
            instanceFollowRedirects = true
        }
    }

    private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }
}

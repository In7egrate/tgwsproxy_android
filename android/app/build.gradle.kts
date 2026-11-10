import java.util.Properties
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.io.File

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The Android app uses the Rust/Tokio native core from ../rust-core.
// Native libraries are built from source by build_v3.ps1 or the Android release workflow.

val keystoreProperties = Properties().apply {
    val f = rootProject.file("keystore.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}
val hasSigning = keystoreProperties.getProperty("storeFile")?.let { rootProject.file(it).exists() } == true


// Science Gothic is pinned to an immutable upstream commit. The font is downloaded
// once into Android resources so the app uses it offline after installation.
val scienceGothicFont = layout.projectDirectory.file("src/main/res/font/science_gothic.ttf").asFile
val downloadScienceGothicFont by tasks.registering {
    outputs.file(scienceGothicFont)
    doLast {
        fun hasPinnedFont(file: File): Boolean {
            if (!file.exists() || file.length() != 2_726_388L) return false
            val existing = file.readBytes()
            val existingDigest = MessageDigest.getInstance("SHA-1")
            existingDigest.update("blob ${existing.size}\u0000".toByteArray(Charsets.UTF_8))
            existingDigest.update(existing)
            return existingDigest.digest().joinToString("") { "%02x".format(it) } == "eb77342eefe0c8549325e10b0f7d48793c714915"
        }
        if (hasPinnedFont(scienceGothicFont)) return@doLast
        scienceGothicFont.parentFile.mkdirs()
        val temp = File(scienceGothicFont.parentFile, "science_gothic.ttf.download")
        val url = URL("https://raw.githubusercontent.com/googlefonts/science-gothic/6e2b703b3df79fecf04cadc9c54ea68459856e50/fonts/variable/ScienceGothic%5BCTRS%2Cslnt%2Cwdth%2Cwght%5D.ttf")
        val connection = url.openConnection() as HttpURLConnection
        connection.connectTimeout = 20_000
        connection.readTimeout = 60_000
        connection.setRequestProperty("User-Agent", "WS-Proxy-Android-Build")
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) error("Science Gothic download failed: HTTP ${connection.responseCode}")
            connection.inputStream.use { input -> temp.outputStream().buffered().use { output -> input.copyTo(output) } }
            if (temp.length() != 2_726_388L) error("Science Gothic font size mismatch: ${temp.length()}")
            val bytes = temp.readBytes()
            val digest = MessageDigest.getInstance("SHA-1")
            digest.update("blob ${bytes.size}\u0000".toByteArray(Charsets.UTF_8))
            digest.update(bytes)
            val actual = digest.digest().joinToString("") { "%02x".format(it) }
            if (actual != "eb77342eefe0c8549325e10b0f7d48793c714915") error("Science Gothic font checksum mismatch")
            if (!temp.renameTo(scienceGothicFont)) {
                temp.copyTo(scienceGothicFont, overwrite = true)
                temp.delete()
            }
        } finally {
            connection.disconnect()
            temp.delete()
        }
    }
}
tasks.named("preBuild").configure { dependsOn(downloadScienceGothicFont) }

android {
    namespace = "com.amurcanov.tgwsproxy"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.tgwsproxy.merged"
        minSdk = 24
        targetSdk = 35
        // Версия APK своя, не совпадает с версией ядра. Префикс «A» —
        // Android, дальше поколение обвязки и порядковый номер сборки.
        // 204 is the first maintenance release after the public 1.0.0 build.
        versionCode = 204
        versionName = "1.0.1"

        ndk {
            // Ship the two ARM ABIs supported by the Rust cross-compilation setup.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    // Produce one APK per supported ABI plus a universal APK for sideloading.
    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "armeabi-v7a")
            isUniversalApk = true
        }
    }

    signingConfigs {
        if (hasSigning) {
            create("release") {
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
                storeType = keystoreProperties.getProperty("storeType") ?: "PKCS12"
            }
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
            versionNameSuffix = "-debug"
        }
        getByName("release") {
            // Keep the JNA API reachable through reflection in the minified build.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            if (hasSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    sourceSets {
        getByName("main") {
            // Keep Android resources and translations in the standard res/values
            // and res/values-<locale> directories.
            res.srcDirs("src/main/res")
        }
    }


    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    debugImplementation("androidx.compose.ui:ui-tooling")
    testImplementation("junit:junit:4.13.2")
}

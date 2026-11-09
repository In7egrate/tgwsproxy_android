# JNA discovers the native interface and its methods reflectively. Keep the
# interface, method names and JNA internals stable in minified release builds.
-keep interface com.amurcanov.tgwsproxy.RustApi { *; }
-keep class com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-dontwarn java.awt.**
-dontwarn com.sun.jna.**

# Android-instantiated application components use this package (not the old
# com.tgwsproxy.android package left by the v2 project).
-keep class com.amurcanov.tgwsproxy.MainActivity { *; }
-keep class com.amurcanov.tgwsproxy.ProxyService { *; }
-keep class com.amurcanov.tgwsproxy.TunnelService { *; }
-keep class com.amurcanov.tgwsproxy.ProxyTileService { *; }
-keep class com.amurcanov.tgwsproxy.WatchdogReceiver { *; }
-keep class com.amurcanov.tgwsproxy.BootReceiver { *; }
-keep class com.amurcanov.tgwsproxy.TilePreferencesActivity { *; }

# Keep source locations in crash reports.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile

# WS Proxy release signing

Run `powershell -ExecutionPolicy Bypass -File .\create-release-keystore.ps1` from the `android` directory. The script creates a local PKCS#12 release key and `keystore.properties`. Both are ignored by Git.

Back up `android/keystore/ws-proxy-release.p12` and its password separately (for example, an encrypted offline backup plus a password manager). Every APK published as an update must be signed with this same key. If the key is lost or changed, Android will refuse an in-place update; users would need to uninstall first, which removes app data.

Do not send the keystore or password in chat, commit them, or include them in source ZIPs. The app's in-app updater verifies that the downloaded APK has the same signing certificate as the installed app before launching Android's installer.

The first build downloads a pinned Science Gothic variable font from the official Google Fonts source and verifies its Git blob SHA-1 before packaging it. The font is licensed under SIL Open Font License 1.1; the license is included in `app/src/main/assets/licenses/ScienceGothic-OFL.txt`.

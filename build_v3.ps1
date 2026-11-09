param(
    [Parameter(Mandatory = $true)] [string]$Project,
    [ValidateSet('Debug', 'Release')] [string]$BuildType = 'Release',
    [switch]$Clean
)
$ErrorActionPreference = 'Stop'
$Project = (Resolve-Path $Project).Path
$readyMarker = Join-Path $Project '.tgws_v3_ready'
if (!(Test-Path $readyMarker)) {
    $hasAndroidApp = Test-Path (Join-Path $Project 'android\app\build.gradle.kts')
    $hasRustCore = Test-Path (Join-Path $Project 'rust-core\Cargo.toml')
    if ($hasAndroidApp -and $hasRustCore) {
        New-Item -ItemType File -Path $readyMarker -Force | Out-Null
    } else {
        throw 'Project is not a prepared Android/Rust v3 source tree. Legacy v2 migration is not included in this Android-only repository.'
    }
}

$Sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } elseif ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT } else { "$env:LOCALAPPDATA\Android\Sdk" }
if (!(Test-Path $Sdk)) { throw "Android SDK не найден: $Sdk" }
$env:ANDROID_HOME = $Sdk
$env:ANDROID_SDK_ROOT = $Sdk
$cmd = Get-Command cargo -ErrorAction SilentlyContinue
if (!$cmd -and (Test-Path "$HOME\.cargo\bin\cargo.exe")) {
    $env:Path = "$HOME\.cargo\bin;$env:Path"
    $cmd = Get-Command cargo -ErrorAction SilentlyContinue
}
if (!$cmd) {
    Write-Host 'Rust отсутствует. Установи https://rustup.rs (Rustup + совместимый host toolchain)'
    throw 'cargo не найден. APK v3 не собирается из старых предкомпилированных .so.'
}
if (!(Get-Command rustup -ErrorAction SilentlyContinue)) { throw 'rustup не найден' }
$ndkRoot = Join-Path $Sdk 'ndk'
$ndk = if (Test-Path $ndkRoot) {
    Get-ChildItem $ndkRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1
} else { $null }
if (!$ndk) {
    $manager = Join-Path $Sdk 'cmdline-tools\latest\bin\sdkmanager.bat'
    if (Test-Path $manager) {
        Write-Host 'NDK не найден. Установи Android NDK через:'
        Write-Host ('  & "{0}" --install "ndk;27.2.12479018"' -f $manager)
    }
    throw 'Требуется Android NDK для компиляции Rust под ARM.'
}
$bin = Join-Path $ndk.FullName 'toolchains\llvm\prebuilt\windows-x86_64\bin'
$arm64 = Join-Path $bin 'aarch64-linux-android24-clang.cmd'
$arm32 = Join-Path $bin 'armv7a-linux-androideabi24-clang.cmd'
if (!(Test-Path $arm64) -or !(Test-Path $arm32)) {
    throw "В установленном NDK нет нужных clang: $bin"
}
Write-Host "Android SDK: $Sdk"
Write-Host "Android NDK: $($ndk.FullName)"
Write-Host "Rust crate: $Project\rust-core"

& rustup target add aarch64-linux-android armv7-linux-androideabi
if ($LASTEXITCODE -ne 0) { throw 'rustup target add failed' }
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $arm64
$env:CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER = $arm32
$env:CC_aarch64_linux_android = $arm64
$env:CC_armv7_linux_androideabi = $arm32
$env:AR_aarch64_linux_android = Join-Path $bin 'llvm-ar.exe'
$env:AR_armv7_linux_androideabi = $env:AR_aarch64_linux_android

Push-Location "$Project\rust-core"
try {
    Write-Host 'Building patched Rust core: ARM64...'
    & cargo build --release --target aarch64-linux-android
    if ($LASTEXITCODE -ne 0) { throw 'ARM64 Rust compilation failed' }
    Write-Host 'Building patched Rust core: ARM32...'
    & cargo build --release --target armv7-linux-androideabi
    if ($LASTEXITCODE -ne 0) { throw 'ARM32 Rust compilation failed' }
}
finally { Pop-Location }

# Check the ABI exports expected by Kotlin/JNA before packaging anything.
$nm = Join-Path $bin 'llvm-nm.exe'
if (!(Test-Path $nm)) { throw "llvm-nm not found in NDK toolchain: $nm" }
foreach ($abi in @('aarch64-linux-android', 'armv7-linux-androideabi')) {
    $lib = "$Project\rust-core\target\$abi\release\libtgwsproxy.so"
    $symbols = (& $nm --dynamic --defined-only $lib | Out-String)
    if ($LASTEXITCODE -ne 0 -or $symbols -notmatch '\bGetStatsJson\b' -or
        $symbols -notmatch '\bGetProxyHealth\b' -or
        $symbols -notmatch '\bResetRouteFailures\b' -or $symbols -notmatch '\bStartProxy\b') {
        throw "Required Rust symbols are missing in $abi"
    }
}

$jni = "$Project\android\app\src\main\jniLibs"
New-Item -ItemType Directory -Force "$jni\arm64-v8a" | Out-Null
New-Item -ItemType Directory -Force "$jni\armeabi-v7a" | Out-Null
Copy-Item -Force "$Project\rust-core\target\aarch64-linux-android\release\libtgwsproxy.so" "$jni\arm64-v8a\libtgwsproxy.so"
Copy-Item -Force "$Project\rust-core\target\armv7-linux-androideabi\release\libtgwsproxy.so" "$jni\armeabi-v7a\libtgwsproxy.so"

if ($BuildType -eq 'Release') {
    $signingProperties = Join-Path $Project 'android\keystore.properties'
    if (!(Test-Path $signingProperties)) {
        throw "Release signing is not configured. Create android\keystore.properties first, or use -BuildType Debug."
    }
    $storeFileLine = Get-Content -LiteralPath $signingProperties |
        Where-Object { $_ -match '^\s*storeFile\s*=' } |
        Select-Object -First 1
    if (!$storeFileLine) { throw 'keystore.properties does not define storeFile.' }
    $storeFile = ($storeFileLine -split '=', 2)[1].Trim()
    $keystorePath = if ([System.IO.Path]::IsPathRooted($storeFile)) {
        $storeFile
    } else {
        Join-Path (Join-Path $Project 'android') $storeFile
    }
    if (!(Test-Path $keystorePath)) { throw "Release keystore not found: $keystorePath" }
}

Push-Location "$Project\android"
try {
    $variant = $BuildType.ToLowerInvariant()
    $gradleTask = "assemble$BuildType"
    if ($Clean) {
        Write-Host 'Cleaning Android Gradle outputs...'
        & .\gradlew.bat clean --max-workers=2 --console=plain
        if ($LASTEXITCODE -ne 0) { throw 'Android Gradle clean failed' }
    }
    Write-Host "Assembling the v3 $BuildType APK with native Rust libraries..."
    & .\gradlew.bat $gradleTask --max-workers=2 --console=plain
    if ($LASTEXITCODE -ne 0) { throw "Android Gradle $BuildType compilation failed" }
    $apkDirectory = ".\app\build\outputs\apk\$variant"
    $apks = @(Get-ChildItem "$apkDirectory\*.apk" -ErrorAction SilentlyContinue)
    if ($apks.Count -eq 0) { throw "No APKs were produced in $apkDirectory" }
    $apks | Select-Object FullName,Length,LastWriteTime | Format-List
}
finally { Pop-Location }

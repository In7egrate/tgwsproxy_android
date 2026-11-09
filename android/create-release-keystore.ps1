$ErrorActionPreference = "Stop"
$androidRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$keystoreDir = Join-Path $androidRoot "keystore"
$keystorePath = Join-Path $keystoreDir "ws-proxy-release.p12"
$propertiesPath = Join-Path $androidRoot "keystore.properties"
$keyAlias = "wsproxy-release"

if (Test-Path $keystorePath) { throw "Keystore already exists: $keystorePath. Back it up; do not overwrite it." }
if (Test-Path $propertiesPath) { throw "keystore.properties already exists. Review it instead of overwriting." }
if (-not (Get-Command keytool -ErrorAction SilentlyContinue)) { throw "keytool not found. Install JDK 17 and add its bin folder to PATH." }
New-Item -ItemType Directory -Force -Path $keystoreDir | Out-Null

$secure = Read-Host "Create a strong release keystore password (save it in a password manager)" -AsSecureString
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
$tempPass = Join-Path $env:TEMP ("wsproxy-keypass-" + [guid]::NewGuid().ToString("N") + ".txt")
try {
    $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
    if ($plain.Length -lt 12) { throw "Use at least 12 characters." }
    if ($plain -notmatch '^[!-~]+$') { throw "Use printable ASCII characters only so keytool and Java properties store the same password." }
    [System.IO.File]::WriteAllText($tempPass, $plain, [System.Text.Encoding]::ASCII)
    & keytool -genkeypair -v -keystore $keystorePath -storetype PKCS12 -alias $keyAlias -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=WS Proxy" -storepass:file $tempPass
    if ($LASTEXITCODE -ne 0) { throw "keytool failed; no signing config was written." }
    $escapedPassword = $plain.Replace('\', '\\').Replace(':', '\:').Replace('=', '\=').Replace('#', '\#').Replace('!', '\!').Replace(' ', '\ ')
    @(
        "storeFile=keystore/ws-proxy-release.p12"
        "storeType=PKCS12"
        "storePassword=$escapedPassword"
        "keyAlias=$keyAlias"
        "keyPassword=$escapedPassword"
    ) | Set-Content -LiteralPath $propertiesPath -Encoding ASCII
    Write-Host "Release keystore created: $keystorePath" -ForegroundColor Green
    Write-Host "Local Gradle config created: $propertiesPath" -ForegroundColor Green
    Write-Warning "Back up the .p12 file and password separately. Losing either prevents future in-place APK updates. Never commit these files."
} finally {
    if (Test-Path $tempPass) { Remove-Item -Force $tempPass }
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    $secure.Dispose()
    $plain = $null
}

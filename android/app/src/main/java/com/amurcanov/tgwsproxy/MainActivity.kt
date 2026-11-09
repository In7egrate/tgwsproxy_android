package com.amurcanov.tgwsproxy

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.draw.scale
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.amurcanov.tgwsproxy.ui.TgWsProxyTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject

private val WsBlack = Color(0xFF060505)
private val WsWhite = Color(0xFFFFFFFF)
private val WsSilver = Color(0xFFC0C0C0)

class MainActivity : ComponentActivity() {
    private val vpnConsent = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()) { result ->
        if (result.resultCode == Activity.RESULT_OK && ProxyService.isRunning &&
            !TunnelService.foreignVpnActive(this)) TunnelService.start(this)
    }

    override fun attachBaseContext(newBase: Context) {
        super.attachBaseContext(AppLocale.wrap(newBase))
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED)
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 102)
        setContent {
            TgWsProxyTheme(themeMode = "dark", dynamicColor = false, themePalette = "monochrome") {
                ProxyApp()
            }
        }
    }

    override fun onResume() {
        super.onResume()
        GithubUpdateManager.resumePendingInstall(this)
        if (ProxyConfigStore.tunnelEnabled(this) && !TunnelService.foreignVpnActive(this)) {
            val consent = android.net.VpnService.prepare(this)
            if (consent != null) vpnConsent.launch(consent)
        }
    }
}

@Composable
private fun ProxyApp() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val running by ProxyRuntime.isRunning.collectAsState()
    var page by rememberSaveable { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    var clients by remember { mutableStateOf<List<TgClient>>(emptyList()) }
    var chooseClient by remember { mutableStateOf(false) }
    var updateResult by remember { mutableStateOf<UpdateCheckResult?>(null) }
    var updateStatus by remember { mutableStateOf<String?>(null) }
    var updateBusy by remember { mutableStateOf(false) }
    var pendingDiagnosticExport by remember { mutableStateOf<String?>(null) }

    // System Back and the Android edge-swipe gesture should return from Settings
    // to the home screen instead of finishing MainActivity.
    BackHandler(enabled = page || chooseClient || message != null) {
        when {
            chooseClient -> chooseClient = false
            message != null -> message = null
            page -> page = false
        }
    }

    val diagnosticExportLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("text/plain")
    ) { uri ->
        val payload = pendingDiagnosticExport
        pendingDiagnosticExport = null
        if (uri != null && payload != null) {
            scope.launch(Dispatchers.IO) {
                val result = runCatching {
                    val output = context.contentResolver.openOutputStream(uri)
                        ?: error("Unable to open selected destination")
                    output.use { it.write(payload.toByteArray(Charsets.UTF_8)) }
                }
                withContext(Dispatchers.Main) {
                    message = if (result.isSuccess) "Диагностика сохранена" else
                        "Не удалось сохранить диагностику: ${result.exceptionOrNull()?.javaClass?.simpleName ?: "ошибка"}"
                }
            }
        }
    }

    fun updateResultMessage(result: UpdateCheckResult): String = when (result) {
        is UpdateCheckResult.Available -> "Доступна версия ${result.release.tagName}"
        is UpdateCheckResult.UpToDate -> "Установлена последняя версия ${result.latestVersion}"
        UpdateCheckResult.NoRelease -> "В репозитории пока нет опубликованных релизов."
        is UpdateCheckResult.ReleaseWithoutApk -> "Версия ${result.version} опубликована, но APK не прикреплён."
        is UpdateCheckResult.Failed -> "Не удалось проверить обновления: ${result.reason}"
    }

    fun checkUpdates() {
        scope.launch {
            updateBusy = true
            updateStatus = "Проверяем GitHub…"
            val result = withContext(Dispatchers.IO) { GithubUpdateManager.checkLatest(context) }
            updateResult = result
            updateStatus = updateResultMessage(result)
            updateBusy = false
        }
    }

    fun installUpdate() {
        val available = updateResult as? UpdateCheckResult.Available ?: return
        scope.launch {
            updateBusy = true
            updateStatus = "Скачиваем и проверяем APK…"
            val result = withContext(Dispatchers.IO) {
                runCatching { GithubUpdateManager.downloadAndValidate(context, available.release) }
            }
            result.onSuccess { apk ->
                val install = runCatching { GithubUpdateManager.requestInstall(context, apk) }
                updateStatus = install.fold(
                    onSuccess = { state -> when (state) {
                        InstallRequestState.Launched -> "Открыт системный установщик Android."
                        InstallRequestState.PermissionRequired -> "Разрешите установку для WS Proxy; после возврата откроется установщик."
                    } },
                    onFailure = { "Не удалось открыть установщик: ${it.message ?: "ошибка"}" }
                )
            }.onFailure {
                updateStatus = "Ошибка обновления: ${it.message ?: it.javaClass.simpleName}"
            }
            updateBusy = false
        }
    }

    LaunchedEffect(Unit) {
        clients = withContext(Dispatchers.IO) { TgClientRegistry.scan(context) }
        // Проверка выполняется при каждом открытии приложения. 404/пустые releases
        // считаются нормальным состоянием до публикации первого релиза форка.
        checkUpdates()
    }

    val applyToTelegram: () -> Unit = {
        scope.launch {
            clients = withContext(Dispatchers.IO) { TgClientRegistry.scan(context) }
            when (clients.size) {
                0 -> message = "Не найдено приложение Telegram, поддерживающее ссылку прокси."
                1 -> TgClientRegistry.open(context, clients.single(), ProxyConfigStore.load(context).link())
                else -> chooseClient = true
            }
        }
    }

    message?.let { txt ->
        AlertDialog(onDismissRequest = { message = null },
            title = { Text("WS Proxy") }, text = { Text(txt) },
            confirmButton = { TextButton(onClick = { message = null }) { Text("ОК") } })
    }

    if (chooseClient) {
        AlertDialog(onDismissRequest = { chooseClient = false },
            title = { Text("Клиент Telegram") },
            text = {
                Column(Modifier.heightIn(max = 340.dp).verticalScroll(rememberScrollState())) {
                    clients.forEach { client ->
                        TextButton(onClick = {
                            chooseClient = false
                            TgClientRegistry.open(context, client, ProxyConfigStore.load(context).link())
                        }, modifier = Modifier.fillMaxWidth()) {
                            Text(client.label, modifier = Modifier.fillMaxWidth())
                        }
                    }
                    TextButton(onClick = {
                        scope.launch { clients = withContext(Dispatchers.IO) { TgClientRegistry.scan(context) } }
                    }) { Text("Обновить список") }
                }
            }, confirmButton = { TextButton(onClick = { chooseClient = false }) { Text("Закрыть") } })
    }

    val availableUpdate = updateResult as? UpdateCheckResult.Available

    Scaffold(containerColor = WsBlack, contentWindowInsets = WindowInsets(0, 0, 0, 0)) { pad ->
        if (!page) {
            Column(
                Modifier.fillMaxSize().padding(pad).background(WsBlack),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                // Keep only the functional icons in the top bar. The brand title
                // belongs with the centered main control, not in the app bar.
                Box(Modifier.fillMaxWidth().statusBarsPadding().height(64.dp)) {
                    Row(
                        modifier = Modifier.align(Alignment.CenterEnd).padding(end = 10.dp),
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        if (availableUpdate != null) {
                            IconButton(onClick = { installUpdate() }, enabled = !updateBusy) {
                                Icon(
                                    painterResource(R.drawable.ic_update),
                                    contentDescription = "Установить обновление",
                                    tint = WsWhite
                                )
                            }
                        }
                        IconButton(onClick = { page = true }) {
                            Icon(
                                painterResource(R.drawable.ic_settings),
                                contentDescription = "Настройки",
                                tint = WsWhite
                            )
                        }
                    }
                }
                Column(
                    modifier = Modifier.fillMaxWidth().weight(1f).navigationBarsPadding(),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.Center
                ) {
                    Text(
                        "WS PROXY",
                        color = WsWhite,
                        style = MaterialTheme.typography.headlineMedium,
                        fontWeight = FontWeight.Bold,
                        textAlign = TextAlign.Center
                    )
                    Spacer(Modifier.height(30.dp))
                    PowerSwitchCard(checked = running, enabled = !busy) { requested ->
                        busy = true
                        scope.launch {
                            try {
                                if (requested) {
                                    val ok = ProxyController.startFromSavedSettings(context)
                                    if (!ok) message = "Не удалось запросить запуск службы"
                                } else ProxyController.stop(context)
                            } catch (t: Exception) {
                                message = t.message ?: "Ошибка службы"
                            } finally { busy = false }
                        }
                    }
                    Spacer(Modifier.height(26.dp))
                    Button(
                        onClick = applyToTelegram,
                        enabled = !busy,
                        modifier = Modifier.fillMaxWidth(0.86f).widthIn(max = 360.dp).height(62.dp),
                        shape = RoundedCornerShape(22.dp),
                        colors = ButtonDefaults.buttonColors(
                            containerColor = WsSilver,
                            contentColor = WsBlack,
                            disabledContainerColor = WsSilver.copy(alpha = 0.35f),
                            disabledContentColor = WsBlack.copy(alpha = 0.55f)
                        ),
                        contentPadding = PaddingValues(horizontal = 20.dp, vertical = 0.dp)
                    ) {
                        Icon(
                            painterResource(R.drawable.ic_telegram_send),
                            contentDescription = null,
                            tint = WsBlack,
                            modifier = Modifier.size(22.dp)
                        )
                        Spacer(Modifier.width(12.dp))
                        Text(
                            "Применить в Telegram",
                            fontSize = 17.sp,
                            fontWeight = FontWeight.SemiBold
                        )
                    }
                }
            }
        } else {
            Column(Modifier.fillMaxSize().padding(pad).background(WsBlack)) {
                Row(
                    Modifier.fillMaxWidth().statusBarsPadding().padding(horizontal = 18.dp, vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween
                ) {
                    Text("НАСТРОЙКИ", color = WsWhite,
                        style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.Bold)
                    TextButton(onClick = { page = false }) { Text("Готово", color = WsSilver) }
                }
                SettingsPage(
                    modifier = Modifier.weight(1f).navigationBarsPadding(),
                    onExportDiagnostics = {
                        scope.launch(Dispatchers.IO) {
                            val payload = DiagnosticStore.exportText(context)
                            withContext(Dispatchers.Main) {
                                pendingDiagnosticExport = payload
                                diagnosticExportLauncher.launch("ws-proxy-diagnostics-${System.currentTimeMillis()}.txt")
                            }
                        }
                    },
                    onError = { message = it },
                    onBack = { page = false },
                    onOpenGithub = {
                        runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(GithubUpdateManager.DEVELOPER_URL)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
                            .onFailure { message = "Не удалось открыть GitHub: ${it.message}" }
                    },
                    onCheckUpdates = { checkUpdates() },
                    onInstallUpdate = { installUpdate() },
                    updateAvailable = availableUpdate != null,
                    updateTag = availableUpdate?.release?.tagName,
                    updateStatus = updateStatus,
                    updateBusy = updateBusy
                )
            }
        }
    }
}

@Composable
private fun PowerSwitchCard(
    checked: Boolean,
    enabled: Boolean,
    onCheckedChange: (Boolean) -> Unit
) {
    val shape = RoundedCornerShape(30.dp)
    Box(
        modifier = Modifier
            .fillMaxWidth(0.86f)
            .widthIn(max = 360.dp)
            .height(108.dp)
            .clip(shape)
            .background(WsWhite.copy(alpha = 0.055f))
            .border(1.dp, WsSilver.copy(alpha = 0.34f), shape),
        contentAlignment = Alignment.Center
    ) {
        Switch(
            checked = checked,
            onCheckedChange = onCheckedChange,
            enabled = enabled,
            modifier = Modifier
                .scale(1.22f)
                .semantics {
                    contentDescription = if (checked) "Прокси включён" else "Прокси выключен"
                },
            colors = SwitchDefaults.colors(
                checkedThumbColor = WsBlack,
                checkedTrackColor = WsSilver,
                checkedBorderColor = WsSilver,
                uncheckedThumbColor = WsWhite,
                uncheckedTrackColor = WsBlack,
                uncheckedBorderColor = WsSilver,
                disabledCheckedThumbColor = WsBlack.copy(alpha = 0.65f),
                disabledCheckedTrackColor = WsSilver.copy(alpha = 0.45f),
                disabledUncheckedThumbColor = WsSilver.copy(alpha = 0.55f),
                disabledUncheckedTrackColor = WsBlack.copy(alpha = 0.45f)
            )
        )
    }
}

@Composable
private fun SettingsPage(
    modifier: Modifier = Modifier,
    onExportDiagnostics: () -> Unit,
    onError: (String) -> Unit,
    onBack: () -> Unit,
    onOpenGithub: () -> Unit,
    onCheckUpdates: () -> Unit,
    onInstallUpdate: () -> Unit,
    updateAvailable: Boolean,
    updateTag: String?,
    updateStatus: String?,
    updateBusy: Boolean
) {
    val context = LocalContext.current
    var auto by remember { mutableStateOf(AutoProxyProfile.isAuto(context)) }
    var cfg by remember { mutableStateOf(ProxyConfigStore.load(context)) }
    val prefs = remember(context) { context.getSharedPreferences(ProxyConfigStore.PREFS, 0) }
    var port by remember { mutableStateOf(prefs.getInt("v3_manual_port", cfg.port).toString()) }
    var pool by remember { mutableStateOf(prefs.getInt("v3_manual_pool", cfg.poolSize.coerceAtLeast(6)).toString()) }
    var buffer by remember { mutableStateOf(prefs.getInt("v3_manual_buffer", cfg.bufKb).toString()) }
    var logMb by remember { mutableStateOf(prefs.getInt("v3_manual_log", cfg.logMaxMb).toString()) }
    var autostart by remember { mutableStateOf(ProxyConfigStore.autostart(context)) }
    var advanced by remember { mutableStateOf(false) }
    var cf by remember { mutableStateOf(cfg.cfproxy) }
    var host by remember { mutableStateOf(cfg.host) }
    var dc by remember { mutableStateOf(cfg.dcIp.joinToString("\n")) }
    var workerDomains by remember { mutableStateOf(cfg.cfWorkerDomains.joinToString(",")) }
    var workerEnabled by remember { mutableStateOf(cfg.cfWorkerEnabled) }

    Column(modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text("ПОДКЛЮЧЕНИЕ", color = WsSilver, style = MaterialTheme.typography.titleMedium)
        CopyProxyRow(proxyUrl = cfg.link()) {
            val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            clipboard.setPrimaryClip(ClipData.newPlainText("WS Proxy", cfg.link()))
            Toast.makeText(context, "Ссылка прокси скопирована", Toast.LENGTH_SHORT).show()
        }
        SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
            SegmentedButton(selected = auto, onClick = { auto = true },
                shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2)) { Text("Авто") }
            SegmentedButton(selected = !auto, onClick = { auto = false },
                shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2)) { Text("Свои") }
        }
        if (auto) {
            val p = remember(auto) { AutoProxyProfile.choose(context) }
            Text("Параметры подбираются автоматически. Пул WS от 6 до ${p.maxPool}, буфер ${p.bufKb} КБ.",
                color = WsSilver.copy(alpha = 0.82f))
            Text("Порт: 1443", color = WsWhite, style = MaterialTheme.typography.bodyMedium)
        } else {
            NumberField("Порт", port, { port = it })
            NumberField("Пул WS (минимум 6)", pool, { pool = it })
            NumberField("TCP-буфер, КБ", buffer, { buffer = it })
            NumberField("Лимит внутренней диагностики, МБ", logMb, { logMb = it })
        }
        HorizontalDivider(color = WsSilver.copy(alpha = 0.25f))
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween) {
            Column {
                Text("Автозапуск", color = WsWhite, style = MaterialTheme.typography.titleMedium)
                Text("После включения телефона", color = WsSilver.copy(alpha = 0.75f), style = MaterialTheme.typography.bodySmall)
            }
            Switch(checked = autostart, onCheckedChange = {
                autostart = it
                ProxyConfigStore.setAutostart(context, it)
            })
        }
        HorizontalDivider(color = WsSilver.copy(alpha = 0.25f))
        Text("ОБНОВЛЕНИЯ", color = WsSilver, style = MaterialTheme.typography.titleMedium)
        OutlinedButton(onClick = onCheckUpdates, enabled = !updateBusy, modifier = Modifier.fillMaxWidth()) {
            if (updateBusy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
            else Icon(painterResource(R.drawable.ic_update), contentDescription = null, tint = WsWhite)
            Spacer(Modifier.width(10.dp))
            Text("Проверить обновление", color = WsWhite)
        }
        if (!updateStatus.isNullOrBlank()) {
            Text(updateStatus, color = WsSilver.copy(alpha = 0.9f), style = MaterialTheme.typography.bodySmall)
        }
        if (updateAvailable && !updateTag.isNullOrBlank()) {
            Button(onClick = onInstallUpdate, enabled = !updateBusy, modifier = Modifier.fillMaxWidth()) {
                Text("Скачать и установить $updateTag")
            }
        }
        OutlinedButton(onClick = onOpenGithub, modifier = Modifier.fillMaxWidth()) {
            Icon(painterResource(R.drawable.ic_github), contentDescription = null, tint = WsWhite)
            Spacer(Modifier.width(10.dp))
            Text("GitHub разработчика", color = WsWhite)
        }
        HorizontalDivider(color = WsSilver.copy(alpha = 0.25f))
        Text("ДИАГНОСТИКА", color = WsSilver, style = MaterialTheme.typography.titleMedium)
        OutlinedButton(onClick = onExportDiagnostics, modifier = Modifier.fillMaxWidth()) {
            Text("Экспорт диагностики", color = WsWhite)
        }
        Text(
            "Файл содержит технические события и метрики, но не содержимое сообщений или секреты.",
            style = MaterialTheme.typography.bodySmall,
            color = WsSilver.copy(alpha = 0.75f)
        )
        TextButton(onClick = { advanced = !advanced }, modifier = Modifier.fillMaxWidth()) {
            Text(if (advanced) "Скрыть дополнительные настройки" else "Дополнительные настройки", color = WsSilver)
        }
        if (advanced) {
            OutlinedTextField(value = host, onValueChange = { host = it },
                label = { Text("Адрес прослушивания") }, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = dc, onValueChange = { dc = it },
                label = { Text("DC:IP — по одному на строку") }, modifier = Modifier.fillMaxWidth(), minLines = 2)
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(checked = cf, onCheckedChange = { cf = it }); Text("Cloudflare fallback", color = WsWhite)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(checked = workerEnabled, onCheckedChange = { workerEnabled = it })
                Text("Cloudflare Worker", color = WsWhite)
            }
            if (workerEnabled) OutlinedTextField(value = workerDomains, onValueChange = { workerDomains = it },
                label = { Text("Worker-домены") }, modifier = Modifier.fillMaxWidth())
        }
        Button(onClick = {
            val chosenPort = if (auto) 1443 else port.toIntOrNull()
            val chosenPool = if (auto) 6 else pool.toIntOrNull()
            val chosenBuffer = if (auto) 256 else buffer.toIntOrNull()
            val chosenLog = if (auto) 5 else logMb.toIntOrNull()
            if (chosenPort == null || chosenPort !in 1024..65535 ||
                chosenPool == null || chosenPool !in 6..16 ||
                chosenBuffer == null || chosenBuffer !in 64..1024 ||
                chosenLog == null || chosenLog !in 1..50) {
                onError("Проверьте порт, пул, буфер и лимит диагностики")
            } else {
                if (!auto) prefs.edit().putInt("v3_manual_port", chosenPort)
                    .putInt("v3_manual_pool", chosenPool)
                    .putInt("v3_manual_buffer", chosenBuffer)
                    .putInt("v3_manual_log", chosenLog).apply()
                AutoProxyProfile.setMode(context, auto)
                val p = if (auto) AutoProxyProfile.choose(context) else null
                val err = ProxyConfigStore.save(context, host, chosenPort.toString(), cfg.secret,
                    dc, (p?.poolSize ?: chosenPool).toString(),
                    (p?.bufKb ?: chosenBuffer).toString(), (p?.logMaxMb ?: chosenLog).toString(), false,
                    cf, cfg.cfproxyUserDomainEnabled, cfg.cfproxyUserDomains.joinToString(","),
                    workerEnabled, workerDomains, cfg.forceTestDc)
                if (err != null) onError(err) else {
                    cfg = ProxyConfigStore.load(context)
                    if (ProxyService.isRunning) ProxyController.restart(context)
                    Toast.makeText(context, "Настройки сохранены", Toast.LENGTH_SHORT).show()
                    onBack()
                }
            }
        }, modifier = Modifier.fillMaxWidth()) { Text("Сохранить настройки") }
        Spacer(Modifier.height(16.dp))
    }
}

@Composable
private fun CopyProxyRow(proxyUrl: String, onCopy: () -> Unit) {
    Row(
        Modifier.fillMaxWidth()
            .clip(RoundedCornerShape(22.dp))
            .background(Color.White.copy(alpha = 0.055f))
            .border(1.dp, WsSilver.copy(alpha = 0.35f), RoundedCornerShape(22.dp))
            .clickable(onClick = onCopy)
            .padding(horizontal = 16.dp, vertical = 18.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text(proxyUrl, color = WsWhite.copy(alpha = 0.88f), maxLines = 1,
            modifier = Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
        Spacer(Modifier.width(12.dp))
        Icon(painterResource(R.drawable.ic_copy), contentDescription = "Скопировать ссылку", tint = WsWhite)
    }
}

@Composable
private fun NumberField(title: String, value: String, change: (String) -> Unit) {
    OutlinedTextField(value = value, onValueChange = change, label = { Text(title) },
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        singleLine = true, modifier = Modifier.fillMaxWidth())
}

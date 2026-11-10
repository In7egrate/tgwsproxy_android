# Changelog

## [1.0.1] - Unreleased

### Network reliability
- Added per-data-center, per-route circuit breakers with bounded exponential cooldown, jitter, and a single half-open recovery probe.
- Applied route gating to direct Telegram WebSocket, CF proxy, CF Worker, pooled WS refill, and TCP fallback connection attempts.
- Added a global outbound-handshake budget and per-route single-flight serialization to prevent duplicate concurrent handshakes from creating connection storms.
- Route success is now recorded after the first upstream response payload rather than immediately after a successful WebSocket handshake.
- Added first-byte latency aggregates and route failures before the first upstream response to native diagnostics.
- Preserved cancellation handling for half-open probes and bounded probe leases so interrupted attempts cannot leave a route permanently blocked.

### Android network diagnostics
- Track default-network identity to avoid resetting pools for stale `onLost` callbacks after a replacement network is already active.
- Record transport class (cellular, Wi-Fi, VPN, Ethernet, or other), validation, metered state, and roaming capability without exporting IP addresses, SSIDs, or credentials.
- Reset route state only on actual default-network/route-class transitions rather than every callback.
- Include native recovery status and more useful service-start failure details in diagnostics.

### Version
- Android `versionName=1.0.1`, `versionCode=204`.

### Validation
- Added Rust circuit-breaker unit tests and Android transport-classification tests.
- Added GitHub Actions CI to run Rust tests, cross-compile both supported ARM native libraries, and build Android debug and release variants.
- Release APKs and signing verification are produced only by the tagged release workflow; this unreleased branch does not publish APKs.

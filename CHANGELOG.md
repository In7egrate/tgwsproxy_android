# Changelog

## 1.0.1

This maintenance release improves route selection and mobile-network recovery.

- Adds per-DC/per-route circuit-breaker states with a single Half-Open recovery probe.
- Limits simultaneous direct WS dials globally and serializes dials to the same route.
- Uses bounded exponential cooldowns with jitter for circuit breaking and WS pool refill.
- Distinguishes WS connect/handshake timing from first upstream application data.
- Updates route success only after an upstream application frame is received.
- Enforces a 15-second first-upstream-byte deadline after the first client payload, while releasing idle Half-Open probes without penalizing client-idle sessions.
- Tracks WebSocket Ping/Pong RTT and feeds ping failures back into route health.
- Tracks first-byte latency and circuit-breaker state in diagnostic exports.
- Applies route health checks to direct Telegram WS, Cloudflare proxy, and Cloudflare Worker fallback paths.
- Preserves the selected Telegram hostname for pooled WS sockets so pool hits can be attributed to a route.
- Improves cancellation handling and drains parallel Cloudflare attempts before releasing their resources.
- Records Android default-network transport, validation, metering, VPN state, and network handovers without exporting IP addresses, SSIDs, or BSSIDs.
- Adds service-start timing and failure context to diagnostics.

No MTProto cryptographic or wire-protocol semantics are intentionally changed.

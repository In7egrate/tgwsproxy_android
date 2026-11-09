# MTProto splitter fuzz target

This target feeds arbitrary byte streams to the optional MTProto frame splitter
using all supported framing modes. It is intended to detect panics, out-of-bounds
accesses, and pathological state transitions; it does not validate MTProto
cryptographic interoperability.

Install `cargo-fuzz`, then from `rust-core/fuzz` run:

```powershell
cargo fuzz run msg_splitter -- -max_total_time=120
```

Keep any discovered corpus entries under version control after triage. This
target has not been executed in the current build environment.

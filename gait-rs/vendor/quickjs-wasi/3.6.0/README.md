# QuickJS-WASI 3.6.0 vendor artifact

- **Source:** `quickjs-wasi` npm package, version **3.6.0**, from the upstream repository `https://github.com/vercel-labs/quickjs-wasi` (QuickJS-NG compiled to WebAssembly).
- **Input:** `quickjs.wasm`, copied from the supplied extracted package at task implementation time. No network download is performed by the build.
- **License:** MIT (package metadata; the package contains QuickJS-NG and its upstream licensing notices).
- **SHA-256:** `b006d95d9475edf7c6648cc3eb391d3b780efdd99022fbfbb470f2359da460ff`

The binary is a minimal reactor ABI. Its `wasi_snapshot_preview1` imports are explicitly linked by the host and do not imply WASI capabilities or preopens.

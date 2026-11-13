# rezzy FFI

`rezzy-ffi` exposes event ID calculation, content-hash verification, and JSONL
event reports through a C ABI. The C declarations are in
[`include/rezzy.h`](include/rezzy.h). Returned buffers are owned by the library;
release them with `rezzy_free`.

The Go package lives at `github.com/gamesguru/rezzy/rezzy-ffi/go/rezzy`. It uses
cgo and statically links `librezzy_ffi.a` through `pkg-config`. `go get`
installs only Go source; the matching native library, header, and `rezzy-ffi.pc`
must also be installed for Linux before building an importer. From this
directory, build and check the binding and Go example with:

```sh
make format lint tidy test build
```

For local consumers, `make rust/install PREFIX="$HOME/.local"` from the
repository root installs the native artifacts; set
`PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig"` when building Go code. Go module
releases use `rezzy-ffi/vX.Y.Z` tags.

The Go example imports the package:

```sh
go run ./go/examples/hash event-id 12 event.json
go run ./go/examples/hash verify-content-hash 12 event.json
go run ./go/examples/hash hash-events 12 events.jsonl
```

Python uses `ctypes` and loads the debug library from the workspace:

```sh
python3 examples/event_id.py event-id 12 event.json
python3 examples/event_id.py verify-content-hash 12 event.json
python3 examples/event_id.py hash-events 12 events.jsonl
```

The Python example requires `../target/debug/librezzy_ffi.so`. The Go package
statically links `../target/release/librezzy_ffi.a`; Docker images need that
archive built for their target architecture plus the matching cgo toolchain
(`CGO_ENABLED=1`). Go builds with cgo disabled are unsupported.

# rezzy FFI

`rezzy-ffi` exposes event hashing, state HAMT operations, and reconciliation
through a native C ABI. The C declarations are in
[`include/rezzy.h`](include/rezzy.h). Returned buffers are owned by the library;
release them with `rezzy_free`.

The ABI can be used directly from C or C++, or by other native callers that link
the static or shared library. HAMT and reconciliation requests use UTF-8 JSON
buffers and return UTF-8 JSON buffers; the low-level GF(2^256) sketch functions
use flat binary buffers. These entry points do not require the Go package.

Supported JSON operations:

- `rezzy_state_hamt`: build a room state root identity, look up an entry in a
  supplied state set, or diff two state sets.
- `rezzy_recon`: calculate an event ID digest, build a sketch, or decode the
  difference between local event IDs and a remote sketch.

The state HAMT calls build from entries supplied in each request. They return
root and state-group identities or entry differences; they do not yet provide
storage callbacks for loading and saving persisted HAMT nodes.

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
PKG_CONFIG_PATH="$(pwd)/../target/pkgconfig" go run ./go/examples/hash event-id 12 event.json
PKG_CONFIG_PATH="$(pwd)/../target/pkgconfig" go run ./go/examples/hash verify-content-hash 12 event.json
PKG_CONFIG_PATH="$(pwd)/../target/pkgconfig" go run ./go/examples/hash hash-events 12 events.jsonl
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

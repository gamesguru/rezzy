# Rezzy Go bindings

Import `github.com/gamesguru/rezzy/rezzy-ffi/go/rezzy` and call `EventID`,
`VerifyContentHash`, `HashEventsJSONL`, `StateHAMTRoot`, `StateHAMTLookup`,
`StateHAMTDiff`, `ReconDigest`, `ReconSketch`, `ReconDiff`, `Kernel`, and
`Client`.

The new HAMT and reconciliation APIs use the same static library and pkg-config
setup as the hashing API:

```go
rootHash, stateGroupID, err := rezzy.StateHAMTRoot(roomID, entries)
value, found, err := rezzy.StateHAMTLookup(roomID, entries, eventType, stateKey)
added, removed, err := rezzy.StateHAMTDiff(roomID, before, after)

digest, count, err := rezzy.ReconDigest(eventIDs)
sketch, err := rezzy.ReconSketch(remoteEventIDs, 4)
localOnly, remoteOnly, err := rezzy.ReconDiff(localEventIDs, sketch, 4)
```

Native operations currently support Linux with cgo and statically link the
native archive. Install the matching archive, header, and `rezzy-ffi.pc` before
using them. On builds without Linux+cgo, the package still compiles and native
calls return `rezzy.ErrUnavailable`. From the Rezzy checkout, run:

```sh
make -C rezzy-ffi format lint tidy test build
```

`rezzy-ffi/go/examples/hash` is a small CLI in the same module that imports the
binding directly.

After a `rezzy-ffi/vX.Y.Z` module tag is published, other Go projects can add
the Go dependency with `go get`. The native install remains a separate
per-platform step; `go get` does not build or install Rust archives.

For a local install, run `make rust/install PREFIX="$HOME/.local"` from the
repository root, then set `PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig"`. For a
Docker image, install a `librezzy_ffi.a` and `rezzy-ffi.pc` built for the
image's target architecture. The image also needs a C linker and the matching
cgo toolchain (`CGO_ENABLED=1`) to run native operations.

## Requester loop

`Kernel` tracks one room's `algebraic_v1` digest and strata. Persist it with
`Bytes` and restore it with `KernelFromBytes`. `Digest` returns the digest and
count; `Strata` returns the encoded 32-strata field for `room_digest/strata`.
`NewClient` snapshots a kernel;
feed its first remote digest and each decoded bucket response to `Step` as JSON.
The result is the next action as JSON. The host remains responsible for
federation requests and event handling. See the `rezzy_client_step` contract in
`include/rezzy.h` for the input and output shapes.

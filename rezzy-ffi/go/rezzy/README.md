# Rezzy Go bindings

Import `github.com/gamesguru/rezzy/rezzy-ffi/go/rezzy` and call `EventID`,
`VerifyContentHash`, or `HashEventsJSONL`.

The package currently supports Linux with cgo and statically links the native
archive. From the Rezzy checkout, run the module's checks with:

```sh
make -C rezzy-ffi format lint tidy test build
```

`rezzy-ffi/go/examples/hash` is a small CLI in the same module that imports the
binding directly.

After this module is pushed, other Go projects can add it with `go get`; they
must also provide the native archive, because cgo cannot build the Rust library
from Go source alone.

For a Docker image, build `librezzy_ffi.a` for the image's target architecture
and install it under `/usr/local/lib` before compiling the Go binary. The
image also needs a C linker and the platform's cgo toolchain (`CGO_ENABLED=1`).
Use a separate Rust target and matching C cross-compiler for each target
architecture. Go builds with cgo disabled are unsupported.

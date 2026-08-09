#!/usr/bin/env bash
# Runs a cargo subcommand with the `tls-native` (system OpenSSL) TLS backend
# first, and falls back to `tls-rustls` (pure-Rust, no system OpenSSL) if
# that fails to build — e.g. openssl-sys lacking pregenerated bindings for a
# too-new system OpenSSL, which is a link failure, not something the code
# can catch and retry at runtime.
#
# Usage: cargo-tls-fallback.sh <cargo-subcommand> [cargo args...]
# Example: cargo-tls-fallback.sh install --locked --path . --bin rezzy
set -euo pipefail

SUBCOMMAND="$1"
shift

if cargo "$SUBCOMMAND" --locked --features cli "$@"; then
	exit 0
fi

echo "warning: build with the native-tls/OpenSSL backend failed; falling back to rustls (--features cli-rustls)." >&2
echo "This should not normally happen — see tls-native's doc comment in Cargo.toml if it keeps happening." >&2
exec cargo "$SUBCOMMAND" --locked --no-default-features --features cli-rustls "$@"

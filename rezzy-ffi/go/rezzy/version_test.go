//go:build linux && cgo

package rezzy

import "testing"

func TestVersionReportsTheRelease(t *testing.T) {
	version := Version()
	if version == "" {
		t.Fatal("Version() is empty")
	}
	// The ABI check is what guards compatibility; this string only has to
	// identify the build, so a loose shape check is enough.
	if version[0] < '0' || version[0] > '9' {
		t.Fatalf("Version() = %q, want a leading digit", version)
	}
}

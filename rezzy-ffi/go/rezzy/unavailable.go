//go:build !(linux && cgo)

package rezzy

import "errors"

// ErrUnavailable is returned by every function when the package was built
// without the native library: on a non-Linux platform or with CGO_ENABLED=0.
// The package still compiles there so that importers can cross-build.
var ErrUnavailable = errors.New("rezzy: native library not linked (needs linux and cgo)")

// EventID is unavailable in this build.
func EventID(roomVersion string, event []byte) (string, error) {
	return "", ErrUnavailable
}

// VerifyContentHash is unavailable in this build.
func VerifyContentHash(roomVersion string, event []byte) error {
	return ErrUnavailable
}

// HashEventsJSONL is unavailable in this build.
func HashEventsJSONL(roomVersion string, jsonl []byte) ([]byte, error) {
	return nil, ErrUnavailable
}

// StateHAMTRoot is unavailable in this build.
func StateHAMTRoot(roomID string, entries []HAMTEntry) (rootHash string, stateGroupID string, err error) {
	return "", "", ErrUnavailable
}

// StateHAMTLookup is unavailable in this build.
func StateHAMTLookup(roomID string, entries []HAMTEntry, eventType, stateKey string) (string, bool, error) {
	return "", false, ErrUnavailable
}

// StateHAMTDiff is unavailable in this build.
func StateHAMTDiff(roomID string, a, b []HAMTEntry) (added []HAMTEntry, removed []HAMTEntry, err error) {
	return nil, nil, ErrUnavailable
}

// ReconDigest is unavailable in this build.
func ReconDigest(eventIDs []string) (digest string, count uint64, err error) {
	return "", 0, ErrUnavailable
}

// ReconSketch is unavailable in this build.
func ReconSketch(eventIDs []string, capacity int) ([]byte, error) {
	return nil, ErrUnavailable
}

// ReconDiff is unavailable in this build.
func ReconDiff(localEventIDs []string, remoteSketch []byte, capacity int) (localOnly []string, remoteOnly [][32]byte, err error) {
	return nil, nil, ErrUnavailable
}

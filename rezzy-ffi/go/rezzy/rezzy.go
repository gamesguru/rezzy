//go:build linux && cgo

// Package rezzy exposes rezzy's event hashing C ABI to Go applications.
//
// The Linux package uses cgo and statically links librezzy_ffi.a. Build that
// archive for the target architecture and install it in /usr/local/lib, or
// leave it in this repository's target/{debug,release} directory.
package rezzy

/*
#cgo CFLAGS: -I${SRCDIR}/../../include
#cgo linux LDFLAGS: -L${SRCDIR}/../../../target/release -L${SRCDIR}/../../../target/debug -L/usr/local/lib -l:librezzy_ffi.a -lpthread -ldl -lm
#include <stdlib.h>
#include "rezzy.h"
*/
import "C"

import (
	"fmt"
	"unsafe"
)

const (
	eventIDOperation = iota
	verifyContentHashOperation
	hashEventsOperation
)

// EventID returns the reference-hash event ID of a JSON event under
// roomVersion. Parsing and hashing failures, including opaque-ID versions,
// return an error.
func EventID(roomVersion string, event []byte) (string, error) {
	result, err := call(roomVersion, event, eventIDOperation)
	if err != nil {
		return "", err
	}
	return string(result), nil
}

// VerifyContentHash checks the event's hashes.sha256 under roomVersion.
func VerifyContentHash(roomVersion string, event []byte) error {
	_, err := call(roomVersion, event, verifyContentHashOperation)
	return err
}

// HashEventsJSONL returns rezzy's JSON event report for each nonblank line.
// Per-event failures are included in the report; invalid UTF-8 or empty input
// returns an error.
func HashEventsJSONL(roomVersion string, jsonl []byte) ([]byte, error) {
	return call(roomVersion, jsonl, hashEventsOperation)
}

func call(roomVersion string, input []byte, operation int) ([]byte, error) {
	version := C.CString(roomVersion)
	defer C.free(unsafe.Pointer(version))
	var out *C.uint8_t
	var outLen C.size_t
	var empty C.uint8_t
	first := &empty
	if len(input) > 0 {
		first = (*C.uint8_t)(unsafe.Pointer(&input[0]))
	}
	var status C.int
	switch operation {
	case verifyContentHashOperation:
		status = C.rezzy_verify_content_hash(version, first, C.size_t(len(input)), &out, &outLen)
	case hashEventsOperation:
		status = C.rezzy_hash_events(version, first, C.size_t(len(input)), &out, &outLen)
	default:
		status = C.rezzy_event_id(version, first, C.size_t(len(input)), &out, &outLen)
	}
	var result []byte
	if out != nil {
		if uint64(outLen) > uint64(^uint32(0)>>1) {
			C.rezzy_free(out, outLen)
			return nil, fmt.Errorf("rezzy: response is too large for Go's C buffer conversion")
		}
		result = C.GoBytes(unsafe.Pointer(out), C.int(outLen))
		C.rezzy_free(out, outLen)
	}
	if status != C.REZZY_OK {
		if len(result) == 0 {
			return nil, fmt.Errorf("rezzy: status %d", int(status))
		}
		return nil, fmt.Errorf("rezzy: %s", result)
	}
	return result, nil
}

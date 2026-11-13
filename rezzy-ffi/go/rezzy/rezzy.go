//go:build linux && cgo

// Package rezzy exposes rezzy's event hashing C ABI to Go applications.
//
// The Linux package uses cgo and statically links librezzy_ffi.a. Install the
// native library and rezzy-ffi.pc with `make rust/install`, or use the
// repository's `make -C rezzy-ffi native` development target.
package rezzy

/*
#cgo pkg-config: rezzy-ffi
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

var abiVersionErr = checkABIVersion(
	uint32(C.REZZY_ABI_VERSION),
	uint32(C.rezzy_abi_version()),
)

func checkABIVersion(expected, actual uint32) error {
	if actual != expected {
		return fmt.Errorf("rezzy: C ABI version mismatch: header %d, library %d", expected, actual)
	}
	return nil
}

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
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
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

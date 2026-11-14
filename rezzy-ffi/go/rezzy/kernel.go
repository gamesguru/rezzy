//go:build linux && cgo

package rezzy

/*
#cgo pkg-config: rezzy-ffi
#include <stdlib.h>
#include "rezzy.h"
*/
import "C"

import (
	"encoding/json"
	"errors"
	"fmt"
	"unsafe"
)

// Kernel is the running reconciliation state for one room: the digest and
// strata that servers exchange (MSC4521 algebraic_v1). Update it as events are
// accepted instead of rebuilding from every event ID.
//
// A Kernel is not safe for concurrent use. Call Close when finished.
type Kernel struct {
	handle *C.RezzyKernel
}

// ErrKernelClosed is returned when a closed Kernel is used.
var ErrKernelClosed = errors.New("rezzy: kernel is closed")

// NewKernel returns an empty kernel.
func NewKernel() (*Kernel, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	return &Kernel{handle: C.rezzy_kernel_new()}, nil
}

// KernelFromBytes rebuilds a kernel from the record written by Bytes.
func KernelFromBytes(record []byte) (*Kernel, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	var empty C.uint8_t
	first := &empty
	if len(record) > 0 {
		first = (*C.uint8_t)(unsafe.Pointer(&record[0]))
	}
	var handle *C.RezzyKernel
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_kernel_from_bytes(first, C.size_t(len(record)), &handle, &out, &outLen)
	if _, err := finish(status, out, outLen); err != nil {
		return nil, err
	}
	return &Kernel{handle: handle}, nil
}

// Close releases the kernel. It is safe to call more than once.
func (k *Kernel) Close() {
	if k != nil && k.handle != nil {
		C.rezzy_kernel_free(k.handle)
		k.handle = nil
	}
}

// Insert adds an event ID. Insert each event once: inserting it twice cancels
// it out of the digest.
func (k *Kernel) Insert(eventID string) error {
	return k.update(eventID, true)
}

// Remove takes an event ID out. Only remove IDs that were inserted.
func (k *Kernel) Remove(eventID string) error {
	return k.update(eventID, false)
}

func (k *Kernel) update(eventID string, insert bool) error {
	if k == nil || k.handle == nil {
		return ErrKernelClosed
	}
	id := []byte(eventID)
	var empty C.uint8_t
	first := &empty
	if len(id) > 0 {
		first = (*C.uint8_t)(unsafe.Pointer(&id[0]))
	}
	var out *C.uint8_t
	var outLen C.size_t
	var status C.int
	if insert {
		status = C.rezzy_kernel_insert(k.handle, first, C.size_t(len(id)), &out, &outLen)
	} else {
		status = C.rezzy_kernel_remove(k.handle, first, C.size_t(len(id)), &out, &outLen)
	}
	_, err := finish(status, out, outLen)
	return err
}

// Bytes returns the fixed-size record to persist; KernelFromBytes reads it.
func (k *Kernel) Bytes() ([]byte, error) {
	if k == nil || k.handle == nil {
		return nil, ErrKernelClosed
	}
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_kernel_to_bytes(k.handle, &out, &outLen)
	return finish(status, out, outLen)
}

// Digest returns the current digest (base64url) and the number of events.
func (k *Kernel) Digest() (digest string, count uint64, err error) {
	if k == nil || k.handle == nil {
		return "", 0, ErrKernelClosed
	}
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_kernel_digest(k.handle, &out, &outLen)
	response, err := finish(status, out, outLen)
	if err != nil {
		return "", 0, err
	}
	var parsed struct {
		Digest string `json:"digest"`
		Count  uint64 `json:"count"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return "", 0, fmt.Errorf("rezzy: decode kernel digest: %w", err)
	}
	return parsed.Digest, parsed.Count, nil
}

// Strata returns the 32 base64url-encoded coordinates used by the
// room_digest/strata response.
func (k *Kernel) Strata() ([]string, error) {
	if k == nil || k.handle == nil {
		return nil, ErrKernelClosed
	}
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_kernel_strata(k.handle, &out, &outLen)
	response, err := finish(status, out, outLen)
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Strata []string `json:"strata"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode kernel strata: %w", err)
	}
	if len(parsed.Strata) != 32 {
		return nil, fmt.Errorf("rezzy: kernel returned %d strata, want 32", len(parsed.Strata))
	}
	return parsed.Strata, nil
}

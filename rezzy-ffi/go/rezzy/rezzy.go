//go:build linux && cgo

// Package rezzy exposes rezzy's event hashing, state HAMT, and reconciliation
// C ABI to Go applications.
//
// The Linux package uses cgo and statically links librezzy.a. Install the
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
	"encoding/hex"
	"encoding/json"
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

// Version returns the release version of the linked native library, such as
// "0.6.0-rc.1". Use it to report which build is loaded; use the package's
// load-time ABI check, not this string, to decide compatibility.
func Version() string {
	return C.GoString(C.rezzy_version())
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

// StateHAMTRoot returns the structural root hash and portable state-group ID
// for a room state map.
func StateHAMTRoot(roomID string, entries []HAMTEntry) (rootHash string, stateGroupID string, err error) {
	payload, err := json.Marshal(map[string]any{
		"op":      "root",
		"entries": entries,
	})
	if err != nil {
		return "", "", err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return "", "", err
	}
	var parsed struct {
		RootHash     string `json:"root_hash"`
		StateGroupID string `json:"state_group_id"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return "", "", fmt.Errorf("rezzy: decode state HAMT root: %w", err)
	}
	return parsed.RootHash, parsed.StateGroupID, nil
}

// StateHAMTLookup returns the event ID for one (event type, state key) pair.
// The boolean is false when the entry is absent.
func StateHAMTLookup(roomID string, entries []HAMTEntry, eventType, stateKey string) (string, bool, error) {
	payload, err := json.Marshal(map[string]any{
		"op":         "lookup",
		"entries":    entries,
		"event_type": eventType,
		"state_key":  stateKey,
	})
	if err != nil {
		return "", false, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return "", false, err
	}
	var parsed struct {
		Value *string `json:"value"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return "", false, fmt.Errorf("rezzy: decode state HAMT lookup: %w", err)
	}
	if parsed.Value == nil {
		return "", false, nil
	}
	return *parsed.Value, true, nil
}

// StateHAMTDiff returns the entries added in b and removed from a.
func StateHAMTDiff(roomID string, a, b []HAMTEntry) (added []HAMTEntry, removed []HAMTEntry, err error) {
	payload, err := json.Marshal(map[string]any{
		"op": "diff",
		"a":  a,
		"b":  b,
	})
	if err != nil {
		return nil, nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, nil, err
	}
	var parsed struct {
		Added   []HAMTEntry `json:"added"`
		Removed []HAMTEntry `json:"removed"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, nil, fmt.Errorf("rezzy: decode state HAMT diff: %w", err)
	}
	return parsed.Added, parsed.Removed, nil
}

// HAMTNodeKey identifies one leaf for root-based lookups.
type HAMTNodeKey struct {
	EventType string
	StateKey  string
}

func encodeNodes(nodes []HAMTNode) [][]string {
	encoded := make([][]string, len(nodes))
	for i, node := range nodes {
		encoded[i] = []string{node.Hash, hex.EncodeToString(node.Bytes)}
	}
	return encoded
}

func decodeNodes(encoded [][]string) ([]HAMTNode, error) {
	nodes := make([]HAMTNode, 0, len(encoded))
	for _, pair := range encoded {
		if len(pair) != 2 {
			return nil, fmt.Errorf("rezzy: node record must be [hash, bytes]")
		}
		bytes, err := hex.DecodeString(pair[1])
		if err != nil {
			return nil, fmt.Errorf("rezzy: decode node %s: %w", pair[0], err)
		}
		nodes = append(nodes, HAMTNode{Hash: pair[0], Bytes: bytes})
	}
	return nodes, nil
}

// StateHAMTPersist builds a state HAMT and returns every MTHN record and the
// MTHR root record needed to store it.
func StateHAMTPersist(roomID string, entries []HAMTEntry) (*HAMTPersisted, error) {
	payload, err := json.Marshal(map[string]any{
		"op":      "root",
		"persist": true,
		"entries": entries,
	})
	if err != nil {
		return nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, err
	}
	return decodePersisted(response)
}

func decodePersisted(response []byte) (*HAMTPersisted, error) {
	var parsed struct {
		RootHash     string     `json:"root_hash"`
		StateGroupID string     `json:"state_group_id"`
		RootRecord   string     `json:"root_record"`
		Nodes        [][]string `json:"nodes"`
		EntryCount   int        `json:"entry_count"`
		NodeBytes    int        `json:"node_bytes"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode persisted state HAMT: %w", err)
	}
	record, err := hex.DecodeString(parsed.RootRecord)
	if err != nil {
		return nil, fmt.Errorf("rezzy: decode state HAMT root record: %w", err)
	}
	nodes, err := decodeNodes(parsed.Nodes)
	if err != nil {
		return nil, err
	}
	return &HAMTPersisted{
		RootHash:     parsed.RootHash,
		StateGroupID: parsed.StateGroupID,
		RootRecord:   record,
		Nodes:        nodes,
		EntryCount:   parsed.EntryCount,
		NodeBytes:    parsed.NodeBytes,
	}, nil
}

// StateHAMTNeeded returns node hashes reachable from rootHash that are not in
// nodes. Repeat until it returns an empty slice, then call a read operation.
func StateHAMTNeeded(roomID, rootHash string, nodes []HAMTNode) ([]string, error) {
	payload, err := json.Marshal(map[string]any{
		"op":        "needed",
		"root_hash": rootHash,
		"nodes":     encodeNodes(nodes),
	})
	if err != nil {
		return nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Needed []string `json:"needed"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode state HAMT needed nodes: %w", err)
	}
	return parsed.Needed, nil
}

// StateHAMTApply applies mutations to the persisted HAMT described by
// parentRoot (an MTHR record; nil starts from the empty state) and returns
// the new root record plus only the nodes created relative to the parent.
// nodes should hold the parent nodes already fetched. When another path node
// is needed it returns a *MissingNodesError; fetch those nodes and call again.
func StateHAMTApply(
	roomID string, parentRoot []byte, parentEntryCount int,
	nodes []HAMTNode, mutations []HAMTMutation,
) (*HAMTPersisted, error) {
	encoded := make([][3]*string, len(mutations))
	for i := range mutations {
		encoded[i] = [3]*string{&mutations[i].EventType, &mutations[i].StateKey, mutations[i].EventID}
	}
	request := map[string]any{
		"op":                 "apply",
		"parent_entry_count": parentEntryCount,
		"nodes":              encodeNodes(nodes),
		"mutations":          encoded,
	}
	if parentRoot != nil {
		request["parent_root_record"] = hex.EncodeToString(parentRoot)
	}
	payload, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, err
	}
	if err := missingNodes(response); err != nil {
		return nil, err
	}
	return decodePersisted(response)
}

func missingNodes(response []byte) error {
	var parsed struct {
		Missing []string `json:"missing"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil
	}
	if len(parsed.Missing) == 0 {
		return nil
	}
	return &MissingNodesError{Hashes: parsed.Missing}
}

// StateHAMTReadEntries returns every (event type, state key, event ID) leaf
// under rootHash. All reachable nodes must be supplied.
func StateHAMTReadEntries(roomID, rootHash string, nodes []HAMTNode) ([]HAMTEntry, error) {
	payload, err := json.Marshal(map[string]any{
		"op":        "entries_root",
		"root_hash": rootHash,
		"nodes":     encodeNodes(nodes),
	})
	if err != nil {
		return nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, err
	}
	if err := missingNodes(response); err != nil {
		return nil, err
	}
	var parsed struct {
		Entries []HAMTEntry `json:"entries"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode state HAMT entries: %w", err)
	}
	return parsed.Entries, nil
}

// StateHAMTLookupRoot resolves each key under rootHash. The result has one
// entry per key; found reports whether that key is present.
func StateHAMTLookupRoot(roomID, rootHash string, nodes []HAMTNode, keys []HAMTNodeKey) (values []string, found []bool, err error) {
	encodedKeys := make([][2]string, len(keys))
	for i, key := range keys {
		encodedKeys[i] = [2]string{key.EventType, key.StateKey}
	}
	payload, err := json.Marshal(map[string]any{
		"op":        "lookup_root",
		"root_hash": rootHash,
		"nodes":     encodeNodes(nodes),
		"keys":      encodedKeys,
	})
	if err != nil {
		return nil, nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, nil, err
	}
	if err := missingNodes(response); err != nil {
		return nil, nil, err
	}
	var parsed struct {
		Values []*string `json:"values"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, nil, fmt.Errorf("rezzy: decode state HAMT lookups: %w", err)
	}
	values = make([]string, len(parsed.Values))
	found = make([]bool, len(parsed.Values))
	for i, value := range parsed.Values {
		if value != nil {
			values[i] = *value
			found[i] = true
		}
	}
	return values, found, nil
}

// StateHAMTVerify checks a root record and every supplied node, and returns
// the identity encoded in the record after walking the whole tree.
func StateHAMTVerify(roomID string, rootRecord []byte, nodes []HAMTNode) (*HAMTVerification, error) {
	payload, err := json.Marshal(map[string]any{
		"op":          "verify",
		"root_record": hex.EncodeToString(rootRecord),
		"nodes":       encodeNodes(nodes),
	})
	if err != nil {
		return nil, err
	}
	response, err := callStateHAMT(roomID, payload)
	if err != nil {
		return nil, err
	}
	if err := missingNodes(response); err != nil {
		return nil, err
	}
	var parsed struct {
		RootHash     string `json:"root_hash"`
		StateGroupID string `json:"state_group_id"`
		RoomID       string `json:"room_id"`
		Prefix       string `json:"prefix"`
		EntryCount   int    `json:"entry_count"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode state HAMT verification: %w", err)
	}
	prefix, err := hex.DecodeString(parsed.Prefix)
	if err != nil {
		return nil, fmt.Errorf("rezzy: decode state HAMT prefix: %w", err)
	}
	return &HAMTVerification{
		RootHash:     parsed.RootHash,
		StateGroupID: parsed.StateGroupID,
		RoomID:       parsed.RoomID,
		Prefix:       prefix,
		EntryCount:   parsed.EntryCount,
	}, nil
}

// ReconDigest returns the MSC4521 frame digest and event count for a set of
// reference-hash event IDs.
func ReconDigest(eventIDs []string) (digest string, count uint64, err error) {
	response, err := callRecon(map[string]any{
		"op":        "digest",
		"event_ids": eventIDs,
	})
	if err != nil {
		return "", 0, err
	}
	var parsed struct {
		Digest string `json:"digest"`
		Count  uint64 `json:"count"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return "", 0, fmt.Errorf("rezzy: decode reconciliation digest: %w", err)
	}
	return parsed.Digest, parsed.Count, nil
}

// ReconSketch returns a GF(2^256) reconciliation sketch for a set of event IDs.
func ReconSketch(eventIDs []string, capacity int) ([]byte, error) {
	response, err := callRecon(map[string]any{
		"op":        "sketch",
		"event_ids": eventIDs,
		"capacity":  capacity,
	})
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Sketch string `json:"sketch"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode reconciliation sketch: %w", err)
	}
	sketch, err := hex.DecodeString(parsed.Sketch)
	if err != nil {
		return nil, fmt.Errorf("rezzy: decode sketch hex: %w", err)
	}
	return sketch, nil
}

// ReconDiff decodes the symmetric difference between the local event set and a
// remote sketch. localOnly contains local event IDs. remoteOnly contains the
// 32-byte digest values that are only present in the remote set; map those
// back to IDs with the remote side's event data.
func ReconDiff(localEventIDs []string, remoteSketch []byte, capacity int) (localOnly []string, remoteOnly [][32]byte, err error) {
	response, err := callRecon(map[string]any{
		"op":              "diff",
		"local_event_ids": localEventIDs,
		"remote_sketch":   hex.EncodeToString(remoteSketch),
		"capacity":        capacity,
	})
	if err != nil {
		return nil, nil, err
	}
	var parsed struct {
		LocalOnly  []string `json:"local_only"`
		RemoteOnly []string `json:"remote_only"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, nil, fmt.Errorf("rezzy: decode reconciliation diff: %w", err)
	}
	remoteOnly = make([][32]byte, 0, len(parsed.RemoteOnly))
	for _, encoded := range parsed.RemoteOnly {
		raw, err := hex.DecodeString(encoded)
		if err != nil {
			return nil, nil, fmt.Errorf("rezzy: decode remote digest hex: %w", err)
		}
		if len(raw) != 32 {
			return nil, nil, fmt.Errorf("rezzy: remote digest is %d bytes, want 32", len(raw))
		}
		var digest [32]byte
		copy(digest[:], raw)
		remoteOnly = append(remoteOnly, digest)
	}
	return parsed.LocalOnly, remoteOnly, nil
}

// withFormat adds the optional event ID format to a request; an empty format
// leaves it out so each ID's format is detected.
func withFormat(format string, request map[string]any) map[string]any {
	if format != "" {
		request["format"] = format
	}
	return request
}

// ReconSnapshot returns the MSC4521 digest, event count and 32 strata for a
// set of event IDs. format is "v4plus" or "v3" to force one ID format, or
// empty to detect each ID's format.
func ReconSnapshot(eventIDs []string, format string) (*Snapshot, error) {
	response, err := callRecon(withFormat(format, map[string]any{
		"op":        "snapshot",
		"event_ids": eventIDs,
	}))
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Digest string   `json:"digest"`
		Count  uint64   `json:"count"`
		Strata []string `json:"strata"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode reconciliation snapshot: %w", err)
	}
	return &Snapshot{Digest: parsed.Digest, Count: parsed.Count, Strata: parsed.Strata}, nil
}

// ReconBucketSketches builds one base64url SyndromeSketch per requested bucket
// over the given event set.
func ReconBucketSketches(eventIDs []string, format string, requests []BucketRequest) ([]string, error) {
	response, err := callRecon(withFormat(format, map[string]any{
		"op":        "bucket_sketches",
		"event_ids": eventIDs,
		"requests":  requests,
	}))
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Sketches []string `json:"sketches"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode bucket sketches: %w", err)
	}
	return parsed.Sketches, nil
}

// ReconBucketDiff subtracts the requester's bucket sketches from the local
// event set and decodes the symmetric difference.
func ReconBucketDiff(
	eventIDs []string, format string, requests []BucketRequest,
	localSketches []string, localDigest string, localCount uint64,
) (*BucketDiffResult, error) {
	response, err := callRecon(withFormat(format, map[string]any{
		"op":                      "bucket_diff",
		"event_ids":               eventIDs,
		"requests":                requests,
		"local_sketches":          localSketches,
		"local_digest":            localDigest,
		"local_known_event_count": localCount,
	}))
	if err != nil {
		return nil, err
	}
	var parsed struct {
		Status                           string   `json:"status"`
		MissingEventIDs                  []string `json:"missing_event_ids"`
		RequesterOnlyShortIDs            []string `json:"requester_only_short_ids"`
		ExpectedRequesterSideAccumulator string   `json:"expected_requester_side_accumulator"`
		Count                            uint64   `json:"count"`
	}
	if err := json.Unmarshal(response, &parsed); err != nil {
		return nil, fmt.Errorf("rezzy: decode bucket diff: %w", err)
	}
	return &BucketDiffResult{
		Status:                           parsed.Status,
		MissingEventIDs:                  parsed.MissingEventIDs,
		RequesterOnlyShortIDs:            parsed.RequesterOnlyShortIDs,
		ExpectedRequesterSideAccumulator: parsed.ExpectedRequesterSideAccumulator,
		KnownEventCount:                  parsed.Count,
	}, nil
}

// Client drives the requester-side reconciliation policy. It snapshots the
// kernel at creation and expects each decoded room-diff result through Step.
type Client struct {
	ptr *C.RezzyClient
}

// NewClient creates a requester using a snapshot of kernel.
func NewClient(kernel *Kernel) (*Client, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	if kernel == nil || kernel.handle == nil {
		return nil, fmt.Errorf("rezzy: kernel is nil or closed")
	}
	ptr := C.rezzy_client_new(kernel.handle)
	if ptr == nil {
		return nil, fmt.Errorf("rezzy: create client failed")
	}
	return &Client{ptr: ptr}, nil
}

// Close releases the native client handle. It is safe to call more than once.
func (c *Client) Close() {
	if c != nil && c.ptr != nil {
		C.rezzy_client_free(c.ptr)
		c.ptr = nil
	}
}

// Step accepts a JSON start/advance request and returns the next action as
// JSON. The caller performs network I/O between steps.
func (c *Client) Step(input []byte) ([]byte, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	if c == nil || c.ptr == nil {
		return nil, fmt.Errorf("rezzy: client is closed")
	}
	var empty C.uint8_t
	first := &empty
	if len(input) > 0 {
		first = (*C.uint8_t)(unsafe.Pointer(&input[0]))
	}
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_client_step(c.ptr, first, C.size_t(len(input)), &out, &outLen)
	return finish(status, out, outLen)
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
	return finish(status, out, outLen)
}

func callStateHAMT(roomID string, payload []byte) ([]byte, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	version := C.CString(roomID)
	defer C.free(unsafe.Pointer(version))
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_state_hamt(
		version,
		(*C.uint8_t)(unsafe.Pointer(&payload[0])),
		C.size_t(len(payload)),
		&out,
		&outLen,
	)
	return finish(status, out, outLen)
}

func callRecon(payload map[string]any) ([]byte, error) {
	if abiVersionErr != nil {
		return nil, abiVersionErr
	}
	encoded, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	var out *C.uint8_t
	var outLen C.size_t
	status := C.rezzy_recon(
		(*C.uint8_t)(unsafe.Pointer(&encoded[0])),
		C.size_t(len(encoded)),
		&out,
		&outLen,
	)
	return finish(status, out, outLen)
}

func finish(status C.int, out *C.uint8_t, outLen C.size_t) ([]byte, error) {
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

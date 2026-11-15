package rezzy

import "fmt"

// HAMTEntry is one state-HAMT leaf: event type, state key, and event ID.
type HAMTEntry [3]string

// Snapshot is a responder-side MSC4521 algebraic_v1 view of an event set.
type Snapshot struct {
	Digest string
	Count  uint64
	Strata []string
}

// BucketRequest is one MSC4521 triage bucket: a prefix of `Depth` bits,
// `Prefix` itself, and the requested sketch `Capacity`.
type BucketRequest struct {
	Depth    uint8  `json:"depth"`
	Prefix   uint64 `json:"prefix"`
	Capacity int    `json:"capacity"`
}

// BucketDiffResult is the responder side of a room_diff exchange.
type BucketDiffResult struct {
	Status                           string   // "decoded" or "capacity_exceeded"
	MissingEventIDs                  []string // events the requester lacks
	RequesterOnlyShortIDs            []string // base64url big-endian h64 values only the requester holds
	ExpectedRequesterSideAccumulator string
	KnownEventCount                  uint64 // responder event count
}

// HAMTNode is one persisted MTHN record: its structural hash (hex) and the
// exact bytes stored under that hash.
type HAMTNode struct {
	Hash  string
	Bytes []byte
}

// HAMTPersisted is the full persisted form of one state HAMT: the root
// identity, the MTHR root record, and every MTHN node reachable from it.
type HAMTPersisted struct {
	RootHash     string
	StateGroupID string
	RootRecord   []byte
	Nodes        []HAMTNode
	EntryCount   int
	NodeBytes    int
}

// HAMTVerification is the result of checking a root record and node set.
type HAMTVerification struct {
	RootHash     string
	StateGroupID string
	RoomID       string
	Prefix       []byte
	EntryCount   int
}

// MissingNodesError reports hashes that a HAMT read needed but were not
// supplied. Fetch them and retry the same call.
type MissingNodesError struct {
	Hashes []string
}

func (e *MissingNodesError) Error() string {
	return fmt.Sprintf("rezzy: state HAMT is missing %d node(s)", len(e.Hashes))
}

package rezzy

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

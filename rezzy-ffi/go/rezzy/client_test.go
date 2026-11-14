//go:build linux && cgo

package rezzy

import (
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"testing"
)

func TestClientStepSynchronized(t *testing.T) {
	kernel, err := NewKernel()
	if err != nil {
		t.Fatal(err)
	}
	defer kernel.Close()

	client, err := NewClient(kernel)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()

	digest, count, err := kernel.Digest()
	if err != nil {
		t.Fatal(err)
	}
	strata, err := kernel.Strata()
	if err != nil {
		t.Fatalf("Kernel.Strata: %v", err)
	}
	request, err := json.Marshal(map[string]any{
		"op":                    "start",
		"digest":                digest,
		"known_event_count":     count,
		"strata":                strata,
		"frame_matches":         true,
		"has_unknown_extremity": false,
	})
	if err != nil {
		t.Fatal(err)
	}
	response, err := client.Step(request)
	if err != nil {
		t.Fatalf("Client.Step: %v", err)
	}
	var action struct {
		Action string `json:"action"`
	}
	if err := json.Unmarshal(response, &action); err != nil {
		t.Fatal(err)
	}
	if action.Action != "synchronized" {
		t.Fatalf("action = %q, want synchronized: %s", action.Action, response)
	}
}

// exchange runs a whole MSC4521 round between a requester (kernel and client
// handle) and a responder (the ReconSnapshot / ReconBucket* calls).
func exchange(t *testing.T, localIDs, remoteIDs []string) (diff *BucketDiffResult, final map[string]any) {
	t.Helper()
	kernel, err := NewKernel()
	if err != nil {
		t.Fatal(err)
	}
	defer kernel.Close()
	for _, id := range localIDs {
		if err := kernel.Insert(id); err != nil {
			t.Fatal(err)
		}
	}
	client, err := NewClient(kernel)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	localDigest, localCount, err := kernel.Digest()
	if err != nil {
		t.Fatal(err)
	}

	step := func(message map[string]any) map[string]any {
		t.Helper()
		request, err := json.Marshal(message)
		if err != nil {
			t.Fatal(err)
		}
		response, err := client.Step(request)
		if err != nil {
			t.Fatalf("Client.Step(%s): %v", message["op"], err)
		}
		var action map[string]any
		if err := json.Unmarshal(response, &action); err != nil {
			t.Fatal(err)
		}
		return action
	}

	snapshot, err := ReconSnapshot(remoteIDs, "")
	if err != nil {
		t.Fatalf("ReconSnapshot: %v", err)
	}
	action := step(map[string]any{
		"op":                    "start",
		"digest":                snapshot.Digest,
		"known_event_count":     snapshot.Count,
		"strata":                snapshot.Strata,
		"frame_matches":         true,
		"has_unknown_extremity": false,
	})
	if action["action"] != "bucket_sketches" {
		return nil, action
	}

	var requests []BucketRequest
	raw, _ := json.Marshal(action["requests"])
	if err := json.Unmarshal(raw, &requests); err != nil {
		t.Fatal(err)
	}
	localSketches, err := ReconBucketSketches(localIDs, "", requests)
	if err != nil {
		t.Fatalf("ReconBucketSketches: %v", err)
	}
	diff, err = ReconBucketDiff(remoteIDs, "", requests, localSketches, localDigest, localCount)
	if err != nil {
		t.Fatalf("ReconBucketDiff: %v", err)
	}

	roots := make([]uint64, 0, len(diff.RequesterOnlyShortIDs))
	for _, short := range diff.RequesterOnlyShortIDs {
		bytes, err := base64.RawURLEncoding.DecodeString(short)
		if err != nil || len(bytes) != 8 {
			t.Fatalf("short ID %q: %v", short, err)
		}
		roots = append(roots, binary.BigEndian.Uint64(bytes))
	}
	return diff, step(map[string]any{"op": "advance", "roots": roots, "failed_buckets": []int{}})
}

func TestExchangeFindsTheDifferences(t *testing.T) {
	const (
		idV3      = "$RrGxF28UrHLmoASHndYb9Jb/1SFww2ptmtur9INS438"
		idV12     = "$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE"
		idV3Other = "$EmwNBlHoSOVmCZ1cM//yv/OvxB6r4OFEIGSJea7+Amk"
	)
	diff, final := exchange(t, []string{idV3, idV12}, []string{idV12, idV3Other})
	if diff == nil {
		t.Fatalf("differing sets should need a bucket round, got %v", final)
	}
	if diff.Status != "decoded" {
		t.Fatalf("status = %q, want decoded", diff.Status)
	}
	if len(diff.MissingEventIDs) != 1 || diff.MissingEventIDs[0] != idV3Other {
		t.Fatalf("missing = %v, want [%s]", diff.MissingEventIDs, idV3Other)
	}
	if len(diff.RequesterOnlyShortIDs) != 1 {
		t.Fatalf("requester-only short IDs = %v, want one", diff.RequesterOnlyShortIDs)
	}
	if final["action"] != "resolve_roots" {
		t.Fatalf("final action = %v, want resolve_roots", final)
	}
}

func TestExchangeIsSynchronizedForEqualSets(t *testing.T) {
	ids := []string{
		"$RrGxF28UrHLmoASHndYb9Jb/1SFww2ptmtur9INS438",
		"$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE",
	}
	diff, final := exchange(t, ids, ids)
	if diff != nil || final["action"] != "synchronized" {
		t.Fatalf("diff = %v, final = %v, want synchronized", diff, final)
	}
}

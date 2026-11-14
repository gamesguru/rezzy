//go:build linux && cgo

package rezzy

import (
	"encoding/base64"
	"strings"
	"testing"
)

// Known-good vector from the Rust ABI tests, room version 12.
const knownEvent = `{"auth_events":["$sbusQdJu8vp4mYghGgIdFrz9K_UbSNVmVr3UBxLM2Mk","$SbBcrKoU8Wo-rq9hH6NORFEcD7jrSgLkyYBgvlvNZVc"],"content":{"body":"A nice sunny day spent near the window.","m.mentions":{},"msgtype":"m.text"},"depth":265,"hashes":{"sha256":"BN2Mpld9Ox01qmZjEh+2SXjc3hf67Z+UrUvCc/TdJoc"},"origin_server_ts":1763426725083,"prev_events":["$YgQCx1FMxaNlpl8CoymZCmPLcE9YJ95kwnp8VBHJiqY"],"room_id":"!L58ME6ufiP49v97UIOBIpvWKEgj4912JmECPuDzlvCI","sender":"@chtr-x:matrix.org","signatures":{"matrix.org":{"ed25519:a_RXGa":"EKGxFhkSSIlattBwQDE9V1moJdRz6FGd4tC+u43q6z0/8v3oys743hvIStLWTO0LYR1GrntNOHyztbUaIVQOAw"}},"type":"m.room.message","unsigned":{"age_ts":1763426725083}}`

const knownID = "$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE"

func TestEventIDMatchesKnownVector(t *testing.T) {
	id, err := EventID("12", []byte(knownEvent))
	if err != nil {
		t.Fatalf("EventID: %v", err)
	}
	if id != knownID {
		t.Fatalf("EventID = %q, want %q", id, knownID)
	}
}

func TestVerifyContentHash(t *testing.T) {
	if err := VerifyContentHash("12", []byte(knownEvent)); err != nil {
		t.Fatalf("VerifyContentHash: %v", err)
	}
	tampered := strings.Replace(knownEvent, "sunny", "rainy", 1)
	if err := VerifyContentHash("12", []byte(tampered)); err == nil {
		t.Fatal("VerifyContentHash accepted tampered content")
	}
}

func TestHashEventsJSONL(t *testing.T) {
	report, err := HashEventsJSONL("12", []byte(knownEvent+"\n"))
	if err != nil {
		t.Fatalf("HashEventsJSONL: %v", err)
	}
	if !strings.Contains(string(report), knownID) {
		t.Fatalf("report does not contain %q: %s", knownID, report)
	}
}

func TestErrorsCarryMessages(t *testing.T) {
	_, err := EventID("12", []byte("not json"))
	if err == nil || !strings.Contains(err.Error(), "invalid JSON") {
		t.Fatalf("EventID error = %v, want an invalid JSON message", err)
	}
	if _, err := EventID("1", []byte(knownEvent)); err == nil {
		t.Fatal("EventID accepted opaque-ID room version 1")
	}
}

func TestABIVersionMismatchIsReported(t *testing.T) {
	err := checkABIVersion(1, 2)
	if err == nil || !strings.Contains(err.Error(), "header 1, library 2") {
		t.Fatalf("ABI version error = %v, want header/library mismatch", err)
	}
}

func TestStateHAMT(t *testing.T) {
	entries := []HAMTEntry{
		{"m.room.create", "", "$create:example.org"},
		{"m.room.member", "@alice:example.org", "$member:example.org"},
	}
	rootHash, stateGroupID, err := StateHAMTRoot("!room:example.org", entries)
	if err != nil {
		t.Fatalf("StateHAMTRoot: %v", err)
	}
	if len(rootHash) != 64 || len(stateGroupID) != 64 {
		t.Fatalf("root = %q, state group = %q, want two 64-character hex strings", rootHash, stateGroupID)
	}

	value, found, err := StateHAMTLookup("!room:example.org", entries, "m.room.member", "@alice:example.org")
	if err != nil {
		t.Fatalf("StateHAMTLookup: %v", err)
	}
	if !found || value != "$member:example.org" {
		t.Fatalf("lookup = %q, %v, want $member:example.org, true", value, found)
	}

	changed := []HAMTEntry{
		{"m.room.create", "", "$create:example.org"},
		{"m.room.member", "@alice:example.org", "$member2:example.org"},
	}
	added, removed, err := StateHAMTDiff("!room:example.org", entries, changed)
	if err != nil {
		t.Fatalf("StateHAMTDiff: %v", err)
	}
	if len(added) != 1 || added[0][2] != "$member2:example.org" {
		t.Fatalf("added = %v, want the changed member event", added)
	}
	if len(removed) != 1 || removed[0][2] != "$member:example.org" {
		t.Fatalf("removed = %v, want the old member event", removed)
	}
}

func TestReconciliation(t *testing.T) {
	const (
		idV3      = "$RrGxF28UrHLmoASHndYb9Jb/1SFww2ptmtur9INS438"
		idV12     = "$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE"
		idV3Other = "$EmwNBlHoSOVmCZ1cM//yv/OvxB6r4OFEIGSJea7+Amk"
	)
	digest, count, err := ReconDigest([]string{idV3, idV12})
	if err != nil {
		t.Fatalf("ReconDigest: %v", err)
	}
	if len(digest) != 22 || count != 2 {
		t.Fatalf("digest = %q, count = %d, want a 22-character digest and count 2", digest, count)
	}

	sketch, err := ReconSketch([]string{idV12, idV3Other}, 4)
	if err != nil {
		t.Fatalf("ReconSketch: %v", err)
	}
	if len(sketch) != 4*32 {
		t.Fatalf("sketch is %d bytes, want %d", len(sketch), 4*32)
	}

	localOnly, remoteOnly, err := ReconDiff([]string{idV3, idV12}, sketch, 4)
	if err != nil {
		t.Fatalf("ReconDiff: %v", err)
	}
	if len(localOnly) != 1 || localOnly[0] != idV3 {
		t.Fatalf("localOnly = %v, want [%s]", localOnly, idV3)
	}
	if len(remoteOnly) != 1 {
		t.Fatalf("remoteOnly = %v, want one digest", remoteOnly)
	}
}

func TestKernelMatchesOneShotDigestAndRoundTrips(t *testing.T) {
	ids := []string{knownID, "$RrGxF28UrHLmoASHndYb9Jb/1SFww2ptmtur9INS438"}
	kernel, err := NewKernel()
	if err != nil {
		t.Fatal(err)
	}
	defer kernel.Close()
	for _, id := range ids {
		if err := kernel.Insert(id); err != nil {
			t.Fatalf("Insert(%s): %v", id, err)
		}
	}
	digest, count, err := kernel.Digest()
	if err != nil {
		t.Fatal(err)
	}
	wantDigest, wantCount, err := ReconDigest(ids)
	if err != nil {
		t.Fatal(err)
	}
	if digest != wantDigest || count != wantCount {
		t.Fatalf("kernel digest = %s/%d, one-shot = %s/%d", digest, count, wantDigest, wantCount)
	}
	strata, err := kernel.Strata()
	if err != nil {
		t.Fatalf("Kernel.Strata: %v", err)
	}
	if len(strata) != 32 {
		t.Fatalf("Kernel.Strata returned %d strata, want 32", len(strata))
	}
	for i, encoded := range strata {
		coordinates, err := base64.RawURLEncoding.DecodeString(encoded)
		if err != nil || len(coordinates) != 8*8 {
			t.Fatalf("stratum %d is invalid: decoded length %d, error %v", i, len(coordinates), err)
		}
	}

	record, err := kernel.Bytes()
	if err != nil {
		t.Fatal(err)
	}
	rebuilt, err := KernelFromBytes(record)
	if err != nil {
		t.Fatal(err)
	}
	defer rebuilt.Close()
	if again, _, _ := rebuilt.Digest(); again != digest {
		t.Fatalf("rebuilt digest = %s, want %s", again, digest)
	}

	if err := kernel.Insert("not-an-event-id"); err == nil {
		t.Fatal("Insert of an invalid ID succeeded")
	}
	if _, err := KernelFromBytes([]byte{1, 2, 3}); err == nil {
		t.Fatal("KernelFromBytes accepted a short record")
	}
	kernel.Close()
	if err := kernel.Insert(knownID); err != ErrKernelClosed {
		t.Fatalf("closed kernel error = %v, want ErrKernelClosed", err)
	}
}

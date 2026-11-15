//go:build linux && cgo

package rezzy

import (
	"bytes"
	"encoding/base64"
	"errors"
	"fmt"
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

func TestStateHAMTPersistRoundTrip(t *testing.T) {
	const room = "!room:example.org"
	entries := []HAMTEntry{
		{"m.room.create", "", "$create:example.org"},
		{"m.room.member", "@alice:example.org", "$member:example.org"},
	}

	persisted, err := StateHAMTPersist(room, entries)
	if err != nil {
		t.Fatalf("StateHAMTPersist: %v", err)
	}
	if len(persisted.RootRecord) < 5 || string(persisted.RootRecord[:5]) != "MTHR\x01" {
		t.Fatalf("root record = %x, want MTHR\\x01 prefix", persisted.RootRecord)
	}
	if len(persisted.Nodes) == 0 || persisted.EntryCount != len(entries) {
		t.Fatalf("nodes = %d, entries = %d", len(persisted.Nodes), persisted.EntryCount)
	}

	needed, err := StateHAMTNeeded(room, persisted.RootHash, nil)
	if err != nil {
		t.Fatalf("StateHAMTNeeded: %v", err)
	}
	if len(needed) != 1 || needed[0] != persisted.RootHash {
		t.Fatalf("needed = %v, want just the root", needed)
	}

	if _, err := StateHAMTReadEntries(room, persisted.RootHash, nil); err == nil {
		t.Fatal("read without nodes should report the missing root")
	} else {
		var missing *MissingNodesError
		if !errors.As(err, &missing) {
			t.Fatalf("missing error = %v, want *MissingNodesError", err)
		}
	}

	read, err := StateHAMTReadEntries(room, persisted.RootHash, persisted.Nodes)
	if err != nil {
		t.Fatalf("StateHAMTReadEntries: %v", err)
	}
	if len(read) != len(entries) {
		t.Fatalf("read %d entries, want %d", len(read), len(entries))
	}

	values, found, err := StateHAMTLookupRoot(room, persisted.RootHash, persisted.Nodes, []HAMTNodeKey{
		{"m.room.member", "@alice:example.org"},
		{"m.room.member", "@bob:example.org"},
	})
	if err != nil {
		t.Fatalf("StateHAMTLookupRoot: %v", err)
	}
	if !found[0] || values[0] != "$member:example.org" {
		t.Fatalf("lookup[0] = %q, %v", values[0], found[0])
	}
	if found[1] {
		t.Fatalf("lookup[1] should be absent, got %q", values[1])
	}

	verified, err := StateHAMTVerify(room, persisted.RootRecord, persisted.Nodes)
	if err != nil {
		t.Fatalf("StateHAMTVerify: %v", err)
	}
	if verified.RootHash != persisted.RootHash || verified.StateGroupID != persisted.StateGroupID {
		t.Fatalf("verified root = %s/%s", verified.RootHash, verified.StateGroupID)
	}
	if verified.EntryCount != len(entries) {
		t.Fatalf("verified %d entries, want %d", verified.EntryCount, len(entries))
	}
}

func TestStateHAMTApplyMatchesRebuild(t *testing.T) {
	const room = "!room:example.org"
	base := make([]HAMTEntry, 0, 200)
	for i := 0; i < 200; i++ {
		base = append(base, HAMTEntry{"m.test", fmt.Sprintf("key%d", i), fmt.Sprintf("$ev%d", i)})
	}
	parent, err := StateHAMTPersist(room, base)
	if err != nil {
		t.Fatal(err)
	}
	byHash := map[string]HAMTNode{}
	for _, node := range parent.Nodes {
		byHash[node.Hash] = node
	}

	replaced, added := "$ev3b", "$new"
	mutations := []HAMTMutation{
		{EventType: "m.test", StateKey: "key3", EventID: &replaced},
		{EventType: "m.test", StateKey: "fresh", EventID: &added},
		{EventType: "m.test", StateKey: "key9"},
	}
	var supplied []HAMTNode
	var applied *HAMTPersisted
	for {
		applied, err = StateHAMTApply(room, parent.RootRecord, parent.EntryCount, supplied, mutations)
		var missing *MissingNodesError
		if errors.As(err, &missing) {
			for _, hash := range missing.Hashes {
				node, ok := byHash[hash]
				if !ok {
					t.Fatalf("apply asked for node %s outside the parent tree", hash)
				}
				supplied = append(supplied, node)
			}
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		break
	}

	want := make([]HAMTEntry, 0, len(base))
	for _, entry := range base {
		switch entry[1] {
		case "key9":
			continue
		case "key3":
			entry[2] = replaced
		}
		want = append(want, entry)
	}
	want = append(want, HAMTEntry{"m.test", "fresh", added})
	rebuilt, err := StateHAMTPersist(room, want)
	if err != nil {
		t.Fatal(err)
	}
	if applied.RootHash != rebuilt.RootHash || applied.StateGroupID != rebuilt.StateGroupID {
		t.Fatalf("apply and rebuild disagree: %s/%s vs %s/%s",
			applied.RootHash, applied.StateGroupID, rebuilt.RootHash, rebuilt.StateGroupID)
	}
	if !bytes.Equal(applied.RootRecord, rebuilt.RootRecord) {
		t.Fatal("root records differ")
	}
	if applied.EntryCount != 200 {
		t.Fatalf("entry count = %d, want 200", applied.EntryCount)
	}
	if len(supplied) >= len(parent.Nodes) || len(applied.Nodes) >= len(rebuilt.Nodes) {
		t.Fatalf("apply touched too much: loaded %d/%d, created %d/%d",
			len(supplied), len(parent.Nodes), len(applied.Nodes), len(rebuilt.Nodes))
	}
}

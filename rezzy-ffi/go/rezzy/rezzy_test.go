package rezzy

import (
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

//go:build linux && cgo

package rezzy

import (
	"encoding/base64"
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
	strata := make([]string, 32)
	for index := range strata {
		strata[index] = base64.RawURLEncoding.EncodeToString(make([]byte, 64))
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

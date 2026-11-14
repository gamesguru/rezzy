//go:build !(linux && cgo)

package rezzy

import (
	"errors"
	"testing"
)

func TestUnavailableWithoutCgo(t *testing.T) {
	if _, err := EventID("12", []byte("{}")); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("EventID error = %v, want ErrUnavailable", err)
	}
	if _, _, err := ReconDigest(nil); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("ReconDigest error = %v, want ErrUnavailable", err)
	}
}

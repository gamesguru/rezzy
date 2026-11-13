// Demonstrate importing the Rezzy Go package.
//
//	make -C rezzy-ffi native
//	cd rezzy-ffi && go run ./go/examples/hash event-id 12 event.json
package main

import (
	"fmt"
	"os"

	"github.com/gamesguru/rezzy/rezzy-ffi/go/rezzy"
)

func main() {
	if len(os.Args) != 4 {
		fmt.Fprintf(os.Stderr, "usage: %s <event-id|verify-content-hash|hash-events> <room-version> <event.jsonl>\n", os.Args[0])
		os.Exit(2)
	}
	action, roomVersion, path := os.Args[1], os.Args[2], os.Args[3]
	event, err := os.ReadFile(path)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	switch action {
	case "event-id":
		id, err := rezzy.EventID(roomVersion, event)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Println(id)
	case "verify-content-hash":
		if err := rezzy.VerifyContentHash(roomVersion, event); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Println("content hash valid")
	case "hash-events":
		report, err := rezzy.HashEventsJSONL(roomVersion, event)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Println(string(report))
	default:
		fmt.Fprintf(os.Stderr, "unknown action %q (use event-id, verify-content-hash, or hash-events)\n", action)
		os.Exit(2)
	}
}

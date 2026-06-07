#!/usr/bin/env python3
"""
Synthesize strictly legally authenticated baseline topologies for matrix state-res.
Prevents "Ghost Victory" illusions in the test suite by abstracting the baseline generation.
"""

import json
import sys


def make_event(
    event_id,
    event_type,
    sender,
    origin_server_ts,
    depth,
    content,
    prev_events,
    auth_events,
    state_key=None,
):
    """
    Helper to synthesize a valid matrix event dictionary.
    """
    event = {
        "event_id": event_id,
        "room_id": "!test_room:example.com",
        "sender": sender,
        "type": event_type,
        "content": content,
        "origin_server_ts": origin_server_ts,
        "depth": depth,
        "prev_events": prev_events,
        "auth_events": auth_events,
    }
    if state_key is not None:
        event["state_key"] = state_key
    elif event_type in [
        "m.room.create",
        "m.room.power_levels",
        "m.room.join_rules",
        "m.room.name",
        "m.room.topic",
    ]:
        event["state_key"] = ""
    return event


def generate_strict_baseline(creator="@alice:example.com", room_version="11"):
    """
    Guarantees a valid, legally authenticated baseline to prevent "Ghost Victories".
    """
    return [
        make_event(
            "$root",
            "m.room.create",
            creator,
            1000,
            1,
            {"room_version": room_version},
            [],
            [],
        ),
        make_event(
            "$alice_join",
            "m.room.member",
            creator,
            1100,
            2,
            {"membership": "join"},
            ["$root"],
            ["$root"],
            state_key=creator,
        ),
        make_event(
            "$jr_pub",
            "m.room.join_rules",
            creator,
            1150,
            3,
            {"join_rule": "public"},
            ["$alice_join"],
            ["$root", "$alice_join"],
            state_key="",
        ),
        make_event(
            "$pl_init",
            "m.room.power_levels",
            creator,
            1200,
            4,
            {"users": {creator: 100}},
            ["$jr_pub"],
            ["$root", "$alice_join", "$jr_pub"],
            state_key="",
        ),
    ]


def generate_08():
    """
    Generates scenario 08 using the strict baseline foundation.
    """
    events = generate_strict_baseline()
    base_auth = ["$root", "$alice_join", "$jr_pub", "$pl_init"]

    # Fork A: Bob joins
    events.append(
        make_event(
            "$bob_join",
            "m.room.member",
            "@bob:example.com",
            1250,
            5,
            {"membership": "join"},
            ["$pl_init"],
            base_auth,
            state_key="@bob:example.com",
        )
    )

    # ... append the rest of the topology for scenario 08
    return events


def main():
    # If run directly, print the generated 08 scenario to verify it compiles
    events_08 = generate_08()
    print(json.dumps(events_08, indent=2))


if __name__ == "__main__":
    main()

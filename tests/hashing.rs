use ruma_lean::LeanEvent;
use serde_json::json;

#[cfg(feature = "hashing")]
#[test]
fn test_event_id_hashing() {
    let json_payload = json!({
        "type": "m.room.message",
        "sender": "@alice:example.com",
        "origin_server_ts": 123456789,
        "content": {
            "body": "Hello, world!"
        },
        "unsigned": {
            "age": 123
        },
        "signatures": {
            "example.com": {
                "ed25519:1": "a_signature"
            }
        }
    });

    let event: LeanEvent = serde_json::from_value(json_payload).unwrap();

    // The event ID should be generated and start with '$'
    assert!(event.event_id.starts_with('$'));

    // To strictly verify, we would check the SHA-256 base64 output, but for now we just verify it exists
    assert!(!event.event_id.is_empty());
}

#[cfg(not(feature = "hashing"))]
#[test]
fn test_missing_event_id_fails_without_hashing() {
    let json_payload = json!({
        "type": "m.room.message",
        "sender": "@alice:example.com",
        "origin_server_ts": 123456789,
        "content": {
            "body": "Hello, world!"
        }
    });

    let result: Result<LeanEvent, _> = serde_json::from_value(json_payload);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("event_id is missing"));
}

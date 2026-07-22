use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rezzy::reconcile::{
    BucketSummary, EventHash, RoomAccumulator, SyndromeSketch, verify_residual,
};

fn event_id(bytes: [u8; 32]) -> String {
    format!("${}", URL_SAFE_NO_PAD.encode(bytes))
}

#[test]
fn hash_derived_event_ids_feed_all_resident_layers() {
    let first_id = event_id([0x11; 32]);
    let second_id = event_id([0xa7; 32]);
    let first = EventHash::from_event_id(&first_id, true).unwrap();
    let second = EventHash::from_event_id(&second_id, true).unwrap();

    assert_eq!(first.h128, u128::from_be_bytes([0x11; 16]));
    assert_eq!(first.h64, u64::from_be_bytes([0x11; 8]));

    let mut accumulator = RoomAccumulator::new();
    let mut sketch = SyndromeSketch::new(8).unwrap();
    let mut buckets = BucketSummary::default();
    for hash in [first, second] {
        accumulator.insert(hash).unwrap();
        sketch.toggle(hash.h64);
        buckets.insert(hash).unwrap();
    }

    assert_eq!(accumulator.known_event_count(), 2);
    assert!(verify_residual(accumulator.digest(), [first, second]));
    assert_eq!(buckets.buckets()[0x11].count, 1);
    assert_eq!(buckets.buckets()[0xa7].count, 1);
    assert_eq!(SyndromeSketch::decode(8, &sketch.encode()).unwrap(), sketch);
}

#[test]
fn legacy_ids_are_sha256_derived_and_stable() {
    let first = EventHash::from_event_id("$opaque:example.org", false).unwrap();
    let second = EventHash::from_event_id("$opaque:example.org", false).unwrap();
    assert_eq!(first, second);
    assert_ne!(first.h128, 0);
}

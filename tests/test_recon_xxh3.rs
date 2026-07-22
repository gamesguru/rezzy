use rezzy::reconcile::{XorSum, hash_bytes, hash_str};

#[test]
fn xxh3_128_is_stable_across_entry_points() {
    let input = "room-event-id-$abc123";

    let from_str = hash_str(input);
    let from_bytes = hash_bytes(input.as_bytes());
    let from_const = xxhash_rust::const_xxh3::xxh3_128(input.as_bytes());

    assert_eq!(from_str, from_bytes);
    assert_eq!(from_bytes, from_const);
}

#[test]
fn xor_accumulator_cancels_pairs() {
    let mut acc = XorSum::new();

    acc.insert_str("$one");
    acc.insert_str("$two");
    acc.remove_str("$one");

    assert_eq!(acc.digest(), hash_str("$two"));

    acc.replace(hash_str("$two"), hash_str("$three"));
    assert_eq!(acc.digest(), hash_str("$three"));

    acc.remove_str("$three");
    assert_eq!(acc.digest(), 0);
}

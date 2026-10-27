use alloc::{string::String, vec};
use base64::Engine;

/// Encodes base64 without relying on the `base64` crate's `alloc` feature.
pub(crate) fn encode<E: Engine + ?Sized>(engine: &E, input: &[u8]) -> String {
    let capacity = base64::encoded_len(input.len(), true).expect("base64 output length overflow");
    let mut output = vec![0; capacity];
    let length = engine
        .encode_slice(input, &mut output)
        .expect("base64 output buffer is sufficiently sized");
    output.truncate(length);
    String::from_utf8(output).expect("base64 output is ASCII")
}

/// Decodes base64 into caller-provided storage without relying on the
/// `base64` crate's `alloc` feature.
#[cfg(feature = "signing-core")]
pub(crate) fn decode_into<E: Engine + ?Sized>(
    engine: &E,
    input: &str,
    output: &mut [u8],
) -> Result<usize, base64::DecodeSliceError> {
    engine.decode_slice(input, output)
}

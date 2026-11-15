//! Native C ABI over rezzy's event hashing, state HAMTs, and reconciliation.
//! See `include/rezzy.h` for the contract.

#![allow(unsafe_code)]
//!
//! Every function returns a status code and hands results back through
//! `out`/`out_len`. Buffers are allocated here and released with
//! [`rezzy_free`]; callers never free them with their own allocator.

use base64::Engine as _;
use rezzy::hamt::codec::PersistedInternalNode;
use rezzy::hamt::{
    build_hamt_root_handle, diff_hamt_nodes, HamtNode, HamtReadError, NodeRef, PersistedHamtReader,
    RootHandle, StructuralHash,
};
use rezzy::incremental::LtHash;
use rezzy::state::hamt_root::{decode_state_hamt_root, encode_state_hamt_root, StateHamtKey};
use rezzy::JsonValue;
use rezzy_recon::resident::ResidentKernel;
use rezzy_recon::{
    build_bucket_sketches, encode_strata, BucketDecodeBatch, BucketDecodeSuccess, BucketRequest,
    ClientAction, ElementHash, EventIdFormat, FixedWindowElementHash, Gf256, Gf256Decoder,
    Gf256Sketch, ReconciliationClient, RemoteDigest, RoomAccumulator, SortedPopulation,
    SyndromeSketch, MAX_BUCKETED_SKETCH_CAPACITY, MAX_RECONCILIATION_ROUNDS,
};
use std::ffi::{c_char, CStr};
use std::ptr;
use std::sync::Arc;

const OK: i32 = 0;
const ERR: i32 = 1;
const BAD_ARG: i32 = 2;
const ABI_VERSION: u32 = 1;
const MAX_RECON_CAPACITY: usize = 256;

/// Returns the version of the public `rezzy-ffi` C ABI.
///
/// Consumers should compare this with `REZZY_ABI_VERSION` from `rezzy.h`
/// before calling any other exported function.
#[no_mangle]
pub const extern "C" fn rezzy_abi_version() -> u32 {
    ABI_VERSION
}

/// Returns the crate's release version, such as `0.6.0-rc.1`, as a static
/// NUL-terminated string. Never free it. Use it to report which release is
/// loaded; use [`rezzy_abi_version`] to decide whether the library is
/// compatible.
#[no_mangle]
pub const extern "C" fn rezzy_version() -> *const core::ffi::c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast::<core::ffi::c_char>()
}

/// Clears the output slots so an early return never leaves stale values.
///
/// # Safety
/// Non-null `out` and `out_len` must point to writable storage.
unsafe fn reset_outputs(out: *mut *mut u8, out_len: *mut usize) -> bool {
    if !out.is_null() {
        // SAFETY: non-null `out` is writable per the contract.
        unsafe { *out = ptr::null_mut() };
    }
    if !out_len.is_null() {
        // SAFETY: non-null `out_len` is writable per the contract.
        unsafe { *out_len = 0 };
    }
    !out.is_null() && !out_len.is_null()
}

/// Moves `bytes` to a heap buffer owned by the caller until [`rezzy_free`].
fn hand_over(bytes: Vec<u8>, out: *mut *mut u8, out_len: *mut usize) {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let raw = Box::into_raw(boxed).cast::<u8>();
    // SAFETY: the caller checked `out` and `out_len` are non-null and writable.
    unsafe {
        *out = raw;
        *out_len = len;
    }
}

/// Runs `body`, turning a panic into `ERR` with a message so it never unwinds
/// across the C boundary (which would abort the host process).
fn guard(out: *mut *mut u8, out_len: *mut usize, body: impl FnOnce() -> i32) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|_| {
        if !out.is_null() && !out_len.is_null() {
            hand_over(b"internal error: rezzy panicked".to_vec(), out, out_len);
        }
        ERR
    })
}

/// Resets the output slots and validates the common `(room_version, input)`
/// pointer arguments, returning the room-version string and input slice.
///
/// # Errors
/// Returns `BAD_ARG` without allocating an output buffer if any pointer is
/// null or the room-version string is not UTF-8. Non-null output slots are
/// reset even when validation fails.
///
/// # Safety
/// `room_version` must be NUL-terminated and `input` must point to `input_len`
/// readable bytes, whenever they are non-null. Non-null `out` and `out_len`
/// must point to writable storage. The room-version and input storage must
/// remain valid and unchanged for the lifetime of the returned borrows. Neither
/// may overlap either output slot.
unsafe fn unpack_raw_args<'a>(
    room_version: *const c_char,
    input: *const u8,
    input_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> Result<(&'a str, &'a [u8]), i32> {
    // SAFETY: non-null output slots are required to be writable.
    if !unsafe { reset_outputs(out, out_len) } || room_version.is_null() || input.is_null() {
        return Err(BAD_ARG);
    }
    // SAFETY: non-null and NUL-terminated per the contract.
    let version = unsafe { CStr::from_ptr(room_version) }
        .to_str()
        .map_err(|_| BAD_ARG)?;
    // SAFETY: non-null, `input_len` readable bytes per the contract.
    let bytes = unsafe { std::slice::from_raw_parts(input, input_len) };
    Ok((version, bytes))
}

/// Validates and parses the single-event FFI arguments inside a panic guard,
/// normalizes the wire event for `room_version`, and hands over any output or
/// error bytes.
///
/// Returns `OK` on success; `op` returning `None` leaves output null with zero
/// length. Null pointers or a non-UTF-8 version return `BAD_ARG` without a
/// buffer. JSON errors, operation errors, and caught panics return `ERR` with
/// an allocated message, released with [`rezzy_free`].
///
/// # Safety
/// Same pointer validity requirements as [`rezzy_event_id`].
unsafe fn guarded_with_wire_event<F>(
    room_version: *const c_char,
    event_json: *const u8,
    event_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
    op: F,
) -> i32
where
    F: FnOnce(&JsonValue, &str) -> Result<Option<Vec<u8>>, String>,
{
    // SAFETY: forwarded from this function's contract.
    guard(out, out_len, || unsafe {
        let (version, bytes) =
            match unpack_raw_args(room_version, event_json, event_len, out, out_len) {
                Ok(unpacked) => unpacked,
                Err(status) => return status,
            };
        let event = match JsonValue::parse_bytes(bytes) {
            Ok(event) => event,
            Err(error) => {
                hand_over(error.to_string().into_bytes(), out, out_len);
                return ERR;
            }
        };
        let wire = rezzy::event_report::wire_event(&event, version);
        match op(&wire, version) {
            Ok(Some(bytes)) => {
                hand_over(bytes, out, out_len);
                OK
            }
            Ok(None) => OK,
            Err(error) => {
                hand_over(error.into_bytes(), out, out_len);
                ERR
            }
        }
    })
}

macro_rules! define_wire_event_ffi {
    ($(#[$meta:meta])* $name:ident, |$wire:ident, $version:ident| $body:expr) => {
        $(#[$meta])*
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            room_version: *const c_char,
            event_json: *const u8,
            event_len: usize,
            out: *mut *mut u8,
            out_len: *mut usize,
        ) -> i32 {
            // SAFETY: forwarded from this function's contract.
            unsafe {
                guarded_with_wire_event(
                    room_version,
                    event_json,
                    event_len,
                    out,
                    out_len,
                    |$wire, $version| $body,
                )
            }
        }
    };
}

define_wire_event_ffi!(
    /// Writes `$` followed by the event reference hash to `out`.
    ///
    /// Ignores an exported `event_id` for versions without opaque IDs. Returns
    /// 0 on success, 1 with an error message for parsing or hashing failures
    /// (including opaque-ID and unsupported versions) or a caught panic, or 2
    /// without a buffer for null arguments or a non-UTF-8 room version. Output bytes are not
    /// NUL-terminated; release any returned buffer with [`rezzy_free`].
    ///
    /// # Safety
    /// See `include/rezzy.h`: the room version must be NUL-terminated, input
    /// pointers must be readable for the stated byte lengths, and non-null
    /// `out`/`out_len` must point to writable storage.
    rezzy_event_id,
    |wire, version| {
        rezzy::reference_hash(wire, version).map(|hash| Some(format!("${hash}").into_bytes()))
    }
);

define_wire_event_ffi!(
    /// Checks an event's `hashes.sha256` against its content.
    ///
    /// Returns 0 with null output and zero length on success, 1 with an allocated
    /// message for parsing or verification errors or a caught panic, or 2 without
    /// a buffer for null arguments or a non-UTF-8 room version. An exported `event_id` is removed
    /// for versions without opaque IDs. Release error buffers with [`rezzy_free`].
    ///
    /// # Safety
    /// See `include/rezzy.h`: the room version must be NUL-terminated, input
    /// pointers must be readable for the stated byte lengths, and non-null
    /// `out`/`out_len` must point to writable storage.
    rezzy_verify_content_hash,
    |wire, version| rezzy::verify_content_hash(wire, version).map(|()| None)
);

/// Describes every event in a JSONL buffer.
///
/// Writes the same report object as `rezzy hash events` to `out`. Each event
/// record carries its 1-based `line`; blank lines are ignored. Per-event
/// failures are recorded and the call still succeeds with status 0. Unlike the
/// CLI, this does not reject unsupported room versions before processing.
///
/// Returns 1 with a message for invalid UTF-8 input, no nonblank lines,
/// serialization failure, or a caught panic; returns 2 without a buffer for
/// null arguments or a non-UTF-8 room version. Output is not NUL-terminated;
/// release any returned buffer with [`rezzy_free`].
///
/// # Safety
/// See `include/rezzy.h`: the room version must be NUL-terminated, input
/// pointers must be readable for the stated byte lengths, and non-null
/// `out`/`out_len` must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_hash_events(
    room_version: *const c_char,
    jsonl: *const u8,
    jsonl_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    guard(out, out_len, || unsafe {
        rezzy_hash_events_inner(room_version, jsonl, jsonl_len, out, out_len)
    })
}

/// Builds a serialized GF(2^256) reconciliation sketch from 32-byte values.
///
/// Returns `OK` and `capacity * 32` sketch bytes on success, `ERR` with a
/// message for invalid lengths, a zero element digest, or invalid capacity,
/// and `BAD_ARG` for invalid pointers. The caller releases output with
/// [`rezzy_free`]. A zero `element_count` is valid and creates an empty sketch.
///
/// # Safety
/// `elements` must reference `elements_len` readable bytes when non-null;
/// `out` and `out_len` must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_recon_gf256_sketch(
    elements: *const u8,
    elements_len: usize,
    element_count: usize,
    capacity: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    guard(out, out_len, || {
        // SAFETY: non-null output slots are required to be writable.
        if !unsafe { reset_outputs(out, out_len) } || (elements.is_null() && elements_len != 0) {
            return BAD_ARG;
        }
        let Some(expected_len) = element_count.checked_mul(32) else {
            hand_over(
                b"element byte length overflows usize".to_vec(),
                out,
                out_len,
            );
            return ERR;
        };
        if elements_len != expected_len {
            hand_over(
                b"elements_len must equal element_count * 32".to_vec(),
                out,
                out_len,
            );
            return ERR;
        }
        // SAFETY: pointer and length were validated; an empty slice does not
        // dereference the pointer.
        let bytes = if elements_len == 0 {
            &[]
        } else {
            // SAFETY: the pointer is non-null and the caller promised
            // `elements_len` readable bytes.
            unsafe { std::slice::from_raw_parts(elements, elements_len) }
        };
        let mut sketch = match Gf256Sketch::new(capacity) {
            Ok(sketch) => sketch,
            Err(error) => {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                return ERR;
            }
        };
        for element in bytes.chunks_exact(32) {
            let Some(value) = Gf256::from_le_bytes(element) else {
                hand_over(b"invalid 32-byte digest element".to_vec(), out, out_len);
                return ERR;
            };
            if let Err(error) = sketch.toggle(value) {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                return ERR;
            }
        }
        hand_over(sketch.to_bytes(), out, out_len);
        OK
    })
}

/// Decodes the symmetric difference represented by two GF(2^256) sketches.
///
/// Returns `OK` with concatenated 32-byte digest values, `ERR` with a message
/// when lengths or decoding are invalid, or `BAD_ARG` for invalid pointers.
/// The caller releases output with [`rezzy_free`].
///
/// # Safety
/// Each non-null sketch pointer must reference its stated readable length;
/// `out` and `out_len` must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_recon_gf256_difference(
    left: *const u8,
    left_len: usize,
    right: *const u8,
    right_len: usize,
    capacity: usize,
    max_elements: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    guard(out, out_len, || {
        // SAFETY: non-null output slots are required to be writable.
        if !unsafe { reset_outputs(out, out_len) } || left.is_null() || right.is_null() {
            return BAD_ARG;
        }
        // SAFETY: non-null pointers are required to reference their stated lengths.
        let left_bytes = unsafe { std::slice::from_raw_parts(left, left_len) };
        // SAFETY: non-null pointers are required to reference their stated lengths.
        let right_bytes = unsafe { std::slice::from_raw_parts(right, right_len) };
        let left = match Gf256Sketch::from_bytes(capacity, left_bytes) {
            Ok(sketch) => sketch,
            Err(error) => {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                return ERR;
            }
        };
        let right = match Gf256Sketch::from_bytes(capacity, right_bytes) {
            Ok(sketch) => sketch,
            Err(error) => {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                return ERR;
            }
        };
        let Some(decoder) = Gf256Decoder::cached() else {
            hand_over(
                b"could not initialize GF(2^256) decoder".to_vec(),
                out,
                out_len,
            );
            return ERR;
        };
        match Gf256Sketch::extract_symmetric_difference(&left, &right, max_elements, decoder) {
            Ok(elements) => {
                hand_over(elements.into_iter().flatten().collect(), out, out_len);
                OK
            }
            Err(error) => {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                ERR
            }
        }
    })
}

/// HAMT and reconciliation operations over one JSON request/response.
///
/// `rezzy_state_hamt` accepts a room ID plus a request with one of:
///
/// - `{"op":"root","entries":[[event_type,state_key,event_id],...]}`
/// - `{"op":"lookup","entries":[...],"event_type":"...","state_key":"..."}`
/// - `{"op":"diff","a":[...],"b":[...]}`
///
/// The response is the JSON described in `include/rezzy.h`.
///
/// # Safety
/// `room_id` must be NUL-terminated; `request_json` must reference
/// `request_len` readable bytes; `out` and `out_len` must point to writable
/// storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_state_hamt(
    room_id: *const c_char,
    request_json: *const u8,
    request_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    guard(out, out_len, || unsafe {
        let (room_id, request_bytes) =
            match unpack_raw_args(room_id, request_json, request_len, out, out_len) {
                Ok(unpacked) => unpacked,
                Err(status) => return status,
            };
        process_json_request(request_bytes, out, out_len, |request| {
            hamt_response(room_id, request)
        })
    })
}

/// Reconciliation operations over one JSON request/response.
///
/// Accepts `{"op":"digest",...}`, `{"op":"sketch",...}`, or
/// `{"op":"diff",...}` as described in `include/rezzy.h`.
///
/// # Safety
/// `request_json` must reference `request_len` readable bytes; `out` and
/// `out_len` must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_recon(
    request_json: *const u8,
    request_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    guard(out, out_len, || unsafe {
        let request_bytes = match unpack_json_args(request_json, request_len, out, out_len) {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        process_json_request(request_bytes, out, out_len, recon_response)
    })
}

/// Opaque running reconciliation state for one room: the digest and the
/// estimator that `ResidentKernel` maintains as events are added and removed.
/// Elements are `ElementHash` values, the MSC4521 `algebraic_v1` profile that
/// other servers (for example congruent) speak on the wire.
///
/// A handle is not thread-safe; callers serialize access to one handle.
pub struct RezzyKernel {
    inner: ResidentKernel,
}

/// Creates an empty kernel. Release it with [`rezzy_kernel_free`].
#[no_mangle]
pub extern "C" fn rezzy_kernel_new() -> *mut RezzyKernel {
    Box::into_raw(Box::new(RezzyKernel {
        inner: ResidentKernel::new(),
    }))
}

/// Releases a kernel. Accepts null; free each handle once.
///
/// # Safety
/// `kernel` must be null or a pointer returned by [`rezzy_kernel_new`] or
/// [`rezzy_kernel_from_bytes`] that has not been freed.
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_free(kernel: *mut RezzyKernel) {
    if !kernel.is_null() {
        // SAFETY: allocated by `Box::new` in this crate, freed once per contract.
        drop(unsafe { Box::from_raw(kernel) });
    }
}

/// Shared prologue for the read-only kernel getters: clears the outputs,
/// rejects null arguments, then runs `body` with the live handle.
///
/// # Safety
/// `kernel` must be null or a live handle; non-null `out` and `out_len` must
/// point to writable storage.
unsafe fn kernel_read(
    kernel: *const RezzyKernel,
    out: *mut *mut u8,
    out_len: *mut usize,
    body: impl FnOnce(&RezzyKernel, *mut *mut u8, *mut usize) -> i32 + std::panic::UnwindSafe,
) -> i32 {
    guard(out, out_len, || {
        // SAFETY: non-null output slots are required to be writable.
        unsafe { reset_outputs(out, out_len) };
        if kernel.is_null() || out.is_null() || out_len.is_null() {
            return BAD_ARG;
        }
        // SAFETY: non-null live handle per the contract.
        body(unsafe { &*kernel }, out, out_len)
    })
}

/// Adds (`insert`) or removes one event ID from the kernel.
///
/// # Safety
/// Same contract as [`rezzy_kernel_insert`].
unsafe fn kernel_update(
    kernel: *mut RezzyKernel,
    event_id: *const u8,
    event_id_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
    insert: bool,
) -> i32 {
    guard(out, out_len, || {
        // SAFETY: non-null output slots are required to be writable.
        unsafe { reset_outputs(out, out_len) };
        if kernel.is_null() || event_id.is_null() || out.is_null() || out_len.is_null() {
            return BAD_ARG;
        }
        // SAFETY: non-null, `event_id_len` readable bytes per the contract.
        let bytes = unsafe { std::slice::from_raw_parts(event_id, event_id_len) };
        let result = std::str::from_utf8(bytes)
            .map_err(|_| String::from("event ID is not UTF-8"))
            .and_then(|id| {
                ElementHash::from_matrix_event_id_any(id)
                    .map_err(|error| std::format!("invalid event ID `{id}`: {error:?}"))
            })
            .and_then(|element| {
                // SAFETY: non-null and exclusively used for this call per the contract.
                let kernel = unsafe { &mut *kernel };
                if insert {
                    kernel.inner.insert(element)
                } else {
                    kernel.inner.remove(element)
                }
                .map_err(|error| std::format!("{error:?}"))
            });
        match result {
            Ok(()) => OK,
            Err(message) => {
                hand_over(message.into_bytes(), out, out_len);
                ERR
            }
        }
    })
}

/// Adds an event ID to the kernel.
///
/// The kernel is an XOR accumulator: inserting the same ID twice cancels it
/// out. Insert each event once. Returns 0, 1 with a message in `*out` for an
/// invalid ID or a limit error, or 2 for null arguments.
///
/// # Safety
/// `kernel` must be a live handle used by one thread at a time. `event_id`
/// must reference `event_id_len` readable bytes. Non-null `out` and `out_len`
/// must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_insert(
    kernel: *mut RezzyKernel,
    event_id: *const u8,
    event_id_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    unsafe { kernel_update(kernel, event_id, event_id_len, out, out_len, true) }
}

/// Removes an event ID from the kernel. Removing from an empty kernel is an
/// error; removing an ID that was never inserted corrupts the digest, so only
/// remove IDs that were inserted.
///
/// # Safety
/// Same contract as [`rezzy_kernel_insert`].
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_remove(
    kernel: *mut RezzyKernel,
    event_id: *const u8,
    event_id_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    unsafe { kernel_update(kernel, event_id, event_id_len, out, out_len, false) }
}

/// Writes the kernel's fixed-size persisted record to `*out`.
///
/// # Safety
/// `kernel` must be a live handle. Non-null `out` and `out_len` must point to
/// writable storage; free the buffer with [`rezzy_free`].
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_to_bytes(
    kernel: *const RezzyKernel,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        kernel_read(kernel, out, out_len, |kernel, out, out_len| {
            hand_over(kernel.inner.to_bytes().to_vec(), out, out_len);
            OK
        })
    }
}

/// Writes `{"digest":"<base64url>","count":N}` for the kernel's current set,
/// the same shape `rezzy_recon` returns for `digest`.
///
/// # Safety
/// Same contract as [`rezzy_kernel_to_bytes`].
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_digest(
    kernel: *const RezzyKernel,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        kernel_read(kernel, out, out_len, |kernel, out, out_len| {
            let accumulator = kernel.inner.accumulator();
            let response = rezzy::json!({
                "digest": accumulator.encode_digest(),
                "count": accumulator.known_event_count(),
            });
            write_json_response(&response, out, out_len)
        })
    }
}

/// Writes the kernel's 32 base64url-encoded strata coordinates as JSON.
///
/// # Safety
/// Same contract as [`rezzy_kernel_to_bytes`].
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_strata(
    kernel: *const RezzyKernel,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        kernel_read(kernel, out, out_len, |kernel, out, out_len| {
            let strata = rezzy_recon::encode_strata(kernel.inner.strata());
            let response = rezzy::json!({
                "strata": JsonValue::Array(strata.into_iter().map(JsonValue::String).collect()),
            });
            write_json_response(&response, out, out_len)
        })
    }
}

/// Rebuilds a kernel from the record written by [`rezzy_kernel_to_bytes`].
///
/// On 0, `*kernel` is a new handle to release with [`rezzy_kernel_free`]. On
/// 1, `*kernel` is null and `*out` holds a message (wrong length, unknown
/// version, or an inconsistent record). On 2, an argument was null.
///
/// # Safety
/// `bytes` must reference `bytes_len` readable bytes. Non-null `kernel`, `out`
/// and `out_len` must point to writable storage.
#[no_mangle]
pub unsafe extern "C" fn rezzy_kernel_from_bytes(
    bytes: *const u8,
    bytes_len: usize,
    kernel: *mut *mut RezzyKernel,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    guard(out, out_len, || {
        // SAFETY: non-null output slots are required to be writable.
        unsafe { reset_outputs(out, out_len) };
        if !kernel.is_null() {
            // SAFETY: non-null `kernel` is required to point to writable storage.
            unsafe { *kernel = ptr::null_mut() };
        }
        if bytes.is_null() || kernel.is_null() || out.is_null() || out_len.is_null() {
            return BAD_ARG;
        }
        // SAFETY: non-null, `bytes_len` readable bytes per the contract.
        let record = unsafe { std::slice::from_raw_parts(bytes, bytes_len) };
        match ResidentKernel::from_bytes(record) {
            Ok(inner) => {
                // SAFETY: checked non-null above.
                unsafe { *kernel = Box::into_raw(Box::new(RezzyKernel { inner })) };
                OK
            }
            Err(error) => {
                hand_over(std::format!("{error:?}").into_bytes(), out, out_len);
                ERR
            }
        }
    })
}

/// Stateful requester-side policy handle. The host owns network I/O and feeds
/// each decoded room-diff response back through `rezzy_client_step`.
pub struct RezzyClient {
    local: ResidentKernel,
    policy: ReconciliationClient,
    requests: Vec<BucketRequest>,
    accumulated_roots: Vec<u64>,
    rounds: usize,
    started: bool,
    finished: bool,
}

/// Creates a requester using a snapshot of `kernel` and the default policy.
/// Returns null if `kernel` is null. Release the handle with
/// [`rezzy_client_free`].
///
/// # Safety
/// `kernel` must be null or a live kernel handle. The handle must not be
/// mutated concurrently while this function copies it.
#[no_mangle]
pub unsafe extern "C" fn rezzy_client_new(kernel: *const RezzyKernel) -> *mut RezzyClient {
    if kernel.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: guaranteed by this function's contract.
    let local = unsafe { &*kernel }.inner.clone();
    Box::into_raw(Box::new(RezzyClient {
        local,
        policy: ReconciliationClient::default(),
        requests: Vec::new(),
        accumulated_roots: Vec::new(),
        rounds: 0,
        started: false,
        finished: false,
    }))
}

/// Releases a requester handle. Accepts null; free each handle once.
///
/// # Safety
/// `client` must be null or a pointer returned by [`rezzy_client_new`] that
/// has not been freed.
#[no_mangle]
pub unsafe extern "C" fn rezzy_client_free(client: *mut RezzyClient) {
    if !client.is_null() {
        // SAFETY: allocated by `Box::new` in this crate, freed once per contract.
        drop(unsafe { Box::from_raw(client) });
    }
}

/// Advances one requester exchange step from a JSON message.
///
/// Start with `{"op":"start","digest":"...","known_event_count":N,
/// "strata":[...],"frame_matches":true,"has_unknown_extremity":false}`.
/// Subsequent calls use `{"op":"advance","roots":[...],
/// "failed_buckets":[[depth,prefix],...]}`. Every requested bucket must be
/// represented as either successful (implicit in `roots`) or failed. The
/// response is a JSON action: `synchronized`, `extremity_diff`,
/// `bucket_sketches`, `resolve_roots`, or `round_limit`.
///
/// # Safety
/// `client` must be a live handle used by one thread at a time. The request
/// buffer must be readable for `request_len` bytes; non-null output pointers
/// must point to writable storage. Free output with [`rezzy_free`].
#[no_mangle]
pub unsafe extern "C" fn rezzy_client_step(
    client: *mut RezzyClient,
    request_json: *const u8,
    request_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: pointer validity and exclusive handle access are the caller's
    // responsibility per this function's contract.
    guard(out, out_len, || unsafe {
        if client.is_null() {
            // SAFETY: non-null output slots are writable per the function contract.
            reset_outputs(out, out_len);
            return BAD_ARG;
        }
        let bytes = match unpack_json_args(request_json, request_len, out, out_len) {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        let request = match JsonValue::parse_bytes(bytes) {
            Ok(request) => request,
            Err(error) => {
                hand_over(error.to_string().into_bytes(), out, out_len);
                return ERR;
            }
        };
        // SAFETY: live handle and serialized access are required by contract.
        match client_step_inner(&mut *client, &request) {
            Ok(response) => write_json_response(&response, out, out_len),
            Err(error) => {
                hand_over(error.into_bytes(), out, out_len);
                ERR
            }
        }
    })
}

// One JSON state machine keeps the two step kinds (`start`, `advance`) in one
// place with their shared handle state, which is clearer than threading that
// state through separate functions.
#[allow(clippy::too_many_lines)]
fn client_step_inner(client: &mut RezzyClient, request: &JsonValue) -> Result<JsonValue, String> {
    match require_str(request, "op")? {
        "start" => {
            if client.started {
                return Err(String::from("client exchange has already started"));
            }
            let digest = require_str(request, "digest")?;
            let known_event_count = request
                .get("known_event_count")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| String::from("`known_event_count` must be an unsigned integer"))?;
            let encoded_strata = request
                .get("strata")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| String::from("`strata` must be an array of strings"))?;
            let strata_strings = encoded_strata
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| String::from("`strata` entries must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let strata = rezzy_recon::decode_strata(&strata_strings)
                .map_err(|error| std::format!("invalid strata: {error:?}"))?;
            let frame_matches = request
                .get("frame_matches")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| String::from("`frame_matches` must be a boolean"))?;
            let has_unknown_extremity = request
                .get("has_unknown_extremity")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| String::from("`has_unknown_extremity` must be a boolean"))?;
            let headroom = request
                .get("concurrency_headroom")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0);
            let headroom = usize::try_from(headroom)
                .map_err(|_| String::from("`concurrency_headroom` is too large"))?;
            let remote = RemoteDigest {
                digest: RoomAccumulator::decode_digest(digest)
                    .map_err(|error| std::format!("invalid digest: {error:?}"))?,
                known_event_count,
                strata,
                frame_matches,
                has_unknown_extremity,
            };
            client.started = true;
            let action = client.policy.select_action(&client.local, remote, headroom);
            Ok(client_action_response(client, action))
        }
        "advance" => {
            if !client.started || client.finished || client.requests.is_empty() {
                return Err(String::from("client is not waiting for a bucket response"));
            }
            let roots = parse_u64_array(request, "roots")?;
            let failed = parse_failed_buckets(request)?;
            let failed_set = failed
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            if failed_set.len() != failed.len() {
                return Err(String::from("`failed_buckets` contains duplicates"));
            }
            for node in &failed {
                if !client
                    .requests
                    .iter()
                    .any(|bucket| (bucket.depth, bucket.prefix) == *node)
                {
                    return Err(String::from(
                        "`failed_buckets` contains an unrequested bucket",
                    ));
                }
            }
            for root in &roots {
                let matched = client
                    .requests
                    .iter()
                    .filter(|bucket| {
                        rezzy_recon::bucket_prefix(*root, bucket.depth) == bucket.prefix
                    })
                    .collect::<Vec<_>>();
                if matched.len() != 1 || failed_set.contains(&(matched[0].depth, matched[0].prefix))
                {
                    return Err(String::from(
                        "`roots` contains a value outside the requested buckets",
                    ));
                }
            }
            let successful_buckets = client
                .requests
                .iter()
                .filter(|bucket| !failed_set.contains(&(bucket.depth, bucket.prefix)))
                .map(|bucket| BucketDecodeSuccess {
                    depth: bucket.depth,
                    prefix: bucket.prefix,
                    roots: roots
                        .iter()
                        .copied()
                        .filter(|root| {
                            rezzy_recon::bucket_prefix(*root, bucket.depth) == bucket.prefix
                        })
                        .collect(),
                })
                .collect();
            let batch = BucketDecodeBatch {
                successful_buckets,
                failed_buckets: failed,
            };
            let requests = std::mem::take(&mut client.requests);
            let accumulated_roots = std::mem::take(&mut client.accumulated_roots);
            let global_estimate = request.get("global_estimate").and_then(JsonValue::as_u64);
            let action = ReconciliationClient::transition_bucket_batch(
                batch,
                &requests,
                accumulated_roots,
                global_estimate,
                MAX_BUCKETED_SKETCH_CAPACITY,
            );
            Ok(client_action_response(client, action))
        }
        other => Err(std::format!("unknown client step `{other}`")),
    }
}

fn parse_u64_array(request: &JsonValue, field: &str) -> Result<Vec<u64>, String> {
    request
        .get(field)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| std::format!("`{field}` must be an array"))?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_u64()
                .ok_or_else(|| std::format!("`{field}` entry {index} must be an unsigned integer"))
        })
        .collect()
}

fn parse_failed_buckets(request: &JsonValue) -> Result<Vec<(u8, u64)>, String> {
    request
        .get("failed_buckets")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| String::from("`failed_buckets` must be an array"))?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let pair = value
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| {
                    std::format!("`failed_buckets` entry {index} must be [depth,prefix]")
                })?;
            let depth = pair[0]
                .as_u64()
                .and_then(|depth| u8::try_from(depth).ok())
                .ok_or_else(|| std::format!("`failed_buckets` entry {index} has invalid depth"))?;
            let prefix = pair[1]
                .as_u64()
                .ok_or_else(|| std::format!("`failed_buckets` entry {index} has invalid prefix"))?;
            Ok((depth, prefix))
        })
        .collect()
}

fn client_action_response(client: &mut RezzyClient, action: ClientAction) -> JsonValue {
    match action {
        ClientAction::Synchronized => {
            client.finished = true;
            client.requests.clear();
            rezzy::json!({"action":"synchronized"})
        }
        ClientAction::ExtremityDiff => {
            client.finished = true;
            client.requests.clear();
            rezzy::json!({"action":"extremity_diff"})
        }
        ClientAction::BucketSketches {
            requests,
            accumulated_roots,
        } => {
            if client.rounds >= MAX_RECONCILIATION_ROUNDS {
                client.finished = true;
                client.requests.clear();
                return rezzy::json!({"action":"round_limit"});
            }
            client.rounds = client.rounds.saturating_add(1);
            client.requests.clone_from(&requests);
            client.accumulated_roots.clone_from(&accumulated_roots);
            rezzy::json!({
                "action":"bucket_sketches",
                "requests": JsonValue::Array(requests.iter().map(bucket_request_json).collect()),
                "accumulated_roots": JsonValue::Array(accumulated_roots.into_iter().map(JsonValue::from).collect()),
            })
        }
        ClientAction::ResolveRoots {
            roots,
            ladder_failed,
        } => {
            client.finished = true;
            client.requests.clear();
            rezzy::json!({
                "action":"resolve_roots",
                "roots": JsonValue::Array(roots.into_iter().map(JsonValue::from).collect()),
                "ladder_failed": JsonValue::Array(ladder_failed.into_iter().map(|(depth, prefix)| rezzy::json!([depth, prefix])).collect()),
            })
        }
    }
}

fn bucket_request_json(request: &BucketRequest) -> JsonValue {
    rezzy::json!({
        "depth": request.depth,
        "prefix": request.prefix,
        "capacity": request.capacity,
        "overflow": request.overflow,
    })
}

fn write_json_response(response: &JsonValue, out: *mut *mut u8, out_len: *mut usize) -> i32 {
    match rezzy::json::write_string_value(response) {
        Ok(json) => {
            hand_over(json.into_bytes(), out, out_len);
            OK
        }
        Err(error) => {
            hand_over(error.to_string().into_bytes(), out, out_len);
            ERR
        }
    }
}

fn process_json_request<F>(
    request_bytes: &[u8],
    out: *mut *mut u8,
    out_len: *mut usize,
    operation: F,
) -> i32
where
    F: FnOnce(&JsonValue) -> Result<JsonValue, String>,
{
    let request = match JsonValue::parse_bytes(request_bytes) {
        Ok(request) => request,
        Err(error) => {
            hand_over(error.to_string().into_bytes(), out, out_len);
            return ERR;
        }
    };
    match operation(&request) {
        Ok(response) => write_json_response(&response, out, out_len),
        Err(error) => {
            hand_over(error.into_bytes(), out, out_len);
            ERR
        }
    }
}

/// Resets and validates the output slots and one non-NUL-terminated buffer.
unsafe fn unpack_json_args<'a>(
    input: *const u8,
    input_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> Result<&'a [u8], i32> {
    // SAFETY: non-null output slots are required to be writable.
    if !unsafe { reset_outputs(out, out_len) } || input.is_null() {
        return Err(BAD_ARG);
    }
    // SAFETY: non-null, `input_len` readable bytes per the contract.
    Ok(unsafe { std::slice::from_raw_parts(input, input_len) })
}

fn require_str<'a>(value: &'a JsonValue, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| std::format!("missing or non-string `{field}`"))
}

fn parse_hamt_entries(value: &JsonValue) -> Result<Vec<(StateHamtKey, String)>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| String::from("`entries` must be an array"))?;
    let mut entries = Vec::with_capacity(items.len());
    let mut keys = std::collections::HashSet::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let parts = item.as_array().ok_or_else(|| {
            std::format!("entry {index} must be [event_type, state_key, event_id]")
        })?;
        if parts.len() != 3 {
            return Err(std::format!(
                "entry {index} must have exactly three elements"
            ));
        }
        let event_type = parts[0]
            .as_str()
            .ok_or_else(|| std::format!("entry {index} event_type must be a string"))?;
        let state_key = parts[1]
            .as_str()
            .ok_or_else(|| std::format!("entry {index} state_key must be a string"))?;
        let event_id = parts[2]
            .as_str()
            .ok_or_else(|| std::format!("entry {index} event_id must be a string"))?;
        let key = (rezzy::EventType::from(event_type), state_key.to_owned());
        if !keys.insert(key.clone()) {
            return Err(std::format!(
                "entry {index} duplicates state key ({event_type:?}, {state_key:?})"
            ));
        }
        entries.push((key, event_id.to_owned()));
    }
    Ok(entries)
}

type BuiltStateHamt = (RootHandle, Arc<HamtNode<StateHamtKey, String>>, LtHash);

fn build_state_hamt(
    room_id: &str,
    entries: &[(StateHamtKey, String)],
) -> Result<BuiltStateHamt, String> {
    let mut lattice = LtHash::ZERO;
    for ((event_type, state_key), event_id) in entries {
        lattice.add_seed(&LtHash::seed(event_type.as_str(), state_key, event_id));
    }
    let (handle, root) =
        build_hamt_root_handle(room_id.as_bytes(), &lattice, entries.iter().cloned())
            .map_err(|error| std::format!("{error:?}"))?;
    Ok((handle, root, lattice))
}

fn entry_json((key, value): &(StateHamtKey, String)) -> JsonValue {
    rezzy::json!([key.0.as_str(), key.1.as_str(), value.as_str()])
}

const HAMT_MAX_WALK_DEPTH: usize = 64;
/// Upper bound on one hex-encoded persisted node or root record.
const MAX_HAMT_HEX_LEN: usize = 64 * 1024 * 1024;

fn parse_root_hash(request: &JsonValue, field: &str) -> Result<StructuralHash, String> {
    let text = require_str(request, field)?;
    let bytes = hex_decode_limited(text, 64)?;
    bytes
        .try_into()
        .map_err(|_| std::format!("`{field}` must be 32 bytes"))
}

fn parse_node_list(
    value: &JsonValue,
) -> Result<std::collections::BTreeMap<StructuralHash, Vec<u8>>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| String::from("`nodes` must be an array of [hash, bytes] pairs"))?;
    let mut nodes = std::collections::BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        let parts = item
            .as_array()
            .ok_or_else(|| std::format!("node {index} must be [hash, bytes]"))?;
        if parts.len() != 2 {
            return Err(std::format!("node {index} must have exactly two elements"));
        }
        let hash_text = parts[0]
            .as_str()
            .ok_or_else(|| std::format!("node {index} hash must be a string"))?;
        let bytes_text = parts[1]
            .as_str()
            .ok_or_else(|| std::format!("node {index} bytes must be a string"))?;
        let hash: StructuralHash = hex_decode_limited(hash_text, 64)?
            .try_into()
            .map_err(|_| std::format!("node {index} hash must be 32 bytes"))?;
        let bytes = hex_decode_limited(bytes_text, MAX_HAMT_HEX_LEN)?;
        nodes.insert(hash, bytes);
    }
    Ok(nodes)
}

fn node_json((hash, bytes): (&StructuralHash, &Vec<u8>)) -> JsonValue {
    rezzy::json!([hex_encode(hash), hex_encode(bytes)])
}

fn collect_persisted_nodes(
    node: &HamtNode<StateHamtKey, String>,
    out: &mut std::collections::BTreeMap<StructuralHash, Vec<u8>>,
) {
    if out.contains_key(&node.structural_hash) {
        return;
    }
    out.insert(
        node.structural_hash,
        PersistedInternalNode::from(node).encode_v1(),
    );
    for child in &node.children {
        if let NodeRef::Resolved(child) = child {
            collect_persisted_nodes(child, out);
        }
    }
}

/// Hashes reachable from `root` that are not yet in `nodes`.
fn needed_node_hashes(
    structural_key: &[u8],
    root: StructuralHash,
    nodes: &std::collections::BTreeMap<StructuralHash, Vec<u8>>,
) -> Result<Vec<StructuralHash>, String> {
    let mut needed = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![(root, 0_usize)];
    while let Some((hash, depth)) = stack.pop() {
        if depth > HAMT_MAX_WALK_DEPTH {
            return Err(String::from("persisted HAMT exceeds maximum depth"));
        }
        if !seen.insert(hash) {
            continue;
        }
        let Some(bytes) = nodes.get(&hash) else {
            needed.insert(hash);
            continue;
        };
        let node = PersistedInternalNode::<StateHamtKey, String>::decode_v1_verified(
            bytes,
            structural_key,
            hash,
        )
        .map_err(|error| std::format!("node {} failed verification: {error}", hex_encode(&hash)))?;
        for child in &node.children {
            stack.push((child.structural_hash(), depth.saturating_add(1)));
        }
    }
    Ok(needed.into_iter().collect())
}

fn missing_json(hash: &StructuralHash) -> JsonValue {
    rezzy::json!({ "missing": [hex_encode(hash)] })
}

/// Applies `mutations` to the persisted HAMT described by `parent_root_record`
/// in O(|mutations| * log n), carrying the lattice forward with
/// `sub_seed`/`add_seed` instead of rehashing every entry.
///
/// Only the nodes on the mutated paths are needed. When one is absent from
/// `nodes` the response is `{"missing":[hash]}`; the caller loads it and
/// retries. Without `parent_root_record` the base is the empty state.
/// `entry_count` is derived from `parent_entry_count` and the mutations.
#[allow(clippy::too_many_lines)]
fn apply_mutations(room_id: &str, request: &JsonValue) -> Result<JsonValue, String> {
    let (parent_root, mut lattice) = match request.get("parent_root_record") {
        Some(value) if !value.is_null() => {
            let record = hex_decode_limited(
                value
                    .as_str()
                    .ok_or_else(|| String::from("`parent_root_record` must be a hex string"))?,
                8192,
            )?;
            let decoded =
                decode_state_hamt_root(&record).map_err(|error| std::format!("{error}"))?;
            if decoded.room_id != room_id || decoded.prefix != room_id.as_bytes() {
                return Err(std::format!(
                    "parent root record is for room {:?}, not {room_id:?}",
                    decoded.room_id
                ));
            }
            (Some(decoded.root_hash), decoded.lattice)
        }
        _ => (None, LtHash::ZERO),
    };
    let nodes = match request.get("nodes") {
        Some(value) => parse_node_list(value)?,
        None => std::collections::BTreeMap::new(),
    };
    let parent_count = request
        .get("parent_entry_count")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let items = request
        .get("mutations")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| String::from("`mutations` must be an array"))?;
    let mut mutations = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let parts = item.as_array().ok_or_else(|| {
            std::format!("mutation {index} must be [event_type, state_key, event_id|null]")
        })?;
        if parts.len() != 3 {
            return Err(std::format!(
                "mutation {index} must have exactly three elements"
            ));
        }
        let event_type = parts[0]
            .as_str()
            .ok_or_else(|| std::format!("mutation {index} event_type must be a string"))?;
        let state_key = parts[1]
            .as_str()
            .ok_or_else(|| std::format!("mutation {index} state_key must be a string"))?;
        let event_id = if parts[2].is_null() {
            None
        } else {
            Some(
                parts[2]
                    .as_str()
                    .ok_or_else(|| {
                        std::format!("mutation {index} event_id must be a string or null")
                    })?
                    .to_owned(),
            )
        };
        mutations.push((
            (rezzy::EventType::from(event_type), state_key.to_owned()),
            event_id,
        ));
    }

    let prev_root: Arc<HamtNode<StateHamtKey, String>> = match parent_root {
        Some(hash) => {
            let Some(bytes) = nodes.get(&hash) else {
                return Ok(missing_json(&hash));
            };
            Arc::new(
                PersistedInternalNode::<StateHamtKey, String>::decode_v1_verified(
                    bytes,
                    room_id.as_bytes(),
                    hash,
                )
                .map_err(|error| std::format!("parent root failed verification: {error}"))?,
            )
        }
        None => {
            build_hamt_root_handle(
                room_id.as_bytes(),
                &LtHash::ZERO,
                Vec::<(StateHamtKey, String)>::new(),
            )
            .map_err(|error| std::format!("{error:?}"))?
            .1
        }
    };

    let structural_key = room_id.as_bytes();
    let mut resolver =
        |hash: &StructuralHash| -> Result<Arc<HamtNode<StateHamtKey, String>>, StructuralHash> {
            let bytes = nodes.get(hash).ok_or(*hash)?;
            PersistedInternalNode::<StateHamtKey, String>::decode_v1_verified(
                bytes,
                structural_key,
                *hash,
            )
            .map(Arc::new)
            .map_err(|_| *hash)
        };
    let applied = rezzy::hamt::persist_mutations(
        &prev_root,
        structural_key,
        mutations.iter().cloned(),
        &mut resolver,
    );
    let (new_root, displaced, created) = match applied {
        Ok(applied) => applied,
        Err(rezzy::hamt::HamtMutateError::Resolve(hash)) => return Ok(missing_json(&hash)),
        Err(error) => return Err(std::format!("{error:?}")),
    };

    let mut entry_count = parent_count;
    for (((event_type, state_key), new_value), old_value) in mutations.iter().zip(&displaced) {
        if let Some(old) = old_value {
            lattice.sub_seed(&LtHash::seed(event_type.as_str(), state_key, old));
        } else if new_value.is_some() {
            entry_count = entry_count.saturating_add(1);
        }
        if let Some(new) = new_value {
            lattice.add_seed(&LtHash::seed(event_type.as_str(), state_key, new));
        } else if old_value.is_some() {
            entry_count = entry_count.saturating_sub(1);
        }
    }
    let handle = RootHandle::from_lthash(new_root.structural_hash, &lattice);
    let record = encode_state_hamt_root(
        room_id.as_bytes(),
        room_id,
        &handle.structural_hash,
        &lattice,
    )
    .map_err(|error| std::format!("{error}"))?;
    let node_bytes: usize = created.iter().map(|(_, bytes)| bytes.len()).sum();
    Ok(rezzy::json!({
        "root_hash": hex_encode(&handle.structural_hash),
        "state_group_id": hex_encode(&handle.state_group_id),
        "root_record": hex_encode(&record),
        "nodes": JsonValue::Array(created.iter().map(|(hash, bytes)| node_json((hash, bytes))).collect()),
        "entry_count": entry_count,
        "node_count": created.len() as u64,
        "node_bytes": node_bytes as u64,
    }))
}

type StateHamtReader<'a, L> = PersistedHamtReader<'a, StateHamtKey, String, (), L>;

#[allow(clippy::too_many_lines)]
fn hamt_response(room_id: &str, request: &JsonValue) -> Result<JsonValue, String> {
    match require_str(request, "op")? {
        "root" => {
            let entries = parse_hamt_entries(
                request
                    .get("entries")
                    .ok_or_else(|| String::from("missing `entries`"))?,
            )?;
            let (handle, root, lattice) = build_state_hamt(room_id, &entries)?;
            let persist = request
                .get("persist")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            if !persist {
                return Ok(rezzy::json!({
                    "root_hash": hex_encode(&handle.structural_hash),
                    "state_group_id": hex_encode(&handle.state_group_id),
                }));
            }
            let record = encode_state_hamt_root(
                room_id.as_bytes(),
                room_id,
                &handle.structural_hash,
                &lattice,
            )
            .map_err(|error| std::format!("{error}"))?;
            let mut nodes = std::collections::BTreeMap::new();
            collect_persisted_nodes(&root, &mut nodes);
            let node_bytes: usize = nodes.values().map(Vec::len).sum();
            Ok(rezzy::json!({
                "root_hash": hex_encode(&handle.structural_hash),
                "state_group_id": hex_encode(&handle.state_group_id),
                "root_record": hex_encode(&record),
                "nodes": JsonValue::Array(nodes.iter().map(node_json).collect()),
                "entry_count": entries.len() as u64,
                "node_count": nodes.len() as u64,
                "node_bytes": node_bytes as u64,
            }))
        }
        "apply" => apply_mutations(room_id, request),
        "needed" => {
            let root = parse_root_hash(request, "root_hash")?;
            let nodes = parse_node_list(
                request
                    .get("nodes")
                    .ok_or_else(|| String::from("missing `nodes`"))?,
            )?;
            let needed = needed_node_hashes(room_id.as_bytes(), root, &nodes)?;
            Ok(rezzy::json!({
                "needed": JsonValue::Array(
                    needed.iter().map(|hash| JsonValue::String(hex_encode(hash))).collect()
                ),
            }))
        }
        "lookup_root" => {
            let root = parse_root_hash(request, "root_hash")?;
            let nodes = parse_node_list(
                request
                    .get("nodes")
                    .ok_or_else(|| String::from("missing `nodes`"))?,
            )?;
            let key_items = request
                .get("keys")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| String::from("`keys` must be an array of [type, state_key]"))?;
            let mut keys = Vec::with_capacity(key_items.len());
            for (index, item) in key_items.iter().enumerate() {
                let parts = item
                    .as_array()
                    .ok_or_else(|| std::format!("key {index} must be [event_type, state_key]"))?;
                if parts.len() != 2 {
                    return Err(std::format!("key {index} must have exactly two elements"));
                }
                let event_type = parts[0]
                    .as_str()
                    .ok_or_else(|| std::format!("key {index} event_type must be a string"))?;
                let state_key = parts[1]
                    .as_str()
                    .ok_or_else(|| std::format!("key {index} state_key must be a string"))?;
                keys.push((rezzy::EventType::from(event_type), state_key.to_owned()));
            }
            let mut nodes_for_reader = nodes;
            let mut reader: StateHamtReader<'_, _> =
                PersistedHamtReader::new(room_id.as_bytes(), move |hash| {
                    Ok::<_, ()>(nodes_for_reader.remove(hash))
                });
            match reader.get_many(root, &keys) {
                Ok(values) => Ok(rezzy::json!({
                    "values": JsonValue::Array(
                        values
                            .into_iter()
                            .map(|value| value.map_or(JsonValue::Null, JsonValue::String))
                            .collect()
                    ),
                })),
                Err(HamtReadError::Missing(hash)) => Ok(missing_json(&hash)),
                Err(error) => Err(std::format!("{error:?}")),
            }
        }
        "entries_root" => {
            let root = parse_root_hash(request, "root_hash")?;
            let nodes = parse_node_list(
                request
                    .get("nodes")
                    .ok_or_else(|| String::from("missing `nodes`"))?,
            )?;
            let mut nodes_for_reader = nodes;
            let mut reader: StateHamtReader<'_, _> =
                PersistedHamtReader::new(room_id.as_bytes(), move |hash| {
                    Ok::<_, ()>(nodes_for_reader.remove(hash))
                });
            match reader.collect_entries(root) {
                Ok(entries) => Ok(rezzy::json!({
                    "entries": JsonValue::Array(entries.iter().map(entry_json).collect()),
                })),
                Err(HamtReadError::Missing(hash)) => Ok(missing_json(&hash)),
                Err(error) => Err(std::format!("{error:?}")),
            }
        }
        "verify" => {
            let record_text = require_str(request, "root_record")?;
            let record = hex_decode_limited(record_text, 8192)?;
            let decoded =
                decode_state_hamt_root(&record).map_err(|error| std::format!("{error}"))?;
            if decoded.room_id != room_id {
                return Err(std::format!(
                    "root record is for room {:?}, not {room_id:?}",
                    decoded.room_id
                ));
            }
            if decoded.prefix != room_id.as_bytes() {
                return Err(String::from(
                    "root record prefix does not match the structural key",
                ));
            }
            let nodes = parse_node_list(
                request
                    .get("nodes")
                    .ok_or_else(|| String::from("missing `nodes`"))?,
            )?;
            let mut nodes_for_reader = nodes;
            let mut reader: StateHamtReader<'_, _> =
                PersistedHamtReader::new(room_id.as_bytes(), move |hash| {
                    Ok::<_, ()>(nodes_for_reader.remove(hash))
                });
            match reader.collect_entries(decoded.root_hash) {
                Ok(entries) => {
                    // The lattice in the root record must be the one the
                    // leaves produce, otherwise the record and tree disagree
                    // about the identity of the state set.
                    let mut lattice = LtHash::ZERO;
                    for ((event_type, state_key), event_id) in &entries {
                        lattice.add_seed(&LtHash::seed(event_type.as_str(), state_key, event_id));
                    }
                    if lattice != decoded.lattice {
                        return Err(String::from(
                            "root record lattice does not match its leaves",
                        ));
                    }
                    Ok(rezzy::json!({
                        "root_hash": hex_encode(&decoded.root_hash),
                        "state_group_id": hex_encode(&decoded.state_group_id),
                        "room_id": decoded.room_id,
                        "prefix": hex_encode(&decoded.prefix),
                        "entry_count": entries.len() as u64,
                    }))
                }
                Err(HamtReadError::Missing(hash)) => Ok(missing_json(&hash)),
                Err(error) => Err(std::format!("{error:?}")),
            }
        }
        "lookup" => {
            let entries = parse_hamt_entries(
                request
                    .get("entries")
                    .ok_or_else(|| String::from("missing `entries`"))?,
            )?;
            let (_, root, _) = build_state_hamt(room_id, &entries)?;
            let event_type = require_str(request, "event_type")?;
            let state_key = require_str(request, "state_key")?;
            let key = (rezzy::EventType::from(event_type), state_key.to_owned());
            let value = root.get(room_id.as_bytes(), &key);
            let mut response = rezzy::json!({});
            let _ = response.insert(
                String::from("value"),
                value.map_or(JsonValue::Null, |value| JsonValue::String(value.clone())),
            );
            Ok(response)
        }
        "diff" => {
            let a = parse_hamt_entries(
                request
                    .get("a")
                    .ok_or_else(|| String::from("missing `a`"))?,
            )?;
            let b = parse_hamt_entries(
                request
                    .get("b")
                    .ok_or_else(|| String::from("missing `b`"))?,
            )?;
            let (_, root_a, _) = build_state_hamt(room_id, &a)?;
            let (_, root_b, _) = build_state_hamt(room_id, &b)?;
            let mut resolver =
                |_hash: &StructuralHash| -> Result<Arc<HamtNode<StateHamtKey, String>>, String> {
                    Err(String::from("unexpected lazy HAMT node"))
                };
            let (added, removed) = diff_hamt_nodes(&root_a, &root_b, &mut resolver)
                .map_err(|error| std::format!("{error:?}"))?;
            Ok(rezzy::json!({
                "added": JsonValue::Array(added.iter().map(entry_json).collect()),
                "removed": JsonValue::Array(removed.iter().map(entry_json).collect()),
            }))
        }
        other => Err(std::format!("unknown HAMT op `{other}`")),
    }
}

fn parse_event_ids(request: &JsonValue, field: &str) -> Result<Vec<String>, String> {
    let items = request
        .get(field)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| std::format!("`{field}` must be an array"))?;
    let mut ids = Vec::with_capacity(items.len());
    let mut seen = std::collections::HashSet::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let id = item
            .as_str()
            .ok_or_else(|| std::format!("`{field}` entry {index} must be a string"))?;
        if !seen.insert(id) {
            return Err(std::format!("`{field}` contains duplicate event ID `{id}`"));
        }
        ids.push(id.to_owned());
    }
    Ok(ids)
}

fn event_elements(ids: &[String]) -> Result<Vec<FixedWindowElementHash>, String> {
    ids.iter()
        .map(|id| {
            FixedWindowElementHash::from_matrix_event_id_any(id)
                .map_err(|error| std::format!("invalid event ID `{id}`: {error:?}"))
        })
        .collect()
}

fn request_capacity(request: &JsonValue) -> Result<usize, String> {
    let capacity = request
        .get("capacity")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| String::from("`capacity` must be a non-negative integer"))?;
    let capacity =
        usize::try_from(capacity).map_err(|_| String::from("`capacity` is too large"))?;
    if !(1..=MAX_RECON_CAPACITY).contains(&capacity) {
        return Err(std::format!(
            "`capacity` must be between 1 and {MAX_RECON_CAPACITY}"
        ));
    }
    Ok(capacity)
}

// One dispatch keeps every reconciliation request/response pair comparable
// side by side; splitting it by operation would scatter that contract.
#[allow(clippy::too_many_lines)]
fn recon_response(request: &JsonValue) -> Result<JsonValue, String> {
    match require_str(request, "op")? {
        "digest" => {
            let ids = parse_event_ids(request, "event_ids")?;
            let elements = event_elements(&ids)?;
            let mut accumulator = RoomAccumulator::new();
            for element in elements {
                accumulator
                    .insert(element)
                    .map_err(|error| std::format!("{error:?}"))?;
            }
            Ok(rezzy::json!({
                "digest": accumulator.encode_digest(),
                "count": accumulator.known_event_count(),
            }))
        }
        "sketch" => {
            let ids = parse_event_ids(request, "event_ids")?;
            let elements = event_elements(&ids)?;
            let capacity = request_capacity(request)?;
            let mut sketch =
                Gf256Sketch::new(capacity).map_err(|error| std::format!("{error:?}"))?;
            for element in elements {
                sketch
                    .toggle(element.sketch_element())
                    .map_err(|error| std::format!("{error:?}"))?;
            }
            Ok(rezzy::json!({
                "sketch": hex_encode(&sketch.to_bytes()),
                "capacity": capacity,
            }))
        }
        "diff" => {
            let ids = parse_event_ids(request, "local_event_ids")?;
            let elements = event_elements(&ids)?;
            let capacity = request_capacity(request)?;
            let remote_hex = require_str(request, "remote_sketch")?;
            let expected_hex_len = capacity
                .checked_mul(64)
                .ok_or_else(|| String::from("sketch length overflows usize"))?;
            if remote_hex.len() != expected_hex_len {
                return Err(std::format!(
                    "remote sketch hex must be exactly {expected_hex_len} characters"
                ));
            }
            let remote_bytes = hex_decode(remote_hex)?;
            let remote = Gf256Sketch::from_bytes(capacity, &remote_bytes)
                .map_err(|error| std::format!("{error:?}"))?;
            let mut local_sketch =
                Gf256Sketch::new(capacity).map_err(|error| std::format!("{error:?}"))?;
            let mut local_by_digest = std::collections::BTreeMap::new();
            for (id, element) in ids.iter().zip(&elements) {
                local_sketch
                    .toggle(element.sketch_element())
                    .map_err(|error| std::format!("{error:?}"))?;
                local_by_digest.insert(element.digest(), id.clone());
            }
            let decoder = Gf256Decoder::cached()
                .ok_or_else(|| String::from("could not initialize GF(2^256) decoder"))?;
            let difference = Gf256Sketch::extract_symmetric_difference(
                &local_sketch,
                &remote,
                capacity,
                decoder,
            )
            .map_err(|error| std::format!("{error:?}"))?;
            let mut local_only = Vec::new();
            let mut remote_only = Vec::new();
            for digest in &difference {
                if let Some(id) = local_by_digest.get(digest) {
                    local_only.push(id.clone());
                } else {
                    remote_only.push(hex_encode(digest));
                }
            }
            Ok(rezzy::json!({
                "local_only": local_only,
                "remote_only": remote_only,
                "count": difference.len(),
            }))
        }
        // Responder-side snapshot: the algebraic_v1 digest, count and strata
        // for a room's event set. `format` ("v4plus" or "v3") forces one ID format; by default each ID is detected.
        "snapshot" => {
            let elements = algebraic_event_elements(request)?;
            let mut kernel = ResidentKernel::new();
            for (_, element) in &elements {
                kernel
                    .insert(*element)
                    .map_err(|error| std::format!("{error:?}"))?;
            }
            Ok(rezzy::json!({
                "digest": kernel.accumulator().encode_digest(),
                "count": kernel.accumulator().known_event_count(),
                "strata": JsonValue::Array(
                    encode_strata(kernel.strata())
                        .into_iter()
                        .map(JsonValue::String)
                        .collect(),
                ),
            }))
        }
        // Responder-side bucket sketches for the requested MSC4521 triage
        // buckets. Returns one unpadded base64url sketch per request.
        "bucket_sketches" => {
            let elements = algebraic_event_elements(request)?;
            let requests = parse_bucket_requests(request)?;
            let population =
                SortedPopulation::new(elements.iter().map(|(_, element)| *element).collect());
            let sketches = build_bucket_sketches(population.h64s(), &requests)
                .map_err(|error| std::format!("{error:?}"))?;
            Ok(rezzy::json!({
                "sketches": JsonValue::Array(
                    sketches
                        .iter()
                        .map(|sketch| JsonValue::String(sketch.encode()))
                        .collect(),
                ),
            }))
        }
        // Responder-side room_diff: subtract the requester's sketches from
        // ours, decode the symmetric difference, and report the events the
        // requester is missing plus the short IDs only it holds.
        "bucket_diff" => {
            let elements = algebraic_event_elements(request)?;
            let requests = parse_bucket_requests(request)?;
            let local_sketches = parse_local_sketches(request, &requests)?;
            let local_digest =
                RoomAccumulator::decode_digest(require_str(request, "local_digest")?)
                    .map_err(|error| std::format!("invalid local_digest: {error:?}"))?;
            let local_count = request
                .get("local_known_event_count")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| {
                    String::from("`local_known_event_count` must be an unsigned integer")
                })?;

            let mut accumulator = RoomAccumulator::new();
            let mut h64_to_events: std::collections::BTreeMap<u64, Vec<(String, ElementHash)>> =
                std::collections::BTreeMap::new();
            for (id, element) in &elements {
                accumulator
                    .insert(*element)
                    .map_err(|error| std::format!("{error:?}"))?;
                h64_to_events
                    .entry(element.h64)
                    .or_default()
                    .push((id.clone(), *element));
            }

            let residual_digest = accumulator.digest() ^ local_digest;
            if residual_digest == 0 && accumulator.known_event_count() == local_count {
                return Ok(rezzy::json!({
                    "status": "decoded",
                    "missing_event_ids": JsonValue::Array(Vec::new()),
                    "requester_only_short_ids": JsonValue::Array(Vec::new()),
                    "expected_requester_side_accumulator":
                        RoomAccumulator::new().encode_digest(),
                    "count": accumulator.known_event_count(),
                }));
            }

            let population =
                SortedPopulation::new(elements.iter().map(|(_, element)| *element).collect());
            let responder_sketches = build_bucket_sketches(population.h64s(), &requests)
                .map_err(|error| std::format!("{error:?}"))?;

            let mut responder_side = RoomAccumulator::new();
            let mut missing_event_ids: Vec<String> = Vec::new();
            let mut requester_only_short_ids: Vec<String> = Vec::new();
            for ((request, local_sketch), responder_sketch) in requests
                .iter()
                .zip(&local_sketches)
                .zip(&responder_sketches)
            {
                let residual = responder_sketch
                    .subtract(local_sketch)
                    .map_err(|error| std::format!("{error:?}"))?;
                if residual
                    .coordinates()
                    .iter()
                    .all(|&coordinate| coordinate == 0)
                {
                    continue;
                }
                let roots = match residual.decode_elements(request.capacity) {
                    Ok(roots) => roots,
                    Err(
                        rezzy_recon::AlgebraicError::DecodeFailure
                        | rezzy_recon::AlgebraicError::BudgetExhausted,
                    ) => {
                        return Ok(rezzy::json!({
                            "status": "capacity_exceeded",
                            "missing_event_ids": JsonValue::Array(Vec::new()),
                            "requester_only_short_ids": JsonValue::Array(Vec::new()),
                            "count": accumulator.known_event_count(),
                        }));
                    }
                    Err(error) => return Err(std::format!("{error:?}")),
                };
                for root in roots {
                    if let Some(events) = h64_to_events.get(&root) {
                        for (event_id, element) in events {
                            missing_event_ids.push(event_id.clone());
                            responder_side
                                .insert(*element)
                                .map_err(|error| std::format!("{error:?}"))?;
                        }
                    } else {
                        requester_only_short_ids.push(
                            base64::engine::general_purpose::URL_SAFE_NO_PAD
                                .encode(root.to_be_bytes()),
                        );
                    }
                }
            }

            Ok(rezzy::json!({
                "status": "decoded",
                "missing_event_ids": JsonValue::Array(
                    missing_event_ids.into_iter().map(JsonValue::String).collect(),
                ),
                "requester_only_short_ids": JsonValue::Array(
                    requester_only_short_ids.into_iter().map(JsonValue::String).collect(),
                ),
                "expected_requester_side_accumulator": RoomAccumulator::from_parts(
                    residual_digest ^ responder_side.digest(),
                    0,
                )
                .encode_digest(),
                "count": accumulator.known_event_count(),
            }))
        }
        other => Err(std::format!("unknown reconciliation op `{other}`")),
    }
}

/// Parses the event IDs of an `algebraic_v1` request and derives their
/// `ElementHash`es. `format` selects the reference-hash base64 alphabet:
/// `"v4plus"` (default, room versions 4+) or `"v3"` (room version 3).
fn algebraic_event_elements(request: &JsonValue) -> Result<Vec<(String, ElementHash)>, String> {
    let ids = parse_event_ids(request, "event_ids")?;
    // Without `format`, detect each ID's format, as the kernel handle does, so
    // rooms that mix v3 and v4+ IDs hash the same way on both sides.
    let format = match request.get("format").and_then(JsonValue::as_str) {
        None => None,
        Some("v4plus") => Some(EventIdFormat::V4Plus),
        Some("v3") => Some(EventIdFormat::V3),
        Some(other) => return Err(std::format!("unknown event ID format `{other}`")),
    };
    ids.into_iter()
        .map(|id| {
            format
                .map_or_else(
                    || ElementHash::from_matrix_event_id_any(&id),
                    |format| ElementHash::from_matrix_event_id(&id, format),
                )
                .map(|element| (id.clone(), element))
                .map_err(|error| std::format!("invalid event ID `{id}`: {error:?}"))
        })
        .collect()
}

/// Parses MSC4521 triage bucket requests. `prefix` may be a number or a
/// decimal string, matching the wire format.
fn parse_bucket_requests(request: &JsonValue) -> Result<Vec<BucketRequest>, String> {
    let items = request
        .get("requests")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| String::from("`requests` must be an array"))?;
    let mut requests = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let depth = item
            .get("depth")
            .and_then(JsonValue::as_u64)
            .and_then(|depth| u8::try_from(depth).ok())
            .ok_or_else(|| std::format!("request {index} has invalid `depth`"))?;
        let prefix = item
            .get("prefix")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .ok_or_else(|| std::format!("request {index} has invalid `prefix`"))?;
        let capacity = item
            .get("capacity")
            .and_then(JsonValue::as_u64)
            .and_then(|capacity| usize::try_from(capacity).ok())
            .ok_or_else(|| std::format!("request {index} has invalid `capacity`"))?;
        let overflow = item
            .get("overflow")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        requests.push(if overflow {
            BucketRequest::with_overflow(depth, prefix, capacity)
        } else {
            BucketRequest::new(depth, prefix, capacity)
        });
    }
    Ok(requests)
}

/// Decodes the requester's sketches, one per bucket request.
fn parse_local_sketches(
    request: &JsonValue,
    requests: &[BucketRequest],
) -> Result<Vec<SyndromeSketch>, String> {
    let sketches = request
        .get("local_sketches")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| String::from("`local_sketches` must be an array"))?;
    if sketches.len() != requests.len() {
        return Err(String::from(
            "`local_sketches` length must match `requests` length",
        ));
    }
    sketches
        .iter()
        .zip(requests)
        .enumerate()
        .map(|(index, (value, request))| {
            let encoded = value
                .as_str()
                .ok_or_else(|| std::format!("`local_sketches` entry {index} must be a string"))?;
            SyndromeSketch::decode(request.capacity, encoded)
                .map_err(|error| std::format!("invalid local sketch {index}: {error:?}"))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Longest hex string `hex_decode` accepts: one maximum-capacity sketch.
const MAX_HEX_LEN: usize = MAX_RECON_CAPACITY * 32 * 2;

fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    hex_decode_limited(text, MAX_HEX_LEN)
}

fn hex_decode_limited(text: &str, max_len: usize) -> Result<Vec<u8>, String> {
    if text.len() > max_len {
        return Err(std::format!("hex string exceeds {max_len} characters"));
    }
    if !text.len().is_multiple_of(2) {
        return Err(String::from("hex string must have an even length"));
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for chunk in text.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(chunk).map_err(|_| String::from("hex is not UTF-8"))?;
        let byte =
            u8::from_str_radix(pair, 16).map_err(|_| std::format!("invalid hex byte `{pair}`"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Unguarded body of [`rezzy_hash_events`].
///
/// # Safety
/// Same contract as [`rezzy_hash_events`].
unsafe fn rezzy_hash_events_inner(
    room_version: *const c_char,
    jsonl: *const u8,
    jsonl_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded from this function's contract.
    let (version, bytes) =
        match unsafe { unpack_raw_args(room_version, jsonl, jsonl_len, out, out_len) } {
            Ok(unpacked) => unpacked,
            Err(status) => return status,
        };
    let Ok(text) = std::str::from_utf8(bytes) else {
        hand_over(b"input is not valid UTF-8".to_vec(), out, out_len);
        return ERR;
    };
    let records = rezzy::event_report::describe_jsonl(text, version);
    if records.is_empty() {
        hand_over(b"no events in input".to_vec(), out, out_len);
        return ERR;
    }
    let report = rezzy::event_report::report(records, version);
    write_json_response(&report, out, out_len)
}

/// Releases a buffer previously returned through `out`.
///
/// # Safety
/// `ptr`/`len` must be exactly a value and length returned by this library,
/// freed at most once. NULL is accepted and ignored.
#[no_mangle]
pub unsafe extern "C" fn rezzy_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `hand_over` leaked a `Box<[u8]>` of exactly this pointer and length.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    const EVENT: &str = r#"{"auth_events":["$sbusQdJu8vp4mYghGgIdFrz9K_UbSNVmVr3UBxLM2Mk","$SbBcrKoU8Wo-rq9hH6NORFEcD7jrSgLkyYBgvlvNZVc"],"content":{"body":"A nice sunny day spent near the window.","m.mentions":{},"msgtype":"m.text"},"depth":265,"hashes":{"sha256":"BN2Mpld9Ox01qmZjEh+2SXjc3hf67Z+UrUvCc/TdJoc"},"origin_server_ts":1763426725083,"prev_events":["$YgQCx1FMxaNlpl8CoymZCmPLcE9YJ95kwnp8VBHJiqY"],"room_id":"!L58ME6ufiP49v97UIOBIpvWKEgj4912JmECPuDzlvCI","sender":"@chtr-x:matrix.org","signatures":{"matrix.org":{"ed25519:a_RXGa":"EKGxFhkSSIlattBwQDE9V1moJdRz6FGd4tC+u43q6z0/8v3oys743hvIStLWTO0LYR1GrntNOHyztbUaIVQOAw"}},"type":"m.room.message","unsigned":{"age_ts":1763426725083}}"#;
    const ID: &str = "$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE";

    /// Copies a returned buffer into a `String` and releases it.
    fn take_text(out: *mut u8, len: usize) -> String {
        if out.is_null() {
            return String::new();
        }
        // SAFETY: `out`/`len` were just returned by the library.
        let text =
            unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(out, len)).into_owned() };
        // SAFETY: freeing the buffer just returned, once.
        unsafe { rezzy_free(out, len) };
        text
    }

    fn call(
        f: unsafe extern "C" fn(*const c_char, *const u8, usize, *mut *mut u8, *mut usize) -> i32,
        version: &str,
        event: &str,
    ) -> (i32, String) {
        let version = CString::new(version).unwrap();
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: valid pointers for the stated lengths; `out` freed below.
        let status = unsafe {
            f(
                version.as_ptr(),
                event.as_ptr(),
                event.len(),
                &raw mut out,
                &raw mut len,
            )
        };
        (status, take_text(out, len))
    }

    fn event_id_bytes(id: &str) -> (*const u8, usize) {
        (id.as_ptr(), id.len())
    }

    fn kernel_op(
        f: unsafe extern "C" fn(
            *mut RezzyKernel,
            *const u8,
            usize,
            *mut *mut u8,
            *mut usize,
        ) -> i32,
        kernel: *mut RezzyKernel,
        id: &str,
    ) -> (i32, String) {
        let (ptr_id, len_id) = event_id_bytes(id);
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: valid handle and buffers; `out` released by `take_text`.
        let status = unsafe { f(kernel, ptr_id, len_id, &raw mut out, &raw mut len) };
        (status, take_text(out, len))
    }

    fn kernel_text(
        f: unsafe extern "C" fn(*const RezzyKernel, *mut *mut u8, *mut usize) -> i32,
        kernel: *const RezzyKernel,
    ) -> (i32, Vec<u8>) {
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: valid handle and buffers.
        let status = unsafe { f(kernel, &raw mut out, &raw mut len) };
        let bytes = if out.is_null() {
            Vec::new()
        } else {
            // SAFETY: just returned by the library; freed once.
            let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
            // SAFETY: same buffer, freed exactly once with its returned length.
            unsafe { rezzy_free(out, len) };
            bytes
        };
        (status, bytes)
    }

    #[test]
    fn kernel_matches_one_shot_digest_and_round_trips() {
        let kernel = rezzy_kernel_new();
        for id in [ID_V12, ID_V3] {
            assert_eq!(kernel_op(rezzy_kernel_insert, kernel, id).0, OK);
        }
        let (status, digest) = kernel_text(rezzy_kernel_digest, kernel);
        assert_eq!(status, OK);
        let parsed = JsonValue::parse(std::str::from_utf8(&digest).unwrap()).unwrap();
        assert_eq!(parsed.get("count").and_then(JsonValue::as_u64), Some(2));
        assert!(parsed.get("digest").and_then(JsonValue::as_str).is_some());
        let (status, strata) = kernel_text(rezzy_kernel_strata, kernel);
        assert_eq!(status, OK);
        let strata = JsonValue::parse(std::str::from_utf8(&strata).unwrap()).unwrap();
        let actual_strata = strata.get("strata").and_then(JsonValue::as_array).unwrap();
        // SAFETY: `kernel` is a live handle created above.
        let expected_strata = encode_strata(unsafe { &*kernel }.inner.strata());
        assert_eq!(actual_strata.len(), expected_strata.len());
        for (actual, expected) in actual_strata.iter().zip(expected_strata) {
            assert_eq!(actual.as_str(), Some(expected.as_str()));
        }
        let (_, expected) = call_recon(&format!(
            r#"{{"op":"digest","event_ids":["{ID_V12}","{ID_V3}"]}}"#
        ));
        assert_eq!(parsed, JsonValue::parse(&expected).unwrap());

        // Persist, rebuild, and confirm the same digest.
        let (status, record) = kernel_text(rezzy_kernel_to_bytes, kernel);
        assert_eq!(status, OK);
        let mut rebuilt = ptr::null_mut();
        let (mut out, mut len) = (ptr::null_mut(), 0);
        // SAFETY: valid buffers; handle freed below.
        let status = unsafe {
            rezzy_kernel_from_bytes(
                record.as_ptr(),
                record.len(),
                &raw mut rebuilt,
                &raw mut out,
                &raw mut len,
            )
        };
        assert_eq!(status, OK);
        assert!(out.is_null());
        assert_eq!(
            kernel_text(rezzy_kernel_digest, rebuilt).1,
            expected.into_bytes()
        );

        // Removing an event returns to the digest of the smaller set.
        assert_eq!(kernel_op(rezzy_kernel_remove, kernel, ID_V3).0, OK);
        let (_, one) = call_recon(&format!(r#"{{"op":"digest","event_ids":["{ID_V12}"]}}"#));
        assert_eq!(kernel_text(rezzy_kernel_digest, kernel).1, one.into_bytes());
        // SAFETY: each handle freed once.
        unsafe {
            rezzy_kernel_free(kernel);
            rezzy_kernel_free(rebuilt);
        }
    }

    fn call_client_step(client: *mut RezzyClient, request: &JsonValue) -> (i32, String) {
        let input = rezzy::json::write_string_value(request).unwrap();
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: live handle and valid request/output buffers; response is freed below.
        let status = unsafe {
            rezzy_client_step(
                client,
                input.as_ptr(),
                input.len(),
                &raw mut out,
                &raw mut len,
            )
        };
        (status, take_text(out, len))
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn client_handle_drives_a_reconciliation_round() {
        let local_ids = [ID_V3, ID_V12];
        let remote_ids = [ID_V12, ID_V3_OTHER];
        let kernel = rezzy_kernel_new();
        for id in local_ids {
            assert_eq!(kernel_op(rezzy_kernel_insert, kernel, id).0, OK);
        }
        // SAFETY: `kernel` is a live handle; the client copies it.
        let client = unsafe { rezzy_client_new(kernel) };
        assert!(!client.is_null());

        let mut remote_state = ResidentKernel::new();
        for id in remote_ids {
            let element = ElementHash::from_matrix_event_id_any(id).unwrap();
            remote_state.insert(element).unwrap();
        }
        let start = rezzy::json!({
            "op": "start",
            "digest": remote_state.accumulator().encode_digest(),
            "known_event_count": remote_state.accumulator().known_event_count(),
            "strata": JsonValue::Array(
                rezzy_recon::encode_strata(remote_state.strata())
                    .into_iter()
                    .map(JsonValue::String)
                    .collect()
            ),
            "frame_matches": true,
            "has_unknown_extremity": false,
        });
        let (status, text) = call_client_step(client, &start);
        assert_eq!(status, OK, "{text}");
        let action = JsonValue::parse(&text).unwrap();
        assert_eq!(
            action.get("action").and_then(JsonValue::as_str),
            Some("bucket_sketches")
        );
        let requests = action
            .get("requests")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|request| {
                BucketRequest::new(
                    u8::try_from(request.get("depth").and_then(JsonValue::as_u64).unwrap())
                        .unwrap(),
                    request.get("prefix").and_then(JsonValue::as_u64).unwrap(),
                    usize::try_from(request.get("capacity").and_then(JsonValue::as_u64).unwrap())
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();

        // Simulate the responder: subtract each requester's sketch from its
        // own, decode the differing short IDs, and classify which IDs it has.
        let sorted_h64 = |ids: &[&str]| {
            let mut hashes = ids
                .iter()
                .map(|id| ElementHash::from_matrix_event_id_any(id).unwrap().h64)
                .collect::<Vec<_>>();
            hashes.sort_unstable();
            hashes
        };
        let local_h64 = sorted_h64(&local_ids);
        let remote_h64 = sorted_h64(&remote_ids);
        let local_sketches = rezzy_recon::build_bucket_sketches(&local_h64, &requests).unwrap();
        let remote_sketches = rezzy_recon::build_bucket_sketches(&remote_h64, &requests).unwrap();
        let mut requester_only_roots = Vec::new();
        let mut responder_only_ids = Vec::new();
        for ((request, local), remote) in requests.iter().zip(&local_sketches).zip(&remote_sketches)
        {
            for root in remote
                .subtract(local)
                .unwrap()
                .decode_elements(request.capacity)
                .unwrap()
            {
                if remote_h64.contains(&root) {
                    responder_only_ids.extend(remote_ids.iter().copied().filter(|id| {
                        ElementHash::from_matrix_event_id_any(id).unwrap().h64 == root
                    }));
                } else {
                    requester_only_roots.push(root);
                }
            }
        }
        responder_only_ids.sort_unstable();
        responder_only_ids.dedup();
        assert_eq!(responder_only_ids, vec![ID_V3_OTHER]);
        let advance = rezzy::json!({
            "op": "advance",
            "roots": JsonValue::Array(requester_only_roots.iter().copied().map(JsonValue::from).collect()),
            "failed_buckets": [],
        });
        let (status, text) = call_client_step(client, &advance);
        assert_eq!(status, OK, "{text}");
        let result = JsonValue::parse(&text).unwrap();
        assert_eq!(
            result.get("action").and_then(JsonValue::as_str),
            Some("resolve_roots")
        );
        let roots = result.get("roots").and_then(JsonValue::as_array).unwrap();
        let local_only = ElementHash::from_matrix_event_id_any(ID_V3).unwrap().h64;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_u64(), Some(local_only));

        // SAFETY: each handle was returned by its constructor and is freed once.
        unsafe {
            rezzy_client_free(client);
            rezzy_kernel_free(kernel);
        }
    }

    /// Calls `rezzy_recon` with a JSON value and parses the response.
    fn recon_value(request: &JsonValue) -> JsonValue {
        let (status, text) = call_recon(&rezzy::json::write_string_value(request).unwrap());
        assert_eq!(status, OK, "{text}");
        JsonValue::parse(&text).unwrap()
    }

    /// Runs a whole exchange through the C ABI: the requester uses a kernel
    /// and client handle, the responder uses the `rezzy_recon` operations.
    /// Returns the responder's `bucket_diff` answer and the client's final
    /// action, or the client's `synchronized` action when no round is needed.
    fn exchange(local_ids: &[&str], remote_ids: &[&str]) -> (Option<JsonValue>, JsonValue) {
        let ids =
            |ids: &[&str]| JsonValue::Array(ids.iter().map(|id| JsonValue::from(*id)).collect());
        let kernel = rezzy_kernel_new();
        for id in local_ids {
            assert_eq!(kernel_op(rezzy_kernel_insert, kernel, id).0, OK);
        }
        // SAFETY: `kernel` is a live handle; the client copies it.
        let client = unsafe { rezzy_client_new(kernel) };
        assert!(!client.is_null());
        let (status, digest) = kernel_text(rezzy_kernel_digest, kernel);
        assert_eq!(status, OK);
        let local_digest = JsonValue::parse(std::str::from_utf8(&digest).unwrap()).unwrap();

        // Responder: describe its event set.
        let snapshot = recon_value(&rezzy::json!({
            "op": "snapshot",
            "event_ids": ids(remote_ids),
        }));
        let start = rezzy::json!({
            "op": "start",
            "digest": snapshot.get("digest").unwrap().clone(),
            "known_event_count": snapshot.get("count").unwrap().clone(),
            "strata": snapshot.get("strata").unwrap().clone(),
            "frame_matches": true,
            "has_unknown_extremity": false,
        });
        let (status, text) = call_client_step(client, &start);
        assert_eq!(status, OK, "{text}");
        let action = JsonValue::parse(&text).unwrap();
        if action.get("action").and_then(JsonValue::as_str) != Some("bucket_sketches") {
            // SAFETY: each handle is freed once.
            unsafe {
                rezzy_client_free(client);
                rezzy_kernel_free(kernel);
            }
            return (None, action);
        }

        // Requester: sketch our own buckets. Responder: subtract and decode.
        let requests = action.get("requests").unwrap().clone();
        let local_sketches = recon_value(&rezzy::json!({
            "op": "bucket_sketches",
            "event_ids": ids(local_ids),
            "requests": requests.clone(),
        }));
        let diff = recon_value(&rezzy::json!({
            "op": "bucket_diff",
            "event_ids": ids(remote_ids),
            "requests": requests,
            "local_sketches": local_sketches.get("sketches").unwrap().clone(),
            "local_digest": local_digest.get("digest").unwrap().clone(),
            "local_known_event_count": local_digest.get("count").unwrap().clone(),
        }));

        // Requester: hand back the short IDs only it holds.
        let roots = diff
            .get("requester_only_short_ids")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|short| {
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(short.as_str().unwrap())
                    .unwrap();
                JsonValue::from(u64::from_be_bytes(bytes.try_into().unwrap()))
            })
            .collect::<Vec<_>>();
        let advance = rezzy::json!({
            "op": "advance",
            "roots": JsonValue::Array(roots),
            "failed_buckets": [],
        });
        let (status, text) = call_client_step(client, &advance);
        assert_eq!(status, OK, "{text}");
        // SAFETY: each handle is freed once.
        unsafe {
            rezzy_client_free(client);
            rezzy_kernel_free(kernel);
        }
        (Some(diff), JsonValue::parse(&text).unwrap())
    }

    fn string_list(value: &JsonValue, key: &str) -> Vec<String> {
        value
            .get(key)
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn exchange_through_the_c_abi_finds_the_differences() {
        let (diff, action) = exchange(&[ID_V3, ID_V12], &[ID_V12, ID_V3_OTHER]);
        let diff = diff.expect("sets differ, so a bucket round runs");
        assert_eq!(
            diff.get("status").and_then(JsonValue::as_str),
            Some("decoded")
        );
        // The requester is missing one event, which the responder names.
        assert_eq!(string_list(&diff, "missing_event_ids"), [ID_V3_OTHER]);
        // The responder reports the one short ID that only the requester has,
        // and the client then asks for exactly that root to be resolved.
        let only_requester = string_list(&diff, "requester_only_short_ids");
        assert_eq!(only_requester.len(), 1);
        assert_eq!(
            action.get("action").and_then(JsonValue::as_str),
            Some("resolve_roots")
        );
        let expected = ElementHash::from_matrix_event_id_any(ID_V3).unwrap().h64;
        let roots = action.get("roots").and_then(JsonValue::as_array).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_u64(), Some(expected));
    }

    #[test]
    fn exchange_through_the_c_abi_is_synchronized_for_equal_sets() {
        let (diff, action) = exchange(&[ID_V3, ID_V12], &[ID_V12, ID_V3]);
        assert!(diff.is_none());
        assert_eq!(
            action.get("action").and_then(JsonValue::as_str),
            Some("synchronized")
        );
    }

    #[test]
    fn bucket_operations_reject_bad_requests() {
        let missing_requests = call_recon(&format!(
            r#"{{"op":"bucket_sketches","event_ids":["{ID_V12}"]}}"#
        ));
        assert_eq!(missing_requests.0, ERR);
        let wrong_length = call_recon(&format!(
            r#"{{"op":"bucket_diff","event_ids":["{ID_V12}"],
                "requests":[{{"depth":0,"prefix":0,"capacity":4}}],
                "local_sketches":[],"local_digest":"AAAAAAAAAAAAAAAAAAAAAA",
                "local_known_event_count":0}}"#
        ));
        assert_eq!(wrong_length.0, ERR);
        assert!(wrong_length.1.contains("length"), "{}", wrong_length.1);
    }

    #[test]
    fn version_is_the_crate_version() {
        // SAFETY: returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(rezzy_version()) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn kernel_rejects_bad_input() {
        let kernel = rezzy_kernel_new();
        let (status, message) = kernel_op(rezzy_kernel_insert, kernel, "not-an-event-id");
        assert_eq!(status, ERR);
        assert!(message.contains("invalid event ID"), "{message}");
        assert_eq!(
            kernel_op(rezzy_kernel_insert, ptr::null_mut(), ID_V12).0,
            BAD_ARG
        );
        // Removing from an empty kernel fails instead of corrupting it.
        assert_eq!(kernel_op(rezzy_kernel_remove, kernel, ID_V12).0, ERR);

        let mut handle = ptr::null_mut();
        let (mut out, mut len) = (ptr::null_mut(), 0);
        let short = [1_u8; 3];
        // SAFETY: valid buffers.
        let status = unsafe {
            rezzy_kernel_from_bytes(
                short.as_ptr(),
                short.len(),
                &raw mut handle,
                &raw mut out,
                &raw mut len,
            )
        };
        assert_eq!(status, ERR);
        assert!(handle.is_null());
        // SAFETY: freeing the message once, then the kernel once; null is accepted.
        unsafe {
            rezzy_free(out, len);
            rezzy_kernel_free(handle);
            rezzy_kernel_free(kernel);
        }
    }

    fn call_recon(request: &str) -> (i32, String) {
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: valid pointers for the stated lengths; `out` freed below.
        let status =
            unsafe { rezzy_recon(request.as_ptr(), request.len(), &raw mut out, &raw mut len) };
        (status, take_text(out, len))
    }

    fn call_hamt(room_id: &str, request: &str) -> (i32, String) {
        let room_id = CString::new(room_id).unwrap();
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: valid pointers for the stated lengths; `out` freed below.
        let status = unsafe {
            rezzy_state_hamt(
                room_id.as_ptr(),
                request.as_ptr(),
                request.len(),
                &raw mut out,
                &raw mut len,
            )
        };
        (status, take_text(out, len))
    }

    #[test]
    fn event_id_matches_the_known_v12_id() {
        assert_eq!(call(rezzy_event_id, "12", EVENT), (OK, ID.to_owned()));
    }

    #[test]
    fn claimed_event_id_field_is_ignored() {
        let with_id = EVENT.replacen('{', &format!("{{\"event_id\":\"{ID}x\","), 1);
        assert_eq!(call(rezzy_event_id, "12", &with_id), (OK, ID.to_owned()));
    }

    #[test]
    fn content_hash_verifies_and_detects_tampering() {
        assert_eq!(call(rezzy_verify_content_hash, "12", EVENT).0, OK);
        let tampered = EVENT.replace("sunny", "rainy");
        assert_eq!(call(rezzy_verify_content_hash, "12", &tampered).0, ERR);
    }

    #[test]
    fn errors_carry_a_message_and_bad_args_do_not() {
        let (status, message) = call(rezzy_event_id, "12", "not json");
        assert_eq!(status, ERR);
        assert!(message.starts_with("invalid JSON"));

        let (status, message) = call(rezzy_event_id, "1", EVENT);
        assert_eq!(status, ERR);
        assert!(message.contains("room version"));

        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: null room version is rejected before any dereference.
        let status = unsafe {
            rezzy_event_id(
                ptr::null(),
                EVENT.as_ptr(),
                EVENT.len(),
                &raw mut out,
                &raw mut len,
            )
        };
        assert_eq!(status, BAD_ARG);
        assert!(out.is_null());
    }

    #[test]
    fn hash_events_reports_each_line_and_flags_bad_ones() {
        let jsonl = format!("{EVENT}\n\nnot json\n");
        let (status, text) = call(rezzy_hash_events, "12", &jsonl);
        assert_eq!(status, OK);
        let actual = JsonValue::parse(&text).unwrap();
        let expected =
            rezzy::event_report::report(rezzy::event_report::describe_jsonl(&jsonl, "12"), "12");
        assert_eq!(actual, expected);
        let events = actual.get("events").unwrap().as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].get("event_id").and_then(JsonValue::as_str),
            Some(ID)
        );
        assert_eq!(events[0].get("line").and_then(JsonValue::as_u64), Some(1));
        assert!(events[1]
            .get("error")
            .and_then(JsonValue::as_str)
            .is_some_and(|error| error.contains("invalid JSON")));
        assert_eq!(events[1].get("line").and_then(JsonValue::as_u64), Some(3));
        assert_eq!(call(rezzy_hash_events, "12", "\n").0, ERR);
    }

    #[test]
    fn free_accepts_null() {
        // SAFETY: NULL is documented as a no-op.
        unsafe { rezzy_free(ptr::null_mut(), 0) };
    }

    #[test]
    fn panic_guard_returns_an_error_without_unwinding() {
        let mut out = ptr::null_mut();
        let mut len = 0;
        let status = guard(&raw mut out, &raw mut len, || panic!("test panic"));
        assert_eq!(status, ERR);
        // SAFETY: the guard returned this buffer with its exact length.
        let message = unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(out, len)) };
        assert!(message.contains("panicked"));
        // SAFETY: this is the buffer returned by the guard, freed once.
        unsafe { rezzy_free(out, len) };
    }

    const HAMT_ENTRIES: &str = r#"[["m.room.create","","$create:example.org"],["m.room.member","@alice:example.org","$member:example.org"]]"#;
    const HAMT_ENTRIES_CHANGED: &str = r#"[["m.room.create","","$create:example.org"],["m.room.member","@alice:example.org","$member2:example.org"]]"#;

    #[test]
    fn state_hamt_root_and_lookup() {
        let (status, text) = call_hamt(
            "!room:example.org",
            &format!(r#"{{"op":"root","entries":{HAMT_ENTRIES}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let root = JsonValue::parse(&text).unwrap();
        let root_hash = root.get("root_hash").and_then(JsonValue::as_str).unwrap();
        let state_group_id = root
            .get("state_group_id")
            .and_then(JsonValue::as_str)
            .unwrap();
        assert_eq!(root_hash.len(), 64);
        assert_eq!(state_group_id.len(), 64);

        let (status, text) = call_hamt(
            "!room:example.org",
            &format!(
                r#"{{"op":"lookup","entries":{HAMT_ENTRIES},"event_type":"m.room.member","state_key":"@alice:example.org"}}"#
            ),
        );
        assert_eq!(status, OK, "{text}");
        let value = JsonValue::parse(&text).unwrap();
        assert_eq!(
            value.get("value").and_then(JsonValue::as_str),
            Some("$member:example.org")
        );
    }

    #[test]
    fn state_hamt_diff_reports_both_sides() {
        let (status, text) = call_hamt(
            "!room:example.org",
            &format!(r#"{{"op":"diff","a":{HAMT_ENTRIES},"b":{HAMT_ENTRIES_CHANGED}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let response = JsonValue::parse(&text).unwrap();
        let added = response.get("added").and_then(JsonValue::as_array).unwrap();
        let removed = response
            .get("removed")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(removed.len(), 1);
        assert_eq!(
            added[0].as_array().unwrap()[2].as_str(),
            Some("$member2:example.org")
        );
        assert_eq!(
            removed[0].as_array().unwrap()[2].as_str(),
            Some("$member:example.org")
        );
    }

    fn nodes_argument(nodes: &[JsonValue]) -> String {
        let pairs = nodes
            .iter()
            .map(|node| {
                let parts = node.as_array().unwrap();
                format!(
                    "[{:?},{:?}]",
                    parts[0].as_str().unwrap(),
                    parts[1].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("[{pairs}]")
    }

    fn field(value: &JsonValue, name: &str) -> String {
        value
            .get(name)
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned()
    }

    /// Incremental `apply` must produce exactly what a full rebuild of the
    /// resulting state produces, while resolving only the nodes on the
    /// mutated paths.
    #[test]
    fn state_hamt_apply_matches_full_rebuild() {
        let room = "!room:example.org";
        let entry = |i: usize, version: usize| format!(r#"["m.test","key{i}","$ev{i}v{version}"]"#);
        let list = |entries: &[String]| format!("[{}]", entries.join(","));

        let base: Vec<String> = (0..300).map(|i| entry(i, 0)).collect();
        let (status, text) = call_hamt(
            room,
            &format!(
                r#"{{"op":"root","persist":true,"entries":{}}}"#,
                list(&base)
            ),
        );
        assert_eq!(status, OK, "{text}");
        let parent = JsonValue::parse(&text).unwrap();
        let parent_nodes = parent
            .get("nodes")
            .and_then(JsonValue::as_array)
            .unwrap()
            .clone();
        let parent_record = field(&parent, "root_record");

        // Replace key 7, add key 1000, remove key 42.
        let mutations = r#"[["m.test","key7","$ev7v1"],["m.test","key1000","$ev1000v0"],["m.test","key42",null]]"#;
        let mut want: Vec<String> = base
            .iter()
            .filter(|e| !e.contains("\"key42\""))
            .map(|e| {
                if e.contains("\"key7\"") {
                    entry(7, 1)
                } else {
                    e.clone()
                }
            })
            .collect();
        want.push(entry(1000, 0));
        let (status, text) = call_hamt(
            room,
            &format!(
                r#"{{"op":"root","persist":true,"entries":{}}}"#,
                list(&want)
            ),
        );
        assert_eq!(status, OK, "{text}");
        let rebuilt = JsonValue::parse(&text).unwrap();

        // Resolve lazily: start with no nodes and feed back what is missing.
        let mut supplied: Vec<JsonValue> = Vec::new();
        let applied = loop {
            let request = format!(
                r#"{{"op":"apply","parent_root_record":"{parent_record}","parent_entry_count":300,"nodes":{},"mutations":{mutations}}}"#,
                JsonValue::Array(supplied.clone()),
            );
            let (status, text) = call_hamt(room, &request);
            assert_eq!(status, OK, "{text}");
            let response = JsonValue::parse(&text).unwrap();
            let Some(missing) = response.get("missing").and_then(JsonValue::as_array) else {
                break response;
            };
            let wanted = missing[0].as_str().unwrap();
            let node = parent_nodes
                .iter()
                .find(|n| n.as_array().unwrap()[0].as_str() == Some(wanted))
                .unwrap_or_else(|| panic!("missing node {wanted} is not in the parent tree"));
            supplied.push(node.clone());
        };

        assert_eq!(field(&applied, "root_hash"), field(&rebuilt, "root_hash"));
        assert_eq!(
            field(&applied, "state_group_id"),
            field(&rebuilt, "state_group_id")
        );
        assert_eq!(
            field(&applied, "root_record"),
            field(&rebuilt, "root_record")
        );
        assert_eq!(
            applied.get("entry_count").and_then(JsonValue::as_u64),
            Some(300)
        );
        assert!(
            supplied.len() < parent_nodes.len(),
            "apply loaded {} of {} nodes; it should touch only the mutated paths",
            supplied.len(),
            parent_nodes.len()
        );
        let created = applied
            .get("node_count")
            .and_then(JsonValue::as_u64)
            .unwrap();
        assert!(
            created
                < rebuilt
                    .get("node_count")
                    .and_then(JsonValue::as_u64)
                    .unwrap()
        );
    }

    #[test]
    fn state_hamt_apply_from_empty_matches_build() {
        let room = "!room:example.org";
        let (status, text) = call_hamt(
            room,
            r#"{"op":"apply","nodes":[],"mutations":[["m.room.create","","$c"],["m.room.name","","$n"]]}"#,
        );
        assert_eq!(status, OK, "{text}");
        let applied = JsonValue::parse(&text).unwrap();
        let (status, text) = call_hamt(
            room,
            r#"{"op":"root","persist":true,"entries":[["m.room.create","","$c"],["m.room.name","","$n"]]}"#,
        );
        assert_eq!(status, OK, "{text}");
        let rebuilt = JsonValue::parse(&text).unwrap();
        assert_eq!(
            field(&applied, "root_record"),
            field(&rebuilt, "root_record")
        );
        assert_eq!(
            applied.get("entry_count").and_then(JsonValue::as_u64),
            Some(2)
        );
    }

    #[test]
    fn state_hamt_persist_and_read_from_nodes() {
        let room = "!room:example.org";
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"root","persist":true,"entries":{HAMT_ENTRIES}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let persisted = JsonValue::parse(&text).unwrap();
        let root_hash = persisted
            .get("root_hash")
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned();
        let state_group_id = persisted
            .get("state_group_id")
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned();
        let root_record = persisted
            .get("root_record")
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned();
        assert_eq!(&root_record[..10], "4d54485201", "MTHR\x01 prefix");
        let nodes = persisted
            .get("nodes")
            .and_then(JsonValue::as_array)
            .unwrap()
            .clone();
        assert!(!nodes.is_empty());
        assert_eq!(
            persisted.get("node_count").and_then(JsonValue::as_u64),
            Some(nodes.len() as u64)
        );
        assert_eq!(
            persisted.get("entry_count").and_then(JsonValue::as_u64),
            Some(2)
        );
        let all_nodes = nodes_argument(&nodes);

        // An empty node set reports the root as missing rather than failing.
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"entries_root","root_hash":"{root_hash}","nodes":[]}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let missing = JsonValue::parse(&text).unwrap();
        assert_eq!(
            missing
                .get("missing")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            1
        );

        // With the full set, enumeration and lookup resolve from stored bytes.
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"entries_root","root_hash":"{root_hash}","nodes":{all_nodes}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let entries = JsonValue::parse(&text).unwrap();
        assert_eq!(
            entries
                .get("entries")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            2
        );

        let (status, text) = call_hamt(
            room,
            &format!(
                r#"{{"op":"lookup_root","root_hash":"{root_hash}","nodes":{all_nodes},"keys":[["m.room.member","@alice:example.org"],["m.room.member","@bob:example.org"]]}}"#
            ),
        );
        assert_eq!(status, OK, "{text}");
        let values = JsonValue::parse(&text).unwrap();
        let values = values.get("values").and_then(JsonValue::as_array).unwrap();
        assert_eq!(values[0].as_str(), Some("$member:example.org"));
        assert_eq!(values[1], JsonValue::Null);

        // The root record verifies the whole tree and matches the hashes.
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"verify","root_record":"{root_record}","nodes":{all_nodes}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let verified = JsonValue::parse(&text).unwrap();
        assert_eq!(
            verified.get("root_hash").and_then(JsonValue::as_str),
            Some(root_hash.as_str())
        );
        assert_eq!(
            verified.get("state_group_id").and_then(JsonValue::as_str),
            Some(state_group_id.as_str())
        );
        assert_eq!(
            verified.get("entry_count").and_then(JsonValue::as_u64),
            Some(2)
        );
    }

    #[test]
    fn state_hamt_rejects_a_tampered_node() {
        let room = "!room:example.org";
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"root","persist":true,"entries":{HAMT_ENTRIES}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let persisted = JsonValue::parse(&text).unwrap();
        let root_hash = persisted
            .get("root_hash")
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned();
        let mut nodes = persisted
            .get("nodes")
            .and_then(JsonValue::as_array)
            .unwrap()
            .clone();
        let parts = nodes[0].as_array().unwrap();
        let mut bytes = parts[1].as_str().unwrap().to_owned();
        bytes.replace_range(..2, "4e");
        let hash = parts[0].as_str().unwrap().to_owned();
        nodes[0] = rezzy::json!([hash, bytes]);
        let all_nodes = nodes_argument(&nodes);

        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"entries_root","root_hash":"{root_hash}","nodes":{all_nodes}}}"#),
        );
        assert_eq!(status, ERR, "{text}");
        assert!(
            text.contains("failed verification") || text.contains("Invalid"),
            "{text}"
        );
    }

    #[test]
    fn state_hamt_verify_rejects_root_record_mismatches() {
        let room = "!room:example.org";
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"root","persist":true,"entries":{HAMT_ENTRIES}}}"#),
        );
        assert_eq!(status, OK, "{text}");
        let persisted = JsonValue::parse(&text).unwrap();
        let record = persisted
            .get("root_record")
            .and_then(JsonValue::as_str)
            .unwrap()
            .to_owned();
        let nodes = persisted
            .get("nodes")
            .and_then(JsonValue::as_array)
            .unwrap()
            .clone();
        let all_nodes = nodes_argument(&nodes);

        // Prefix starts at byte 7: magic(4) + version(1) + u16 length(2).
        let mut wrong_prefix = record.clone();
        wrong_prefix.replace_range(14..16, "22");
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"verify","root_record":"{wrong_prefix}","nodes":{all_nodes}}}"#),
        );
        assert_eq!(status, ERR, "{text}");
        assert!(text.contains("prefix"), "{text}");

        // The lattice is the tail of the fixed-width record.
        let mut wrong_lattice = record.clone();
        let tail = record.len() - 2;
        let replacement = if &record[tail..] == "00" { "01" } else { "00" };
        wrong_lattice.replace_range(tail.., replacement);
        let (status, text) = call_hamt(
            room,
            &format!(r#"{{"op":"verify","root_record":"{wrong_lattice}","nodes":{all_nodes}}}"#),
        );
        assert_eq!(status, ERR, "{text}");
        assert!(text.contains("lattice"), "{text}");
    }

    const ID_V3: &str = "$RrGxF28UrHLmoASHndYb9Jb/1SFww2ptmtur9INS438";
    const ID_V12: &str = "$IYh3j5DDlpXmmErAivzq4d1PVT0DxegxvvKtlWVDoFE";
    const ID_V3_OTHER: &str = "$EmwNBlHoSOVmCZ1cM//yv/OvxB6r4OFEIGSJea7+Amk";

    #[test]
    fn recon_digest_and_sketch() {
        let (status, text) = call_recon(&format!(
            r#"{{"op":"digest","event_ids":["{ID_V3}","{ID_V12}"]}}"#
        ));
        assert_eq!(status, OK, "{text}");
        let digest = JsonValue::parse(&text).unwrap();
        assert_eq!(
            digest
                .get("digest")
                .and_then(JsonValue::as_str)
                .unwrap()
                .len(),
            22
        );
        assert_eq!(digest.get("count").and_then(JsonValue::as_u64), Some(2));

        let (status, text) = call_recon(&format!(
            r#"{{"op":"sketch","event_ids":["{ID_V3}","{ID_V12}"],"capacity":4}}"#
        ));
        assert_eq!(status, OK, "{text}");
        let sketch = JsonValue::parse(&text).unwrap();
        assert_eq!(
            sketch
                .get("sketch")
                .and_then(JsonValue::as_str)
                .unwrap()
                .len(),
            4 * 32 * 2
        );
    }

    #[test]
    fn recon_diff_classifies_local_and_remote() {
        let (status, remote_text) = call_recon(&format!(
            r#"{{"op":"sketch","event_ids":["{ID_V12}","{ID_V3_OTHER}"],"capacity":4}}"#
        ));
        assert_eq!(status, OK, "{remote_text}");
        let remote = JsonValue::parse(&remote_text).unwrap();
        let remote_sketch = remote.get("sketch").and_then(JsonValue::as_str).unwrap();

        let (status, text) = call_recon(&format!(
            r#"{{"op":"diff","local_event_ids":["{ID_V3}","{ID_V12}"],"remote_sketch":"{remote_sketch}","capacity":4}}"#
        ));
        assert_eq!(status, OK, "{text}");
        let response = JsonValue::parse(&text).unwrap();
        assert_eq!(response.get("count").and_then(JsonValue::as_u64), Some(2));
        let local_only = response
            .get("local_only")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(local_only.len(), 1);
        assert_eq!(local_only[0].as_str(), Some(ID_V3));

        let remote_only = response
            .get("remote_only")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(remote_only.len(), 1);
        let expected = FixedWindowElementHash::from_matrix_event_id_any(ID_V3_OTHER).unwrap();
        let expected_hex = hex_encode(&expected.digest());
        assert_eq!(remote_only[0].as_str(), Some(expected_hex.as_str()));
    }

    #[test]
    fn new_entry_points_reject_bad_requests() {
        assert_eq!(call_recon(r#"{"op":"nope"}"#).0, ERR);
        assert_eq!(
            call_recon(r#"{"op":"sketch","event_ids":[],"capacity":0}"#).0,
            ERR
        );
        assert_eq!(call_hamt("!room:example.org", r#"{"op":"nope"}"#).0, ERR);
    }

    fn drain(status: i32, out: *mut u8, len: usize) -> (i32, String) {
        let text = if out.is_null() {
            String::new()
        } else {
            // SAFETY: `out`/`len` were just returned by the library.
            let text = unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(out, len)).into_owned()
            };
            // SAFETY: freeing the buffer just returned, once.
            unsafe { rezzy_free(out, len) };
            text
        };
        (status, text)
    }

    fn call_client(client: *mut RezzyClient, request: &str) -> (i32, String) {
        let mut out = ptr::null_mut();
        let mut len = 0;
        // SAFETY: the caller holds a live handle; request bytes are valid for
        // their length; `out` is freed by `drain`.
        let status = unsafe {
            rezzy_client_step(
                client,
                request.as_ptr(),
                request.len(),
                &raw mut out,
                &raw mut len,
            )
        };
        drain(status, out, len)
    }

    fn start_message(remote: &ResidentKernel) -> String {
        let strata = rezzy_recon::resident::encode_strata(remote.strata())
            .into_iter()
            .map(JsonValue::String)
            .collect();
        let request = rezzy::json!({
            "op": "start",
            "digest": remote.accumulator().encode_digest(),
            "known_event_count": remote.accumulator().known_event_count(),
            "strata": JsonValue::Array(strata),
            "frame_matches": true,
            "has_unknown_extremity": false,
        });
        rezzy::json::write_string_value(&request).unwrap()
    }

    #[test]
    fn client_synchronizes_when_the_remote_set_matches() {
        let kernel = rezzy_kernel_new();
        // SAFETY: null is accepted; the handle is released below.
        let client = unsafe { rezzy_client_new(kernel) };
        assert!(!client.is_null());
        let remote = ResidentKernel::new();
        let (status, text) = call_client(client, &start_message(&remote));
        assert_eq!(status, OK, "{text}");
        let response = JsonValue::parse(&text).unwrap();
        assert_eq!(
            response.get("action").and_then(JsonValue::as_str),
            Some("synchronized")
        );
        // SAFETY: both handles were created here and are freed once.
        unsafe {
            rezzy_client_free(client);
            rezzy_kernel_free(kernel);
        }
    }

    #[test]
    fn client_rejects_out_of_order_steps() {
        let kernel = rezzy_kernel_new();
        // SAFETY: null is accepted; the handle is released below.
        let client = unsafe { rezzy_client_new(kernel) };
        assert!(!client.is_null());

        let (status, text) =
            call_client(client, r#"{"op":"advance","roots":[],"failed_buckets":[]}"#);
        assert_eq!(status, ERR, "{text}");
        assert!(text.contains("not waiting"), "{text}");

        let remote = ResidentKernel::new();
        assert_eq!(call_client(client, &start_message(&remote)).0, OK);
        let (status, text) = call_client(client, &start_message(&remote));
        assert_eq!(status, ERR, "{text}");
        assert!(text.contains("already started"), "{text}");

        // SAFETY: both handles were created here and are freed once.
        unsafe {
            rezzy_client_free(client);
            rezzy_kernel_free(kernel);
        }
    }

    #[test]
    fn client_rejects_malformed_start_and_null_arguments() {
        let kernel = rezzy_kernel_new();
        // SAFETY: null is accepted; the handle is released below.
        let client = unsafe { rezzy_client_new(kernel) };
        assert!(!client.is_null());
        let remote = ResidentKernel::new();
        let request = std::format!(
            r#"{{"op":"start","digest":"{}","known_event_count":0,"strata":["!!!not-strata"],"frame_matches":true,"has_unknown_extremity":false}}"#,
            remote.accumulator().encode_digest()
        );
        assert_eq!(call_client(client, &request).0, ERR);
        assert_eq!(
            call_client(ptr::null_mut(), &start_message(&remote)).0,
            BAD_ARG
        );
        // SAFETY: null is accepted by both constructors.
        unsafe {
            assert!(rezzy_client_new(ptr::null()).is_null());
            rezzy_client_free(client);
            rezzy_kernel_free(kernel);
        }
    }
}

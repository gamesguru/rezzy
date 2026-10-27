//! C ABI over rezzy's event hashing, for callers that cannot link Rust
//! (cgo, Python `ctypes`). See `include/rezzy.h` for the contract.

#![allow(unsafe_code)]
//!
//! Every function returns a status code and hands results back through
//! `out`/`out_len`. Buffers are allocated here and released with
//! [`rezzy_free`]; callers never free them with their own allocator.

use rezzy::JsonValue;
use std::ffi::{c_char, CStr};
use std::ptr;

const OK: i32 = 0;
const ERR: i32 = 1;
const BAD_ARG: i32 = 2;

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
    if !out.is_null() {
        // SAFETY: non-null `out` is required to point to writable storage.
        unsafe { *out = ptr::null_mut() };
    }
    if !out_len.is_null() {
        // SAFETY: non-null `out_len` is required to point to writable storage.
        unsafe { *out_len = 0 };
    }
    if room_version.is_null() || input.is_null() || out.is_null() || out_len.is_null() {
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
    match rezzy::json::write_string_value(&report) {
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
}

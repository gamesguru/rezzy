/* Native C ABI for rezzy. Keep in sync with src/lib.rs. */
#ifndef REZZY_H
#define REZZY_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Returns the ABI version declared by the REZZY_ABI_VERSION enum value. */
uint32_t rezzy_abi_version(void);

/* Status codes. */
enum {
  REZZY_ABI_VERSION = 1,
  REZZY_OK = 0,
  REZZY_ERR = 1,     /* operation failed; *out holds a UTF-8 message */
  REZZY_BAD_ARG = 2, /* null pointer or non-UTF-8 room version; *out is NULL */
};

/*
 * On REZZY_OK, writes the event ID ("$" + unpadded base64 reference hash) of
 * the JSON event to *out / *out_len. A top-level "event_id" field is ignored,
 * as it is derived data. On REZZY_ERR, *out holds an error message instead.
 * Free any non-NULL *out with rezzy_free. Output is not NUL-terminated.
 */
int rezzy_event_id(const char *room_version, const uint8_t *event_json,
                   size_t event_len, uint8_t **out, size_t *out_len);

/*
 * REZZY_OK when the event's hashes.sha256 matches its content under the room
 * version; REZZY_ERR (with a message in *out) when it does not.
 */
int rezzy_verify_content_hash(const char *room_version,
                              const uint8_t *event_json, size_t event_len,
                              uint8_t **out, size_t *out_len);

/*
 * Describes every event in a JSONL buffer. On REZZY_OK, *out holds the same
 * JSON report object as `rezzy hash events`, including event records with
 * claimed_event_id, content_hash_ok, links, and a 1-based "line". event_id
 * is present only for room v3 and later (v1/v2 IDs cannot be derived).
 * A line that cannot be hashed yields an {"error": ...} record and the call
 * still succeeds. REZZY_ERR (message in *out) means no events or invalid UTF-8.
 */
int rezzy_hash_events(const char *room_version, const uint8_t *jsonl,
                      size_t jsonl_len, uint8_t **out, size_t *out_len);

/*
 * Builds an MSC4521 GF(2^256) sketch from contiguous 32-byte digest values.
 * `elements_len` must equal `element_count * 32`; `capacity` is 1..256.
 * On success, *out is the serialized sketch (capacity * 32 bytes).
 */
int rezzy_recon_gf256_sketch(const uint8_t *elements, size_t elements_len,
                             size_t element_count, size_t capacity,
                             uint8_t **out, size_t *out_len);

/*
 * Decodes the symmetric difference of two serialized GF(2^256) sketches.
 * Each sketch must be exactly `capacity * 32` bytes. On success, *out is
 * zero or more contiguous 32-byte digest values, limited by `max_elements`.
 */
int rezzy_recon_gf256_difference(const uint8_t *left, size_t left_len,
                                 const uint8_t *right, size_t right_len,
                                 size_t capacity, size_t max_elements,
                                 uint8_t **out, size_t *out_len);

/*
 * HAMT and reconciliation entry points. Both take one UTF-8 JSON request and
 * return one UTF-8 JSON response, so new operations do not need new symbols.
 * On REZZY_ERR, *out holds an error message. Free any non-NULL *out with
 * rezzy_free. Output is not NUL-terminated.
 *
 * rezzy_state_hamt(room_id, request):
 *   {"op":"root","entries":[["event_type","state_key","event_id"], ...]}
 *     -> {"root_hash":"<hex>","state_group_id":"<hex>"}
 *   {"op":"lookup","entries":[...],"event_type":"...","state_key":"..."}
 *     -> {"value":"<event_id>"} or {"value":null}
 *   {"op":"diff","a":[...],"b":[...]}
 *     -> {"added":[[type,key,id],...],"removed":[...]}
 *   Each entries array must contain at most one row per (event_type,
 *   state_key); repeated keys cannot be separated and fail the build.
 *
 * rezzy_recon(request):
 *   {"op":"digest","event_ids":["$...", ...]}
 *     -> {"digest":"<base64url>","count":N}
 *     Pass each event ID once: digest is a 128-bit XOR, so a duplicate
 *     cancels itself while `count` still increases.
 *   {"op":"sketch","event_ids":[...],"capacity":N}
 *     -> {"sketch":"<hex>","capacity":N}
 *   {"op":"diff","local_event_ids":[...],"remote_sketch":"<hex>",
 *    "capacity":N}
 *     -> {"local_only":["$...",...],"remote_only":["<hex digest>",...],
 *         "count":N}
 */
int rezzy_state_hamt(const char *room_id, const uint8_t *request_json,
                     size_t request_len, uint8_t **out, size_t *out_len);

int rezzy_recon(const uint8_t *request_json, size_t request_len, uint8_t **out,
                size_t *out_len);

/*
 * Releases a buffer returned through *out. Pass the exact out_len that came
 * back with it, and free each buffer once; a wrong length or a second free is
 * undefined behaviour. Accepts NULL.
 */
void rezzy_free(uint8_t *ptr, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* REZZY_H */

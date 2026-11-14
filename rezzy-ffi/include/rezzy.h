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
 * Running reconciliation state for one room (MSC4521 "algebraic_v1": the
 * digest and strata that other servers exchange). A kernel is updated as
 * events are accepted instead of being rebuilt from every event ID, and is
 * persisted with rezzy_kernel_to_bytes / rezzy_kernel_from_bytes.
 *
 * A handle is not thread-safe: use one handle from one thread at a time.
 * Free each handle once with rezzy_kernel_free (NULL is accepted).
 *
 * The kernel is an XOR accumulator. Insert each event once: inserting the
 * same ID twice cancels it out, and removing an ID that was never inserted
 * corrupts the digest. Removing from an empty kernel returns REZZY_ERR.
 */
typedef struct RezzyKernel RezzyKernel;

RezzyKernel *rezzy_kernel_new(void);
void rezzy_kernel_free(RezzyKernel *kernel);

/* REZZY_OK, REZZY_ERR (message in *out) for an invalid ID or a limit error,
 * or REZZY_BAD_ARG for a NULL argument. */
int rezzy_kernel_insert(RezzyKernel *kernel, const uint8_t *event_id,
                        size_t event_id_len, uint8_t **out, size_t *out_len);
int rezzy_kernel_remove(RezzyKernel *kernel, const uint8_t *event_id,
                        size_t event_id_len, uint8_t **out, size_t *out_len);

/* On REZZY_OK, *out is the fixed-size persisted record. */
int rezzy_kernel_to_bytes(const RezzyKernel *kernel, uint8_t **out,
                          size_t *out_len);

/* On REZZY_OK, *out is {"digest":"<base64url>","count":N}, the same shape
 * rezzy_recon returns for "digest". */
int rezzy_kernel_digest(const RezzyKernel *kernel, uint8_t **out,
                        size_t *out_len);

/* On REZZY_OK, *out is {"strata":["<base64url>", ...]} with 32 entries,
 * suitable for the room_digest/strata response. */
int rezzy_kernel_strata(const RezzyKernel *kernel, uint8_t **out,
                        size_t *out_len);

/* On REZZY_OK, *kernel is a new handle. On REZZY_ERR, *kernel is NULL and
 * *out holds a message (wrong length, unknown version, inconsistent record). */
int rezzy_kernel_from_bytes(const uint8_t *bytes, size_t bytes_len,
                            RezzyKernel **kernel, uint8_t **out,
                            size_t *out_len);

/*
 * Requester-side reconciliation policy. Create it from a snapshot of a kernel;
 * then call rezzy_client_step with JSON messages and use the returned action
 * to perform host-side federation I/O. Handles are not thread-safe.
 *
 * First step:
 *   {"op":"start","digest":"<base64url>","known_event_count":N,
 *    "strata":["<base64url>",...],"frame_matches":true,
 *    "has_unknown_extremity":false,"concurrency_headroom":N}
 * Next steps:
 *   {"op":"advance","roots":[...],"failed_buckets":[[depth,prefix],...]}
 * `roots` contains decoded local-missing short IDs; failed buckets are the
 * requested buckets whose response could not be decoded. All remaining
 * requested buckets are treated as successful. Responses are JSON actions
 * (`synchronized`, `extremity_diff`, `bucket_sketches`, `resolve_roots`, or
 * `round_limit`). The caller performs network requests and feeds each result
 * back into the same handle.
 */
typedef struct RezzyClient RezzyClient;

/* Returns NULL for a NULL kernel; release the result with rezzy_client_free. */
RezzyClient *rezzy_client_new(const RezzyKernel *kernel);
void rezzy_client_free(RezzyClient *client);
int rezzy_client_step(RezzyClient *client, const uint8_t *request_json,
                      size_t request_len, uint8_t **out, size_t *out_len);

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

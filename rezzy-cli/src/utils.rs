// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::error::{AppError, ErrorCode};
use crate::network::fetch_room_state;
use crate::Args;
use rezzy::basespec::event_types::{
    EventType, FIELD_CONTENT, FIELD_EVENT_ID, FIELD_ROOM_VERSION, FIELD_TYPE, FIELD_USERS,
    FIELD_USERS_DEFAULT, M_ROOM_CREATE, M_ROOM_JOIN_RULES, M_ROOM_MEMBER, M_ROOM_POWER_LEVELS,
};
use rezzy::{LeanEvent, SharedState, StateResVersion};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::time::Instant;

pub use rezzy::{
    discover_array_spans, discover_envelope_spans, discover_federation_spans, discover_jsonl_spans,
    extract_matrix_event_into, EnvelopeSpans, FederationSpans, MatrixEventScratch, MatrixEventView,
    RawEventSpan, ADJACENCY_MASK,
};

/// A resolved state shared between events without copying.
pub type SharedStateMap = std::sync::Arc<ResolvedState>;

/// Parse a room version string.
///
/// # Errors
///
/// Returns an error when the room version is unsupported.
pub fn parse_room_version(ver: &str) -> Result<StateResVersion, AppError> {
    StateResVersion::from_room_version(ver).ok_or_else(|| {
        err!(
            ErrorCode::UnsupportedVersion,
            "Unsupported room version: {ver}"
        )
    })
}

/// Detect the room version from a state map.
///
/// # Errors
///
/// Returns an error when no create event is present or its room version is
/// unsupported.
pub fn detect_version(
    events: &[rezzy::JsonValue],
    debug: bool,
) -> Result<StateResVersion, AppError> {
    let mut saw_create_event = false;
    for ev in events {
        if ev.get(FIELD_TYPE).and_then(|t| t.as_str()) == Some(M_ROOM_CREATE) {
            saw_create_event = true;
            if let Some(raw) = ev
                .get(FIELD_CONTENT)
                .and_then(|c| c.get(FIELD_ROOM_VERSION))
            {
                // Only an absent field defaults to v1; a present non-string is malformed.
                let Some(ver) = raw.as_str() else {
                    bail_code!(
                        ErrorCode::UnsupportedVersion,
                        "m.room.create content.room_version must be a string"
                    );
                };
                if debug {
                    eprintln!("[DEBUG] Found m.room.create with version: {ver}");
                }
                return parse_room_version(ver);
            }
        }
    }

    if saw_create_event {
        // Per the spec, a create event without `content.room_version` is room version 1.
        if debug {
            eprintln!("[DEBUG] m.room.create has no room_version; defaulting to 1");
        }
        return parse_room_version("1");
    }

    bail_code!(
        ErrorCode::NoCreateEvent,
        "No m.room.create event found — cannot detect room version. \
         Use --state-res to specify the algorithm manually."
    )
}

/// Detect the literal `content.room_version` string from `m.room.create`.
///
/// Distinct from [`detect_version`]: that returns the coarser
/// [`StateResVersion`] (which state-resolution algorithm to run), while
/// [`LeanEvent::validate_syntactic`](rezzy::LeanEvent::validate_syntactic)
/// needs the exact version string for its version-string-sensitive checks
/// (e.g. the pre-v11 255-byte field limit). Returns `None` if no
/// `m.room.create` event is present or its `content.room_version` is absent
/// -- callers should apply the spec's "missing `room_version` defaults to 1"
/// rule themselves.
#[must_use]
pub fn detect_room_version_string(events: &[rezzy::JsonValue]) -> Option<String> {
    events.iter().find_map(|ev| {
        if ev.get(FIELD_TYPE).and_then(|t| t.as_str()) != Some(M_ROOM_CREATE) {
            return None;
        }
        ev.get(FIELD_CONTENT)
            .and_then(|c| c.get(FIELD_ROOM_VERSION))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    })
}

fn sort_gaps_by_depth<T, S: std::hash::BuildHasher>(
    gaps: &mut [T],
    events_map: &HashMap<String, LeanEvent, S>,
    event_id: impl Fn(&T) -> &str,
) {
    gaps.sort_by(
        |a, b| match (events_map.get(event_id(a)), events_map.get(event_id(b))) {
            (Some(a_event), Some(b_event)) => a_event.cmp_by_depth(b_event),
            _ => event_id(a).cmp(event_id(b)),
        },
    );
}

/// Detect `prev_events` and `auth_events` references that point to events
/// absent from `events_map` and not known to the `exists` oracle.
///
/// This is the "warn surface" for DAG gaps. A reference to an unknown event is
/// *not* necessarily a resolution failure — a missing `prev_event` is a backward
/// extremity (incomplete timeline / backfill needed), while a missing
/// `auth_event` means the event's authorization cannot be verified (potentially
/// unsafe state). The two cases are reported separately so callers can treat
/// them differently.
///
/// `exists` is the seam where a caller can plug in a richer notion of "known":
/// e.g. a query against a homeserver's event store, a fetch attempt, or a
/// compact accumulator (Bloom filter / 128-bit digest) of the known event set.
/// A `|_| false` oracle means "known iff present in `events_map`".
///
/// Returns `(backward_extremities, missing_auth_events)`.
pub fn report_gaps<F, S: std::hash::BuildHasher>(
    events_map: &HashMap<String, LeanEvent, S>,
    exists: F,
) -> (
    Vec<rezzy::state::BackwardExtremity<String>>,
    Vec<rezzy::state::MissingAuthEvent<String>>,
)
where
    F: Fn(&String) -> bool,
{
    let mut backward = rezzy::find_backward_extremities(events_map, &exists);
    let mut missing_auth = rezzy::find_missing_auth_events(events_map, &exists);
    sort_gaps_by_depth(&mut backward, events_map, |gap| gap.event_id.as_str());
    for gap in &mut backward {
        gap.missing_prev_events.sort();
    }
    sort_gaps_by_depth(&mut missing_auth, events_map, |gap| gap.event_id.as_str());
    for gap in &mut missing_auth {
        gap.missing_auth_events.sort();
    }
    (backward, missing_auth)
}

/// The number of gap events and a bounded, shallow-first sample of them.
///
/// [`report_gaps`] materializes one record per gap, which is the right shape
/// for callers that consume every gap. The CLI only prints a handful, so it
/// uses this instead: a full scan with no per-gap allocation beyond the sample.
pub struct GapSummary {
    /// How many events have at least one missing reference.
    pub count: usize,
    /// At most the requested number of `(event_id, missing_ids)` pairs,
    /// ordered by the referencing event's depth then id.
    pub samples: Vec<(String, Vec<String>)>,
}

fn collect_gap_samples<S, F>(
    events_map: &HashMap<String, LeanEvent, S>,
    references: F,
    limit: usize,
) -> GapSummary
where
    S: std::hash::BuildHasher,
    F: Fn(&LeanEvent) -> &[String],
{
    let mut count = 0_usize;
    let mut samples: Vec<(u64, String, Vec<String>)> = Vec::with_capacity(limit);
    for event in events_map.values() {
        let mut missing: Vec<String> = Vec::new();
        for id in references(event) {
            if !events_map.contains_key(id) {
                missing.push(id.clone());
            }
        }
        if missing.is_empty() {
            continue;
        }
        count = count.saturating_add(1);
        let rank = (event.depth, event.event_id.as_str());
        if samples.len() < limit {
            samples.push((event.depth, event.event_id.clone(), missing));
            continue;
        }
        // Keep the `limit` shallowest events. The sample is small, so a linear
        // scan for the current maximum is cheaper than a heap.
        if let Some((worst, _)) = samples
            .iter()
            .enumerate()
            .max_by_key(|(_, (depth, id, _))| (*depth, id.as_str()))
        {
            let worst_rank = (samples[worst].0, samples[worst].1.as_str());
            if rank < worst_rank {
                samples[worst] = (event.depth, event.event_id.clone(), missing);
            }
        }
    }
    samples.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));
    for (_, _, missing) in &mut samples {
        missing.sort();
    }
    GapSummary {
        count,
        samples: samples
            .into_iter()
            .map(|(_, event_id, missing)| (event_id, missing))
            .collect(),
    }
}

/// Summarize missing `prev_events` and `auth_events` without materializing one
/// record per gap. `limit` bounds the printed sample.
#[must_use]
pub fn summarize_gaps<S: std::hash::BuildHasher>(
    events_map: &HashMap<String, LeanEvent, S>,
    limit: usize,
) -> (GapSummary, GapSummary) {
    (
        collect_gap_samples(events_map, |event| &event.prev_events, limit),
        collect_gap_samples(events_map, |event| &event.auth_events, limit),
    )
}

/// Computes an FNV-1a hash of `StateEntries`.
#[must_use]
pub fn compute_state_hash(state: &SharedState<String, String>) -> String {
    let mut hash: u64 = 14_695_981_039_346_656_037; // FNV offset basis
    for ((event_type, state_key), event_id) in state {
        for &byte in event_type.as_str().as_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211); // FNV prime
        }
        hash ^= 0x00;
        hash = hash.wrapping_mul(1_099_511_628_211);
        for &byte in state_key.as_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
        hash ^= 0x00;
        hash = hash.wrapping_mul(1_099_511_628_211);
        for &byte in event_id.as_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    format!("{hash:016x}")
}

/// Load a JSON file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or its contents are invalid
/// JSON (including an empty JSONL file).
pub fn load_file(input_path: &PathBuf) -> Result<Vec<rezzy::JsonValue>, AppError> {
    let input_reader: Box<dyn Read> = if input_path.to_str() == Some("-") {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(input_path)?)
    };

    let is_jsonl = input_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));

    let mut reader = BufReader::new(input_reader);

    if is_jsonl {
        // Read the whole stream once and split on newlines. `BufRead::lines`
        // allocates a fresh `String` per event; a large export has enough
        // lines that the parser should get byte slices directly.
        let mut buffer = Vec::new();
        let mut values = Vec::new();
        loop {
            buffer.clear();
            if reader.read_until(b'\n', &mut buffer)? == 0 {
                break;
            }
            let raw_line = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            values.push(rezzy::JsonValue::parse_bytes(line)?);
        }
        if values.is_empty() {
            bail_code!(
                ErrorCode::EmptyInput,
                "No input data provided in JSONL file."
            );
        }
        Ok(values)
    } else {
        let mut input_data = Vec::new();
        loop {
            let mut line = String::new();
            let bytes_read = reader.read_line(&mut line)?;
            if bytes_read == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            input_data.extend_from_slice(line.as_bytes());
        }
        if input_data.is_empty() {
            bail_code!(
                ErrorCode::EmptyInput,
                "No input data provided before empty line or EOF."
            );
        }
        let val = rezzy::JsonValue::parse_bytes(&input_data)?;
        match val {
            rezzy::JsonValue::Array(arr) => Ok(arr),
            other => Ok(vec![other]),
        }
    }
}

/// Load or fetch the input value from args.
///
/// # Errors
///
/// Returns an error if input cannot be read or the network request fails.
pub fn load_or_fetch_input_value(args: &Args) -> Result<rezzy::JsonValue, AppError> {
    if let Some(room_id) = &args.room {
        let homeserver = args.homeserver.as_deref().ok_or_else(|| {
            err!(
                ErrorCode::MissingHomeserver,
                "--homeserver is required when using --room"
            )
        })?;

        let token = args.token.clone().or_else(|| {
            let env_key = format!(
                "MTOKEN_{}",
                homeserver
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_uppercase()
                    .replace(['.', '-'], "_")
            );
            std::env::var(&env_key).ok()
        });
        fetch_room_state(homeserver, room_id, token.as_deref())
            .map_err(|e| err!(ErrorCode::NetworkError, "{e}"))
    } else if !args.input.is_empty() {
        if args.input.len() == 1 {
            let input_path = &args.input[0];
            let is_jsonl = input_path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
            if is_jsonl {
                let events = load_file(input_path)?;
                Ok(rezzy::JsonValue::Array(events))
            } else {
                let content = std::fs::read(input_path)?;
                let val = rezzy::JsonValue::parse_bytes(&content)?;
                Ok(val)
            }
        } else {
            let mut file_sets = Vec::with_capacity(args.input.len());
            for path in &args.input {
                let label = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().to_string(),
                );
                let t = Instant::now();
                let events = load_file(path)?;
                if args.debug {
                    eprintln!(
                        "[DEBUG] loaded {label}: {} events in {:.2?}",
                        events.len(),
                        t.elapsed()
                    );
                }
                file_sets.push((label, events));
            }
            let merged = crate::jsonl_merge::merge_event_sets(&file_sets, args.debug, args.quiet)?;
            if args.debug {
                eprintln!(
                    "[DEBUG] packaging {} merged events into input value...",
                    merged.len()
                );
            }
            let t = Instant::now();
            let out = rezzy::JsonValue::Array(merged);
            if args.debug {
                eprintln!("[DEBUG] packaged merged events in {:.2?}", t.elapsed());
            }
            Ok(out)
        }
    } else {
        bail_code!(
            ErrorCode::MissingInputFlag,
            "Either --input or --room must be provided. Use -h or --help for more info."
        );
    }
}

/// Parse input and extract the state heads.
///
/// Consumes `input_val`: the caller owns it and, for inputs with tens of
/// thousands of events, cloning the array only to drop the original doubles
/// both peak memory and startup time.
///
/// # Errors
///
/// Returns an error when the input structure is unsupported, the `events`
/// field is not an array, or a head is not a string.
pub fn parse_and_extract_heads(
    input_val: rezzy::JsonValue,
    debug: bool,
) -> Result<(Vec<rezzy::JsonValue>, Vec<String>), AppError> {
    match input_val {
        rezzy::JsonValue::Object(mut obj) => match obj.remove("events") {
            Some(rezzy::JsonValue::Array(evs)) => {
                let mut hds = Vec::new();
                if let Some(hds_arr) = obj.get("heads").and_then(|h| h.as_array()) {
                    for v in hds_arr {
                        hds.push(
                            v.as_str()
                                .ok_or_else(|| {
                                    err!(ErrorCode::InvalidHeadType, "each 'head' must be a string")
                                })?
                                .to_string(),
                        );
                    }
                }
                Ok((evs, hds))
            }
            Some(_) => bail_code!(
                ErrorCode::EventsNotArray,
                "'events' field must be a JSON array"
            ),
            None if obj.contains_key(FIELD_EVENT_ID) || obj.contains_key(FIELD_TYPE) => {
                Ok((vec![rezzy::JsonValue::Object(obj)], Vec::new()))
            }
            None => bail_code!(
                ErrorCode::UnrecognisedStructure,
                "Unrecognized JSON object structure. Top-level object must either contain 'events' or represent a single event with 'event_id' or 'type'."
            ),
        },
        rezzy::JsonValue::Array(evs) => {
            if debug {
                eprintln!("[DEBUG] took ownership of {} top-level events", evs.len());
            }
            Ok((evs, Vec::new()))
        }
        _ => bail_code!(
            ErrorCode::UnexpectedFormat,
            "Unexpected JSON format: expected object or array"
        ),
    }
}

/// Upper bound on the dense head-bitset matrix in
/// [`partition_state_maps_fast`]; larger graphs use the per-head pipeline.
const MAX_HEAD_BITS_BYTES: usize = 64 * 1024 * 1024;

/// Partition the per-head state maps directly from the DAG.
///
/// This is equivalent to building one full state map per head and passing them
/// to [`rezzy::partition_state_maps`], but it never materializes those maps.
/// Each event carries the bitset of heads it is an ancestor of; walking the
/// state events by descending `(depth, event_id)` assigns the winning event
/// for every key/head pair. The cost is proportional to
/// `edges × heads/64 + state_events × heads/64`, not `heads × events`.
///
/// # Panics
///
/// Panics only if an event index built from `events_map` is missing from that
/// same map, which cannot happen for a consistent snapshot.
#[allow(clippy::arithmetic_side_effects, clippy::too_many_lines)]
#[must_use]
pub fn partition_state_maps_fast<S1: std::hash::BuildHasher>(
    heads: &[String],
    events_map: &HashMap<String, LeanEvent, S1>,
    version: StateResVersion,
    debug: bool,
) -> (ResolvedState, Vec<String>) {
    let start = Instant::now();
    let n = events_map.len();
    let head_count = heads.len();
    if head_count == 0 || n == 0 {
        return (ResolvedState::new(), Vec::new());
    }

    let words = head_count.div_ceil(64);

    let mut ids: Vec<&str> = Vec::with_capacity(n);
    let mut parents: Vec<&[String]> = Vec::with_capacity(n);
    let mut timestamps: Vec<u64> = Vec::with_capacity(n);
    let mut depths: Vec<u64> = Vec::with_capacity(n);
    for event in events_map.values() {
        ids.push(event.event_id.as_str());
        parents.push(event.prev_events.as_slice());
        timestamps.push(event.origin_server_ts);
        depths.push(event.depth);
    }

    let order = crate::timeline_order::kahn_order_borrowed(&ids, &parents, &timestamps, &depths);
    // The fast reverse pass is valid only when one global event ordering also
    // preserves each head's resolved state. Multi-parent merges and cycles can
    // violate that assumption, so use the state-at-event pipeline for those
    // graphs. This keeps the optimized path for simple acyclic chains/forks.
    //
    // The bitset pass is O(events × heads). Many independent events (roughly
    // one head each) would otherwise allocate a dense `n × words` array that
    // can reach gigabytes even when almost no event carries state, so bound it
    // and let the (sparse) state-at-event pipeline handle that case.
    let head_bits_bytes = n
        .saturating_mul(words)
        .saturating_mul(core::mem::size_of::<u64>());
    if order.len() != n
        || parents.iter().any(|event_parents| event_parents.len() > 1)
        || head_bits_bytes > MAX_HEAD_BITS_BYTES
    {
        let states: Vec<_> = heads
            .iter()
            .filter_map(|head| rezzy::compute_state_at(head, events_map, version, &String::new()))
            .collect();
        let (unconflicted, conflicted) =
            rezzy::partition_state_maps(states.iter().map(|state| state.iter()), states.len());
        if debug {
            eprintln!(
                "[DEBUG] Used state-at-event partitioning for a cyclic, multi-parent, or very large graph"
            );
        }
        return (unconflicted, conflicted);
    }
    let mut index_of: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (index, id) in ids.iter().enumerate() {
        index_of.insert(*id, index);
    }

    // Child adjacency, for propagating head bitsets from children to parents.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (child, event_parents) in parents.iter().enumerate() {
        for parent in *event_parents {
            if let Some(&parent_index) = index_of.get(parent.as_str()) {
                children[parent_index].push(child);
            }
        }
    }

    // head_bits[event] = the heads that are descendants of `event`.
    let mut head_bits = vec![0_u64; n.saturating_mul(words)];
    for (head_index, head) in heads.iter().enumerate() {
        if let Some(&event_index) = index_of.get(head.as_str()) {
            head_bits[event_index
                .saturating_mul(words)
                .saturating_add(head_index / 64)] |= 1_u64 << (head_index % 64);
        }
    }

    for &event_index in order.iter().rev() {
        let base = event_index.saturating_mul(words);
        for &child in &children[event_index] {
            let child_base = child.saturating_mul(words);
            for word in 0..words {
                let bits = head_bits[child_base.saturating_add(word)];
                head_bits[base.saturating_add(word)] |= bits;
            }
        }
    }

    // Group state events by logical key. Descending rank gives each head its
    // winning event for the key: the same last-write-wins order the per-head
    // depth sort produced.
    let mut by_key: HashMap<(EventType, String), Vec<usize>> = HashMap::new();
    for (index, id) in ids.iter().enumerate() {
        let event = events_map.get(*id).expect("ids came from events_map");
        if let Some(state_key) = event.accepted_state_key() {
            let key = (EventType::from(event.event_type.clone()), state_key.clone());
            by_key.entry(key).or_default().push(index);
        }
    }

    for event_indices in by_key.values_mut() {
        event_indices.sort_unstable_by(|&a, &b| {
            let event_a = events_map.get(ids[a]).expect("ids came from events_map");
            let event_b = events_map.get(ids[b]).expect("ids came from events_map");
            (event_b.depth, event_b.event_id.as_str())
                .cmp(&(event_a.depth, event_a.event_id.as_str()))
        });
    }

    let key_count = by_key.len();
    let valid_bits = head_count % 64;
    let last_mask = if valid_bits == 0 {
        u64::MAX
    } else {
        u64::MAX >> (64 - valid_bits)
    };
    let mut unconflicted_state = ResolvedState::new();
    let mut conflicted_ids = Vec::new();

    for (key, event_indices) in by_key {
        let mut remaining = vec![u64::MAX; words];
        if let Some(last) = remaining.last_mut() {
            *last = last_mask;
        }

        let mut assigned = 0_usize;
        // Each event appears once per key, so winners are distinct.
        let mut winners: Vec<&str> = Vec::new();
        for event_index in event_indices {
            let base = event_index.saturating_mul(words);
            let mut here_count = 0_usize;
            for word in 0..words {
                let here = head_bits[base.saturating_add(word)] & remaining[word];
                if here != 0 {
                    here_count = here_count.saturating_add(here.count_ones() as usize);
                    remaining[word] &= !here;
                }
            }

            if here_count > 0 {
                assigned = assigned.saturating_add(here_count);
                winners.push(ids[event_index]);
            }

            if assigned == head_count {
                break;
            }
        }

        let absent = head_count.saturating_sub(assigned);
        if winners.len() == 1 && absent == 0 {
            unconflicted_state.insert(key, winners[0].to_owned());
        } else {
            conflicted_ids.extend(winners.into_iter().map(str::to_owned));
        }
    }

    if debug {
        eprintln!(
            "[DEBUG] partitioned {key_count} state keys over {head_count} heads in {:.2?}",
            start.elapsed()
        );
    }

    (unconflicted_state, conflicted_ids)
}

/// Resolved state: `(type, state_key)` to event ID.
pub type ResolvedState = SharedState<String, String>;

/// Resolve parent states for a set of events.
#[must_use]
pub fn resolve_parent_states<S: std::hash::BuildHasher>(
    parent_states: &[SharedStateMap],
    events_map: &HashMap<String, LeanEvent, S>,
    version: StateResVersion,
    reachability: &rezzy::resolve::reachability::RangePrefilterReachability<String>,
    caches: &mut rezzy::ForkResolveCaches<String, rezzy::JsonValue>,
) -> SharedStateMap {
    // Fast path: all parent states are identical (Arc::ptr_eq or value equality).
    // Common in linear DAGs where every parent shares the same resolved state.
    if parent_states.len() > 1 {
        let first = &parent_states[0];
        let all_identical = parent_states[1..]
            .iter()
            .all(|s| std::sync::Arc::ptr_eq(s, first) || s.as_ref() == first.as_ref());
        if all_identical {
            return first.clone();
        }
    }

    // Pass the room event map by reference: no per-fork context clone. The
    // shared reachability index restricts V2.1 forward reachability, and the
    // full map is a superset of the former per-fork auth closure, which the
    // full-context reference already proved resolves identically.
    let bare_maps: Vec<ResolvedState> = parent_states
        .iter()
        .map(|arc| arc.as_ref().clone())
        .collect();
    let resolved =
        rezzy::resolve_state_maps_cached(&bare_maps, events_map, version, reachability, caches);
    std::sync::Arc::new(resolved)
}

/// Partition and resolve state across components.
/// Returns the resolved state and elapsed partitioning/resolution time.
/// After V1, supplements conflicts with the auth-chain difference of `heads`;
/// heads absent from `auth_graph` and events absent from `events_map` are skipped.
///
/// # Panics
///
/// Panics if an auth-chain bitmap contains an index absent from its own
/// graph index, or if `version` is [`StateResVersion::V3`].
#[must_use]
pub fn partition_and_resolve_state<S1: std::hash::BuildHasher>(
    heads: &[String],
    events_map: &HashMap<String, LeanEvent, S1>,
    state_maps: &[ResolvedState],
    version: StateResVersion,
    auth_graph: &rezzy::auth::roaring::AuthGraph,
) -> (ResolvedState, std::time::Duration) {
    let start = Instant::now();
    let (unconflicted_state, conflicted_state_set) =
        rezzy::partition_state_maps(state_maps, state_maps.len());

    resolve_partitioned_state(
        heads,
        events_map,
        version,
        auth_graph,
        &unconflicted_state,
        &conflicted_state_set,
        start,
    )
}

/// Resolve a pre-partitioned unconflicted state plus its conflicted event set.
///
/// `start` is the start of the partition phase, so the returned duration covers
/// the whole partition-and-resolve operation.
///
/// # Panics
///
/// Panics if an auth-chain bitmap references an index absent from its own
/// graph index.
pub fn resolve_partitioned_state<S1: std::hash::BuildHasher>(
    heads: &[String],
    events_map: &HashMap<String, LeanEvent, S1>,
    version: StateResVersion,
    auth_graph: &rezzy::auth::roaring::AuthGraph,
    unconflicted_state: &ResolvedState,
    conflicted_state_set: &[String],
    start: Instant,
) -> (ResolvedState, std::time::Duration) {
    let mut auth_difference = std::collections::HashSet::new();
    if version != StateResVersion::V1 && !heads.is_empty() {
        let mut union = rezzy::bitmap::Bitmap::new();
        let mut intersection = rezzy::bitmap::Bitmap::new();
        let mut first = true;

        for head_id in heads {
            if let Some(idx) = auth_graph.index.index_of(head_id) {
                let chain_bitmap = &auth_graph.auth_bitmaps[idx as usize];
                if first {
                    union.clone_from(chain_bitmap);
                    intersection.clone_from(chain_bitmap);
                    first = false;
                } else {
                    union |= chain_bitmap;
                    intersection &= chain_bitmap;
                }
            }
        }

        let diff = <rezzy::bitmap::Bitmap as std::ops::Sub<&rezzy::bitmap::Bitmap>>::sub(
            union,
            &intersection,
        );
        for idx in diff {
            auth_difference.insert(
                auth_graph
                    .index
                    .item_at(idx as usize)
                    .cloned()
                    .expect("auth-chain index came from this graph"),
            );
        }
    }

    // V2.1 computes a subgraph that already contains every conflicted event
    // (and its auth closure). Start from it so each event is cloned once,
    // instead of cloning the conflicted set and then cloning it again when the
    // subgraph is merged in.
    let mut conflicted_events =
        if version == StateResVersion::V2_1 || version == StateResVersion::V2_1_1 {
            rezzy::compute_v2_1_conflicted_subgraph(events_map, conflicted_state_set)
        } else {
            HashMap::new()
        };
    for id in conflicted_state_set {
        if conflicted_events.contains_key(id) {
            continue;
        }
        if let Some(ev) = events_map.get(id) {
            conflicted_events.insert(id.clone(), ev.clone());
        }
    }

    for id in &auth_difference {
        if let Some(ev) = events_map.get(id) {
            conflicted_events.insert(id.clone(), ev.clone());
        }
    }

    let mut pl_cache = HashMap::new();
    let final_state_map = rezzy::resolve_iterative_sort(rezzy::IterativeInputs::new(
        unconflicted_state,
        &conflicted_events,
        events_map,
        version,
        &mut pl_cache,
        &String::new(),
    ));

    let duration = start.elapsed();
    (final_state_map, duration)
}

/// Apply global power levels to the state.
pub fn apply_global_power_levels<S: std::hash::BuildHasher>(
    events_map: &mut HashMap<String, LeanEvent, S>,
    creator_user_id: &str,
    version: StateResVersion,
) {
    let mut power_events = HashMap::new();
    let power_event_types = [
        M_ROOM_CREATE,
        M_ROOM_POWER_LEVELS,
        M_ROOM_JOIN_RULES,
        M_ROOM_MEMBER,
    ];
    for ev in events_map.values() {
        if power_event_types.contains(&ev.event_type.as_str()) {
            let mut power_ev = ev.clone();
            if (!creator_user_id.is_empty() && ev.sender == creator_user_id)
                || ev.event_type == M_ROOM_CREATE
            {
                power_ev.power_level = 100;
            } else {
                power_ev.power_level = 0;
            }
            power_events.insert(ev.event_id.clone(), power_ev);
        }
    }

    let create_ev = events_map
        .values()
        .find(|ev| ev.event_type == M_ROOM_CREATE);
    let mut pl_cache = HashMap::new();
    let sorted_power_ids =
        rezzy::KahnSortInputs::new(&power_events, events_map, create_ev, version, &mut pl_cache)
            .sort();
    let mut resolved_power_state = ResolvedState::new();
    for id in sorted_power_ids {
        if let Some(ev) = power_events.get(&id) {
            if let Some(state_key) = &ev.state_key {
                resolved_power_state.insert(
                    (EventType::from(ev.event_type.clone()), state_key.clone()),
                    id,
                );
            }
        }
    }

    let mut user_power_levels = HashMap::new();
    let mut default_power_level = 0;
    if let Some(id) =
        resolved_power_state.get(&(EventType::from(M_ROOM_POWER_LEVELS), String::new()))
    {
        if let Some(ev) = events_map.get(id) {
            if let Some(users) = ev.content.get(FIELD_USERS).and_then(|u| u.as_object()) {
                for (user_id, pl) in users {
                    if let Some(pl_val) = pl.as_i64() {
                        user_power_levels.insert(user_id.clone(), pl_val);
                    }
                }
            }
            if let Some(pl_val) = ev
                .content
                .get(FIELD_USERS_DEFAULT)
                .and_then(rezzy::JsonValue::as_i64)
            {
                default_power_level = pl_val;
            }
        }
    }

    for ev in events_map.values_mut() {
        ev.power_level = *user_power_levels
            .get(&ev.sender)
            .unwrap_or(&default_power_level);
    }
}

/// Convert epoch days to a YMD tuple.
///
/// # Panics
///
/// Panics if an intermediate date component cannot be represented by its
/// destination integer type.
#[must_use]
pub fn epoch_days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days.wrapping_add(719_468);
    let era = (if z >= 0 { z } else { z.wrapping_sub(146_096) }).wrapping_div(146_097);
    let doe = u64::try_from(z.wrapping_sub(era.wrapping_mul(146_097))).unwrap();
    let yoe = (doe
        .wrapping_sub(doe.wrapping_div(1460))
        .wrapping_add(doe.wrapping_div(36524))
        .wrapping_sub(doe.wrapping_div(146_096)))
    .wrapping_div(365);
    let y = i64::try_from(yoe)
        .unwrap()
        .wrapping_add(era.wrapping_mul(400));
    let doy = doe.wrapping_sub(
        (365_u64)
            .wrapping_mul(yoe)
            .wrapping_add(yoe.wrapping_div(4))
            .wrapping_sub(yoe.wrapping_div(100)),
    );
    let mp = (5_u64.wrapping_mul(doy).wrapping_add(2)).wrapping_div(153);
    let d = u32::try_from(
        doy.wrapping_sub((153_u64.wrapping_mul(mp).wrapping_add(2)).wrapping_div(5))
            .wrapping_add(1),
    )
    .unwrap();
    let m = u32::try_from(if mp < 10 {
        mp.wrapping_add(3)
    } else {
        mp.wrapping_sub(9)
    })
    .unwrap();
    let y = if m <= 2 { y.wrapping_add(1) } else { y };
    (y, m, d)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rezzy::LeanEvent;

    fn map_from_jsonl(jsonl: &str) -> HashMap<String, LeanEvent> {
        jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let value = rezzy::JsonValue::parse(l).unwrap_or_else(|err| {
                    panic!("failed to parse JSONL fixture line: {err}\nline: {l}")
                });
                let e = LeanEvent::from_value(&value, None).unwrap_or_else(|err| {
                    panic!("failed to parse event in JSONL fixture: {err}\nline: {l}")
                });
                (e.event_id.clone(), e)
            })
            .collect()
    }

    const CREATE_AND_JOIN: &str = r#"
{"event_id":"A","type":"m.room.create","state_key":"","sender":"@x:x","depth":1,"content":{"room_version":"10","creator":"@x:x"},"prev_events":[],"auth_events":[]}
{"event_id":"B","type":"m.room.member","state_key":"@x:x","sender":"@x:x","depth":2,"content":{"membership":"join"},"prev_events":["A"],"auth_events":["A"]}
"#;

    fn events_with(extra: &str) -> HashMap<String, LeanEvent> {
        map_from_jsonl(&format!("{CREATE_AND_JOIN}{extra}"))
    }

    /// A gap in `auth_events` must be reported distinctly from a `prev_events`
    /// gap, and both must be absent when no oracle gap exists.
    #[test]
    fn test_report_gaps_distinguishes_prev_vs_auth() {
        let events = events_with(
            r#"{"event_id":"C","type":"m.room.message","sender":"@x:x","depth":3,"prev_events":["B","MISSING_PREV"],"auth_events":["A","B"]}
{"event_id":"D","type":"m.room.message","sender":"@x:x","depth":4,"prev_events":["C"],"auth_events":["A","MISSING_AUTH"]}"#,
        );

        let (backward, missing_auth) = report_gaps(&events, |_| false);

        assert_eq!(backward.len(), 1);
        assert_eq!(backward[0].event_id, "C");
        assert_eq!(backward[0].missing_prev_events, vec!["MISSING_PREV"]);

        assert_eq!(missing_auth.len(), 1);
        assert_eq!(missing_auth[0].event_id, "D");
        assert_eq!(missing_auth[0].missing_auth_events, vec!["MISSING_AUTH"]);
    }

    /// The `exists` oracle suppresses gaps for events the caller knows about
    /// outside the map (the seam for a fetch check / 128-bit accumulator).
    #[test]
    fn test_report_gaps_uses_exists_oracle() {
        let events = map_from_jsonl(
            r#"
{"event_id":"A","type":"m.room.create","state_key":"","sender":"@x:x","depth":1,"content":{"room_version":"10","creator":"@x:x"},"prev_events":[],"auth_events":[]}
{"event_id":"B","type":"m.room.message","sender":"@x:x","depth":2,"prev_events":["A"],"auth_events":["A"]}
{"event_id":"C","type":"m.room.message","sender":"@x:x","depth":3,"prev_events":["B","KNOWN_ELSEWHERE"],"auth_events":["A"]}
            "#,
        );

        // No oracle: the reference to KNOWN_ELSEWHERE is a backward extremity.
        let (backward, _) = report_gaps(&events, |_| false);
        assert_eq!(backward.len(), 1);

        // Oracle that knows KNOWN_ELSEWHERE: gap suppressed.
        let (backward, _) = report_gaps(&events, |id| id == "KNOWN_ELSEWHERE");
        assert!(backward.is_empty(), "oracle-known prev must not be a gap");
    }

    /// A fully-connected DAG reports no gaps.
    #[test]
    fn test_report_gaps_clean() {
        let events = events_with(
            r#"{"event_id":"C","type":"m.room.message","sender":"@x:x","depth":3,"prev_events":["B"],"auth_events":["A","B"]}"#,
        );
        let (backward, missing_auth) = report_gaps(&events, |_| false);
        assert_eq!(
            backward,
            [] as [rezzy::BackwardExtremity<std::string::String>; 0]
        );
        assert_eq!(
            missing_auth,
            [] as [rezzy::MissingAuthEvent<std::string::String>; 0]
        );
    }

    fn shared_state(entries: &[(&str, &str, &str)]) -> SharedStateMap {
        std::sync::Arc::new(
            entries
                .iter()
                .map(|(typ, key, id)| (((*typ).into(), (*key).to_string()), (*id).to_string()))
                .collect(),
        )
    }

    fn resolve_full(
        parents: &[SharedStateMap],
        events: &HashMap<String, LeanEvent>,
    ) -> ResolvedState {
        let bare: Vec<ResolvedState> = parents.iter().map(|s| s.as_ref().clone()).collect();
        rezzy::resolve_state_maps(&bare, events, StateResVersion::V2_1)
    }

    fn resolve_indexed(
        parents: &[SharedStateMap],
        events: &HashMap<String, LeanEvent>,
    ) -> SharedStateMap {
        let reachability =
            rezzy::resolve::reachability::RangePrefilterReachability::<String>::build(events);
        let mut caches =
            rezzy::ForkResolveCaches::<String, rezzy::JsonValue>::new(StateResVersion::V2_1);
        resolve_parent_states(
            parents,
            events,
            StateResVersion::V2_1,
            &reachability,
            &mut caches,
        )
    }

    /// Regression guard: resolving a fork through the shared reachability
    /// index must produce exactly the same state as the plain full-context
    /// resolver.
    ///
    /// The V2.1+ MSC4297 step needs the forward-reachable side of the
    /// conflicted subgraph. A context narrowed to only the backward-reachable
    /// set of the conflicted events (e.g. an on-demand lazy provider)
    /// under-resolves and silently changes the result — this test fails if that
    /// ever becomes the implementation, or if the shared index diverges from
    /// the per-call index.
    #[test]
    fn indexed_parent_resolution_matches_full_context() {
        let events = map_from_jsonl(
            r#"
{"event_id":"$create","type":"m.room.create","state_key":"","sender":"@a:x","depth":1,"content":{"room_version":"10","creator":"@a:x"},"prev_events":[],"auth_events":[]}
{"event_id":"$pl_a","type":"m.room.power_levels","state_key":"","sender":"@a:x","depth":2,"content":{"users":{"@a:x":100}},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$pl_b","type":"m.room.power_levels","state_key":"","sender":"@b:x","depth":2,"content":{"users":{"@b:x":100,"@a:x":50}},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$ma","type":"m.room.member","state_key":"@a:x","sender":"@a:x","depth":3,"content":{"membership":"join"},"prev_events":["$pl_a"],"auth_events":["$create","$pl_a"]}
{"event_id":"$mb","type":"m.room.member","state_key":"@b:x","sender":"@b:x","depth":3,"content":{"membership":"join"},"prev_events":["$pl_a"],"auth_events":["$create","$pl_a"]}
{"event_id":"$unrelated","type":"m.room.message","sender":"@a:x","depth":3,"content":{"body":"x"},"prev_events":["$pl_a"],"auth_events":["$create","$pl_a"]}
"#,
        );
        let state_a: SharedStateMap = shared_state(&[
            ("m.room.create", "", "$create"),
            ("m.room.power_levels", "", "$pl_a"),
            ("m.room.member", "@a:x", "$ma"),
        ]);
        let state_b: SharedStateMap = shared_state(&[
            ("m.room.create", "", "$create"),
            ("m.room.power_levels", "", "$pl_b"),
            ("m.room.member", "@b:x", "$mb"),
        ]);

        let parents = vec![state_a, state_b];
        let indexed = resolve_indexed(&parents, &events);

        let full = resolve_full(&parents, &events);

        assert_eq!(
            indexed.as_ref(),
            &full,
            "indexed fork resolution must match full-context resolution"
        );
    }

    /// Adversarial regression for the borrowed-room-map optimization.
    ///
    /// The room contains unrelated auth/state branches and message events that
    /// are forward-reachable from the fork's conflicted events but are *not*
    /// auth ancestors of any parent-state event. The full room therefore has a
    /// materially larger forward-reachable set than the fork's auth closure.
    ///
    /// The shared-index resolver passes the whole room by reference, so prove it
    /// resolves identically to both the full-context resolver and the (removed)
    /// filtered auth-closure resolver, for resolved state and HAMT roots.
    #[test]
    fn fork_resolution_equivalent_across_context_shapes() {
        let events = map_from_jsonl(
            r#"
{"event_id":"$create","type":"m.room.create","state_key":"","sender":"@a:x","depth":1,"content":{"room_version":"10","creator":"@a:x"},"prev_events":[],"auth_events":[]}
{"event_id":"$pl_a","type":"m.room.power_levels","state_key":"","sender":"@a:x","depth":2,"content":{"users":{"@a:x":100,"@b:x":50}},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$pl_b","type":"m.room.power_levels","state_key":"","sender":"@b:x","depth":2,"content":{"users":{"@b:x":100,"@a:x":50}},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$ma","type":"m.room.member","state_key":"@a:x","sender":"@a:x","depth":3,"content":{"membership":"join"},"prev_events":["$pl_a"],"auth_events":["$create","$pl_a"]}
{"event_id":"$mb","type":"m.room.member","state_key":"@b:x","sender":"@b:x","depth":3,"content":{"membership":"join"},"prev_events":["$pl_b"],"auth_events":["$create","$pl_b"]}
{"event_id":"$topic_a","type":"m.room.topic","state_key":"","sender":"@a:x","depth":3,"content":{"topic":"a"},"prev_events":["$pl_a"],"auth_events":["$create","$pl_a"]}
{"event_id":"$topic_b","type":"m.room.topic","state_key":"","sender":"@b:x","depth":3,"content":{"topic":"b"},"prev_events":["$pl_b"],"auth_events":["$create","$pl_b"]}
{"event_id":"$m1","type":"m.room.message","sender":"@a:x","depth":4,"content":{"body":"1"},"prev_events":["$ma"],"auth_events":["$create","$pl_a"]}
{"event_id":"$m2","type":"m.room.message","sender":"@a:x","depth":5,"content":{"body":"2"},"prev_events":["$m1"],"auth_events":["$create","$pl_a"]}
{"event_id":"$m3","type":"m.room.message","sender":"@b:x","depth":4,"content":{"body":"3"},"prev_events":["$mb"],"auth_events":["$create","$pl_b"]}
{"event_id":"$m4","type":"m.room.message","sender":"@b:x","depth":5,"content":{"body":"4"},"prev_events":["$m3"],"auth_events":["$create","$pl_b"]}
"#,
        );

        let state_a: SharedStateMap = shared_state(&[
            ("m.room.create", "", "$create"),
            ("m.room.power_levels", "", "$pl_a"),
            ("m.room.member", "@a:x", "$ma"),
            ("m.room.topic", "", "$topic_a"),
        ]);
        let state_b: SharedStateMap = shared_state(&[
            ("m.room.create", "", "$create"),
            ("m.room.power_levels", "", "$pl_b"),
            ("m.room.member", "@b:x", "$mb"),
            ("m.room.topic", "", "$topic_b"),
        ]);

        let parents = vec![state_a, state_b];
        let full = resolve_full(&parents, &events);

        let borrowed = resolve_indexed(&parents, &events);

        let filtered_events = auth_closure(&events, &parents);
        assert!(
            filtered_events.len() < events.len(),
            "fixture must place events outside the fork's auth closure"
        );
        let filtered = resolve_full(&parents, &filtered_events);

        assert_eq!(borrowed.as_ref(), &full, "borrowed room must match full");
        assert_eq!(filtered.as_ref(), &full, "filtered closure must match full");
        assert_eq!(
            hamt_root_hash(&full, b"room"),
            hamt_root_hash(&filtered, b"room"),
            "HAMT roots must match (full vs filtered)"
        );
        assert_eq!(
            hamt_root_hash(&full, b"room"),
            hamt_root_hash(borrowed.as_ref(), b"room"),
            "HAMT roots must match (full vs borrowed)"
        );
    }

    #[test]
    fn v1_partition_and_resolve_state_skips_auth_difference() {
        let events = map_from_jsonl(
            r#"
{"event_id":"$create","type":"m.room.create","state_key":"","sender":"@alice:a.com","depth":0,"origin_server_ts":1000,"content":{"creator":"@alice:a.com","room_version":"1"},"prev_events":[],"auth_events":[]}
{"event_id":"$alice_join","type":"m.room.member","state_key":"@alice:a.com","sender":"@alice:a.com","depth":1,"origin_server_ts":1001,"content":{"membership":"join"},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$pl","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":2,"origin_server_ts":1002,"content":{"users":{"@alice:a.com":100},"events_default":0,"state_default":50,"ban":50,"kick":50,"invite":0},"prev_events":["$alice_join"],"auth_events":["$create","$alice_join"]}
{"event_id":"$bob_join","type":"m.room.member","state_key":"@bob:b.com","sender":"@bob:b.com","depth":3,"origin_server_ts":1003,"content":{"membership":"join"},"prev_events":["$pl"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$pl_diff","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":4,"origin_server_ts":1500,"content":{"users":{"@alice:a.com":100},"events_default":0,"state_default":0,"ban":50,"kick":50,"invite":0},"prev_events":["$bob_join"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$name_alice","type":"m.room.name","state_key":"","sender":"@alice:a.com","depth":5,"origin_server_ts":1600,"content":{"name":"alice"},"prev_events":["$bob_join"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$name_bob","type":"m.room.name","state_key":"","sender":"@bob:b.com","depth":6,"origin_server_ts":1700,"content":{"name":"bob"},"prev_events":["$pl_diff"],"auth_events":["$create","$bob_join","$pl_diff"]}
"#,
        );
        let state_alice: ResolvedState = [
            ((EventType::from("m.room.create"), String::new()), "$create"),
            (
                (EventType::from("m.room.member"), "@alice:a.com".into()),
                "$alice_join",
            ),
            (
                (EventType::from("m.room.member"), "@bob:b.com".into()),
                "$bob_join",
            ),
            (
                (EventType::from("m.room.power_levels"), String::new()),
                "$pl",
            ),
            (
                (EventType::from("m.room.name"), String::new()),
                "$name_alice",
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key, String::from(value)))
        .collect();

        let mut state_bob = state_alice.clone();
        state_bob.insert(
            (EventType::from("m.room.name"), String::new()),
            "$name_bob".into(),
        );

        let auth_graph = rezzy::auth::roaring::AuthGraph::build(&events);
        let heads = vec!["$name_alice".to_string(), "$name_bob".to_string()];
        let state_maps = vec![state_alice, state_bob];

        let (resolved, _) = partition_and_resolve_state(
            &heads,
            &events,
            &state_maps,
            StateResVersion::V1,
            &auth_graph,
        );
        let name_key = (EventType::from("m.room.name"), String::new());
        assert_eq!(
            resolved.get(&name_key).map(String::as_str),
            Some("$name_alice"),
            "V1 skips the auth difference, so $name_bob's auth-only $pl_diff grants Bob no power"
        );
    }

    fn auth_closure(
        events: &HashMap<String, LeanEvent>,
        parents: &[SharedStateMap],
    ) -> HashMap<String, LeanEvent> {
        let auth_graph = rezzy::auth::roaring::AuthGraph::build(events);
        let mut relevant = rezzy::bitmap::Bitmap::new();
        for state in parents {
            for id in state.values() {
                if let Some(idx) = auth_graph.index.index_of(id) {
                    relevant.insert(idx);
                    relevant |= &auth_graph.auth_bitmaps[idx as usize];
                }
            }
        }
        relevant
            .into_iter()
            .filter_map(|idx| {
                let id = auth_graph.index.item_at(idx as usize)?;
                events.get(id).map(|ev| (id.clone(), ev.clone()))
            })
            .collect()
    }

    fn hamt_root_hash(state: &ResolvedState, structural_key: &[u8]) -> rezzy::hamt::StructuralHash {
        rezzy::hamt::build_hamt::<(EventType, String), String, _>(
            structural_key,
            state.iter().map(|(k, v)| (k.clone(), v.clone())),
        )
        .expect("HAMT build")
        .structural_hash
    }
    /// Deterministic DAG fixture for the partition tests: event `i` has one to
    /// `max_prevs` parents among earlier events, every third event is a state
    /// event over a small key space, and `depth` is the longest parent path
    /// plus one, or a copy of a parent's depth to create `(depth, id)` ties.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::format_push_string
    )]
    fn random_dag(seed: u64, events: usize, max_prevs: u64) -> HashMap<String, LeanEvent> {
        let mut state = seed;
        let mut next = move |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        let mut depths: Vec<u64> = Vec::with_capacity(events);
        let mut jsonl = String::new();
        for index in 0..events {
            let id = format!("$e{index:04}");
            let mut prevs: Vec<usize> = Vec::new();
            if index > 0 {
                for _ in 0..=next(max_prevs) {
                    let parent = next(index as u64) as usize;
                    if !prevs.contains(&parent) {
                        prevs.push(parent);
                    }
                }
            }
            let parent_max = prevs.iter().map(|&p| depths[p]).max().unwrap_or(0);
            let depth = if prevs.is_empty() || next(5) != 0 {
                parent_max + 1
            } else {
                parent_max
            };
            depths.push(depth);
            let prev_ids: Vec<String> = prevs.iter().map(|p| format!("\"$e{p:04}\"")).collect();
            let state = if index == 0 {
                String::from(r#""type":"m.room.create","state_key":"","#)
            } else if index % 3 == 0 {
                format!(r#""type":"m.test.{}","state_key":"k{}","#, next(3), next(4))
            } else {
                String::from(r#""type":"m.room.message","#)
            };
            jsonl.push_str(&format!(
                r#"{{"event_id":"{id}",{state}"sender":"@x:x","depth":{depth},"content":{{}},"prev_events":[{}],"auth_events":[]}}"#,
                prev_ids.join(",")
            ));
            jsonl.push('\n');
        }
        map_from_jsonl(&jsonl)
    }

    /// The per-head reference the fast partition replaced: each head's state
    /// map takes, for every key, the ancestor with the greatest
    /// `(depth, event_id)`; the maps are then partitioned by
    /// `rezzy::partition_state_maps`.
    #[allow(clippy::type_complexity)]
    fn partition_by_per_head_maps(
        heads: &[String],
        events: &HashMap<String, LeanEvent>,
    ) -> (Vec<((EventType, String), String)>, Vec<String>) {
        let maps: Vec<ResolvedState> = heads
            .iter()
            .map(|head| {
                let mut best: HashMap<(EventType, String), (u64, &str)> = HashMap::new();
                let mut seen = std::collections::HashSet::new();
                let mut stack = vec![head.as_str()];
                while let Some(id) = stack.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let Some(event) = events.get(id) else {
                        continue;
                    };
                    stack.extend(event.prev_events.iter().map(String::as_str));
                    if let Some(state_key) = &event.state_key {
                        let key = (EventType::from(event.event_type.clone()), state_key.clone());
                        let rank = (event.depth, event.event_id.as_str());
                        let slot = best.entry(key).or_insert(rank);
                        if rank > *slot {
                            *slot = rank;
                        }
                    }
                }
                let mut map = ResolvedState::new();
                for (key, (_, id)) in best {
                    map.insert(key, id.to_owned());
                }
                map
            })
            .collect();
        let (unconflicted, mut conflicted) = rezzy::partition_state_maps(&maps, maps.len());
        conflicted.sort_unstable();
        conflicted.dedup();
        let unconflicted = unconflicted
            .iter()
            .map(|(key, id)| (key.clone(), id.clone()))
            .collect();
        (unconflicted, conflicted)
    }

    fn auto_heads(events: &HashMap<String, LeanEvent>) -> Vec<String> {
        let referenced: std::collections::HashSet<&str> = events
            .values()
            .flat_map(|event| event.prev_events.iter().map(String::as_str))
            .collect();
        let mut heads: Vec<String> = events
            .keys()
            .filter(|id| !referenced.contains(id.as_str()))
            .cloned()
            .collect();
        heads.sort_unstable();
        heads
    }

    /// The bitset partition must give exactly the per-head result, including
    /// keys some heads lack, `(depth, id)` ties, and more than 64 heads.
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn partition_state_maps_fast_matches_per_head_maps() {
        let mut max_heads = 0;
        for seed in 0..24_u64 {
            // Low fan-in leaves many unreferenced events, so some runs cross
            // the 64-head word boundary.
            // Single-parent graphs only: merges take the state-at-event
            // fallback, which is not comparable to the depth-only reference.
            let max_prevs = 1;
            let events = random_dag(seed, 40 + (seed as usize) * 20, max_prevs);
            let heads = auto_heads(&events);
            max_heads = max_heads.max(heads.len());

            // Every head, and a few smaller subsets, as an explicit head list.
            for subset in [
                heads.clone(),
                heads.iter().step_by(3).cloned().collect(),
                heads[..1].to_vec(),
            ] {
                let (fast_unconflicted, mut fast_conflicted) =
                    partition_state_maps_fast(&subset, &events, StateResVersion::V2_1, false);
                fast_conflicted.sort_unstable();
                let fast_unconflicted: Vec<_> = fast_unconflicted
                    .iter()
                    .map(|(key, id)| (key.clone(), id.clone()))
                    .collect();
                let (expected_unconflicted, expected_conflicted) =
                    partition_by_per_head_maps(&subset, &events);

                assert_eq!(
                    fast_unconflicted,
                    expected_unconflicted,
                    "seed {seed}, {} heads",
                    subset.len()
                );
                assert_eq!(
                    fast_conflicted,
                    expected_conflicted,
                    "seed {seed}, {} heads",
                    subset.len()
                );
            }
        }
        assert!(
            max_heads > 64,
            "fixtures must cross the 64-head word boundary, max {max_heads}"
        );
    }

    #[test]
    fn partition_state_maps_fast_with_no_heads_is_empty() {
        let events = random_dag(1, 10, 2);
        let (unconflicted, conflicted) =
            partition_state_maps_fast(&[], &events, StateResVersion::V2_1, false);
        assert_eq!(unconflicted.iter().count(), 0);
        assert!(conflicted.is_empty());
    }
}

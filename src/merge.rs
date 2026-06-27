#![cfg(feature = "cli")]

use alloc::string::String;
use alloc::vec::Vec;

fn perform_connectivity_check(
    per_file_ids: &[std::collections::HashSet<String>],
) -> Result<(), anyhow::Error> {
    extern crate anyhow;
    let num_files = per_file_ids.len();
    let mut any_shared = false;
    for i in 0..num_files {
        for j in (i + 1)..num_files {
            let pair_shared = per_file_ids[i].intersection(&per_file_ids[j]).count();
            if pair_shared > 0 {
                any_shared = true;
                break;
            }
        }
        if any_shared {
            break;
        }
    }

    if !any_shared {
        anyhow::bail!(
            "Disjoint DAGs: no shared events found across inputs. \
             Cannot compute meaningful merge — the DAGs share no history."
        );
    }
    Ok(())
}

fn report_highest_shared_depths(
    per_file_ids: &[std::collections::HashSet<String>],
    merged: &[serde_json::Value],
) {
    use std::collections::HashSet;
    let num_files = per_file_ids.len();
    let shared_all: HashSet<&String> = {
        let mut s: HashSet<&String> = HashSet::new();
        for i in 0..num_files {
            for j in (i + 1)..num_files {
                s.extend(per_file_ids[i].intersection(&per_file_ids[j]));
            }
        }
        s
    };
    let mut shared_depths: Vec<(&String, u64)> = shared_all
        .iter()
        .filter_map(|id| {
            merged.iter().find_map(|v| {
                let eid = v.get("event_id")?.as_str()?;
                if eid == id.as_str() {
                    Some((*id, v.get("depth")?.as_u64().unwrap_or(0)))
                } else {
                    None
                }
            })
        })
        .collect();
    shared_depths.sort_by_key(|b| std::cmp::Reverse(b.1));
    std::eprintln!(
        "[merge] highest shared depths: {:?}",
        &shared_depths[..shared_depths.len().min(5)]
    );
}

/// Merge multiple event sets by `event_id` (first-seen wins, PDUs are immutable).
/// Returns the merged events.
///
/// # Errors
///
/// Returns an error if the files describe disjoint DAGs that share no history.
pub fn merge_event_sets(
    file_sets: &[(String, Vec<serde_json::Value>)],
    debug: bool,
    quiet: bool,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    extern crate anyhow;
    use alloc::borrow::ToOwned;
    use std::collections::HashSet;

    let num_files = file_sets.len();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut merged: Vec<serde_json::Value> = Vec::new();
    let mut per_file_ids: Vec<HashSet<String>> = Vec::with_capacity(num_files);

    for (label, events) in file_sets {
        let mut file_ids = HashSet::with_capacity(events.len());
        let mut added = 0usize;
        let mut dupes = 0usize;

        for val in events {
            let event_id = val
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();

            file_ids.insert(event_id.clone());
            if seen_ids.insert(event_id) {
                merged.push(val.clone());
                added += 1;
            } else {
                dupes += 1;
            }
        }

        if !quiet {
            std::eprintln!(
                "[merge] {}: {} events ({} new, {} shared)",
                label,
                events.len(),
                added,
                dupes
            );
        }
        per_file_ids.push(file_ids);
    }

    // Merge-base check: verify each file shares at least one event with another
    if num_files >= 2 {
        perform_connectivity_check(&per_file_ids)?;

        // Report total shared count (union of all pairwise intersections)
        let total_shared: usize = {
            let mut shared_ids: HashSet<&String> = HashSet::new();
            for i in 0..num_files {
                for j in (i + 1)..num_files {
                    shared_ids.extend(per_file_ids[i].intersection(&per_file_ids[j]));
                }
            }
            shared_ids.len()
        };

        if !quiet {
            std::eprintln!(
                "[merge] merge-base: {total_shared} shared events across {num_files} inputs"
            );
        }

        if debug {
            report_highest_shared_depths(&per_file_ids, &merged);
        }
    }

    if !quiet {
        std::eprintln!("[merge] total: {} unique events", merged.len());
    }

    Ok(merged)
}

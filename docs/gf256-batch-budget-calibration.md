# GF(2^256) capacity-32 batch work calibration

Release-mode work-unit measurements for 128-request batches. These numbers
measure decoder work, not elapsed time, and do not change either work ceiling.

## Reproduce

```sh
cargo +stable test -p rezzy-recon --release --lib \
  gf256_128_bucket_batch_work_calibration -- --ignored --nocapture
```

The ignored harness is in `rezzy-recon/src/triage.rs`. It used one cached
`Gf256Decoder`, capacity 32 for all 128 requests, and 100 batches per scenario
and pattern. Each batch starts with an explicit `300,000,000 × 128` work-unit
allowance. For each bucket, the harness records that bucket's local
`allowance - remaining`, then subtracts the charge from the shared batch
allowance. Successful means decoded roots exactly equal the generated set;
errors and incorrect over-capacity results count as failed and their work is
still charged.

For total-difference scenarios, each batch's exact number of roots is assigned
to the 128 buckets by deterministic uniform pseudo-random placement. Within each
bucket, roots are either deterministic xorshift pseudo-random 256-bit values or
consecutive low-limb field values. This keeps bucket occupancy comparable while
changing root structure. `worst_bucket_32` places 32 roots in one bucket and
leaves the other 127 empty. Roots and sketches are generated before each decode;
decoder construction, population setup, and sketch-building work are excluded.
The work counter covers root finding/factoring, not Berlekamp–Massey locator
preparation. When an over-capacity bucket fails during locator computation (such
as locator degree exceeding capacity 32 or syndrome inconsistency), root
factoring is never entered and factoring work is 0, explaining the 0-unit median
in consecutive over-capacity scenarios.

The full output is in `benches/bench-results.csv` under the `work_units` column.
New work-unit rows can be appended with
`scripts/record_bench.py --work-units <captured calibration output>`. The run
used `rustc 1.99.0` on `x86_64-unknown-linux-gnu`, against revision `5b6653a6`,
which contains the ignored harness. The appended calibration rows carry the same
revision (recorded on 2026-11-04 and 2026-11-05). Historical rows in that CSV
predate the `work_units` column and remain millisecond measurements.

## Per-bucket work

Each bucket distribution has 12,800 samples (100 batches × 128 buckets).
Percentiles use nearest-rank selection. Empty outcome groups have no row. Values
are work units.

| Pattern     | Scenario        | Outcome | Samples |     P50 |     P95 |     P99 |     Max |
| ----------- | --------------- | ------- | ------: | ------: | ------: | ------: | ------: |
| random      | total 10        | success |  12,800 |       0 |       0 |       0 |   3,081 |
| random      | total 100       | success |  12,800 |       0 |       0 |   4,635 |  19,293 |
| random      | total 1,000     | success |  12,800 |  36,023 | 105,238 | 148,521 | 302,566 |
| random      | total 10,000    | failed  |  12,800 | 261,120 | 261,120 | 261,120 | 261,120 |
| random      | worst bucket 32 | success |  12,800 |       0 |       0 |       0 | 712,488 |
| consecutive | total 10        | success |  12,800 |       0 |       0 |       0 |   3,858 |
| consecutive | total 100       | success |  12,800 |       0 |       0 |   3,858 |  18,504 |
| consecutive | total 1,000     | success |  12,800 |  35,999 |  95,446 | 124,782 | 285,202 |
| consecutive | total 10,000    | failed  |  12,800 |       0 | 261,120 | 544,314 | 583,995 |
| consecutive | worst bucket 32 | success |  12,800 |       0 |       0 |       0 | 584,511 |

## Aggregate work per 128-bucket batch

These distributions have 100 batch samples each. `failed_work` is included in
`total_work`; it is shown separately to expose the work spent before fallback.

| Pattern     | Scenario        | Work group |        P50 |        P95 |        P99 |        Max |
| ----------- | --------------- | ---------- | ---------: | ---------: | ---------: | ---------: |
| random      | total 10        | success    |          0 |          0 |          0 |      3,081 |
| random      | total 10        | failed     |          0 |          0 |          0 |          0 |
| random      | total 10        | total      |          0 |          0 |          0 |      3,081 |
| random      | total 100       | success    |     22,344 |     40,874 |     46,010 |     50,368 |
| random      | total 100       | failed     |          0 |          0 |          0 |          0 |
| random      | total 100       | total      |     22,344 |     40,874 |     46,010 |     50,368 |
| random      | total 1,000     | success    |  5,507,707 |  5,654,436 |  5,722,044 |  5,730,955 |
| random      | total 1,000     | failed     |          0 |          0 |          0 |          0 |
| random      | total 1,000     | total      |  5,507,707 |  5,654,436 |  5,722,044 |  5,730,955 |
| random      | total 10,000    | success    |          0 |          0 |          0 |          0 |
| random      | total 10,000    | failed     | 33,423,360 | 33,423,360 | 33,423,360 | 33,423,360 |
| random      | total 10,000    | total      | 33,423,360 | 33,423,360 | 33,423,360 | 33,423,360 |
| random      | worst bucket 32 | success    |    640,094 |    692,186 |    703,880 |    712,488 |
| random      | worst bucket 32 | failed     |          0 |          0 |          0 |          0 |
| random      | worst bucket 32 | total      |    640,094 |    692,186 |    703,880 |    712,488 |
| consecutive | total 10        | success    |          0 |          0 |          0 |      3,858 |
| consecutive | total 10        | failed     |          0 |          0 |          0 |          0 |
| consecutive | total 10        | total      |          0 |          0 |          0 |      3,858 |
| consecutive | total 100       | success    |     27,006 |     46,788 |     51,408 |     56,557 |
| consecutive | total 100       | failed     |          0 |          0 |          0 |          0 |
| consecutive | total 100       | total      |     27,006 |     46,788 |     51,408 |     56,557 |
| consecutive | total 1,000     | success    |  4,957,626 |  5,079,139 |  5,119,317 |  5,179,584 |
| consecutive | total 1,000     | failed     |          0 |          0 |          0 |          0 |
| consecutive | total 1,000     | total      |  4,957,626 |  5,079,139 |  5,119,317 |  5,179,584 |
| consecutive | total 10,000    | success    |          0 |          0 |          0 |          0 |
| consecutive | total 10,000    | failed     |  2,465,694 |  3,660,470 |  4,125,422 |  4,686,100 |
| consecutive | total 10,000    | total      |  2,465,694 |  3,660,470 |  4,125,422 |  4,686,100 |
| consecutive | worst bucket 32 | success    |    584,511 |    584,511 |    584,511 |    584,511 |
| consecutive | worst bucket 32 | failed     |          0 |          0 |          0 |          0 |
| consecutive | worst bucket 32 | total      |    584,511 |    584,511 |    584,511 |    584,511 |

## Budget interpretation

The largest successful per-bucket sample was 712,488 units; the largest batch
sample was 33,423,360 units, including failed buckets. These observations are
not a tail bound. For context, `WorkModel::GF256.single_call_work_ceiling(32)`
is 2,695,168 units: the model ceiling for one root-finding call at degree 32. It
is not a bound on total recursive work for every valid bucket.

The decoder takes one shared batch budget. If it is less than the sum of the
remaining per-request ceilings, the current allowance is divided evenly among
the requests still requiring decode; unused work rolls forward. This reduces
request-order starvation, but is not a strict per-bucket cap: later requests can
receive rolled-over work. A bucket that exhausts its share is recorded as failed
while other buckets continue; the caller can retry or split failed buckets with
a fresh allowance. The full helper's `128 × 300,000,000` budget gives every
request its full current allowance.

No lower production batch budget is selected here. Enforcing a hard per-bucket
cap would still require separate per-request accounting; the shared fair-share
budget allows unused work to roll forward. Keep the 300,000,000 per-sketch
ceiling and `MAX_BATCH_FACTOR_WORK` unchanged. These measurements do not prove
the recursive work bound.

## Expanded batch sample

Follow-up run at revision `5b6653a6` on 2026-11-05 with rustc 1.99.0. The same
reproducible ignored release test now uses 1,000 repetitions per pattern and
scenario: 128,000 bucket samples and 1,000 batch samples in each cell. The raw
P50/P95/P99/max values are appended to `benches/bench-results.csv` under
`samples=128000` and `samples=1000`.

All buckets decoded successfully for total differences 10, 100, and 1,000, and
for the single 32-root stress bucket. For total differences 10,000, all 128,000
buckets failed in both patterns: the average occupancy is over the capacity
of 32. Their work is failed-decode work, not successful factoring.

### Per-bucket work (work units)

| Pattern     | Scenario        | Outcome | Samples |     P50 |     P95 |     P99 |     Max |
| ----------- | --------------- | ------- | ------: | ------: | ------: | ------: | ------: |
| random      | total 10        | success | 128,000 |       0 |       0 |       0 |   3,858 |
| random      | total 100       | success | 128,000 |       0 |       0 |   4,635 |  28,864 |
| random      | total 1,000     | success | 128,000 |  36,023 | 104,788 | 146,794 | 311,293 |
| random      | total 10,000    | failed  | 128,000 | 261,120 | 261,120 | 261,120 | 261,120 |
| random      | worst bucket 32 | success | 128,000 |       0 |       0 |       0 | 753,949 |
| consecutive | total 10        | success | 128,000 |       0 |       0 |       0 |   3,858 |
| consecutive | total 100       | success | 128,000 |       0 |       0 |   3,858 |  24,419 |
| consecutive | total 1,000     | success | 128,000 |  35,999 |  95,446 | 124,782 | 285,202 |
| consecutive | total 10,000    | failed  | 128,000 |       0 | 261,120 | 544,314 | 583,995 |
| consecutive | worst bucket 32 | success | 128,000 |       0 |       0 |       0 | 584,511 |

### Aggregate work per 128-bucket batch (work units)

Failed-bucket work is included in total work and shown separately.

| Pattern     | Scenario        | Work group | Samples |        P50 |        P95 |        P99 |        Max |
| ----------- | --------------- | ---------- | ------: | ---------: | ---------: | ---------: | ---------: |
| random      | total 10        | success    |   1,000 |          0 |          0 |          0 |      3,858 |
| random      | total 10        | failed     |   1,000 |          0 |          0 |          0 |          0 |
| random      | total 10        | total      |   1,000 |          0 |          0 |          0 |      3,858 |
| random      | total 100       | success    |   1,000 |     23,121 |     41,136 |     50,368 |     66,331 |
| random      | total 100       | failed     |   1,000 |          0 |          0 |          0 |          0 |
| random      | total 100       | total      |   1,000 |     23,121 |     41,136 |     50,368 |     66,331 |
| random      | total 1,000     | success    |   1,000 |  5,501,574 |  5,685,146 |  5,754,975 |  5,867,486 |
| random      | total 1,000     | failed     |   1,000 |          0 |          0 |          0 |          0 |
| random      | total 1,000     | total      |   1,000 |  5,501,574 |  5,685,146 |  5,754,975 |  5,867,486 |
| random      | total 10,000    | success    |   1,000 |          0 |          0 |          0 |          0 |
| random      | total 10,000    | failed     |   1,000 | 33,423,360 | 33,423,360 | 33,423,360 | 33,423,360 |
| random      | total 10,000    | total      |   1,000 | 33,423,360 | 33,423,360 | 33,423,360 | 33,423,360 |
| random      | worst bucket 32 | success    |   1,000 |    639,288 |    682,069 |    713,566 |    753,949 |
| random      | worst bucket 32 | failed     |   1,000 |          0 |          0 |          0 |          0 |
| random      | worst bucket 32 | total      |   1,000 |    639,288 |    682,069 |    713,566 |    753,949 |
| consecutive | total 10        | success    |   1,000 |          0 |          0 |          0 |      3,858 |
| consecutive | total 10        | failed     |   1,000 |          0 |          0 |          0 |          0 |
| consecutive | total 10        | total      |   1,000 |          0 |          0 |          0 |      3,858 |
| consecutive | total 100       | success    |   1,000 |     27,006 |     44,994 |     53,226 |     64,260 |
| consecutive | total 100       | failed     |   1,000 |          0 |          0 |          0 |          0 |
| consecutive | total 100       | total      |   1,000 |     27,006 |     44,994 |     53,226 |     64,260 |
| consecutive | total 1,000     | success    |   1,000 |  4,962,248 |  5,093,064 |  5,162,327 |  5,247,121 |
| consecutive | total 1,000     | failed     |   1,000 |          0 |          0 |          0 |          0 |
| consecutive | total 1,000     | total      |   1,000 |  4,962,248 |  5,093,064 |  5,162,327 |  5,247,121 |
| consecutive | total 10,000    | success    |   1,000 |          0 |          0 |          0 |          0 |
| consecutive | total 10,000    | failed     |   1,000 |  2,511,841 |  3,910,187 |  4,580,722 |  5,089,712 |
| consecutive | total 10,000    | total      |   1,000 |  2,511,841 |  3,910,187 |  4,580,722 |  5,089,712 |
| consecutive | worst bucket 32 | success    |   1,000 |    584,511 |    584,511 |    584,511 |    584,511 |
| consecutive | worst bucket 32 | failed     |   1,000 |          0 |          0 |          0 |          0 |
| consecutive | worst bucket 32 | total      |   1,000 |    584,511 |    584,511 |    584,511 |    584,511 |

## an-program packages

| Workload | Iterations | Samples | Min (us) | Median (us) | P95 (us) | Max (us) | Checksum |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| encoding::crc32_text/3520B | 256 | 9 | 15000 | 16000 | 31000 | 31000 | `0x0000004c46bdf700` |
| glob_regex::glob_match/8paths | 32768 | 9 | 15000 | 16000 | 16000 | 16000 | `0x0000000000020000` |
| json::parse_json/261B | 8192 | 9 | 31000 | 31000 | 47000 | 47000 | `0x000000000002e000` |

Measured 610 ms across 3 workloads. Each sample times one batch of `Iterations` runs.

Clock: `monotonic_ms()` with an observed tick of 15 ms. Batch sizes double until one batch takes at least 20 ms, so each sample is a whole number of milliseconds reported in microseconds.

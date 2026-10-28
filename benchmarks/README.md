# Anode 2.0 compared with Rust

Four small programs, each written once in Anode (`anode/`) and once in idiomatic Rust (`rust/`). The two versions of each program use the same algorithm and input size and print identical output.

| Program | What it does | Output |
| :--- | :--- | :--- |
| `lines` | Builds a 200,000-line `label\|amount` document, then counts rows, the amount total and malformed lines | `156380` `78198330` `17375` |
| `sieve` | Counts the primes up to 2,000,000 with a sieve of Eratosthenes | `148933` |
| `words` | Counts 300,000 words from a 5,000-word vocabulary with a hash map | `5000` `60` `1433400` |
| `fib` | Naive recursive Fibonacci of 30, about 2.7 million calls | `832040` |

## Results

Medians in milliseconds (type-check over 9 runs, builds over 5, runtime over 9), and the size of the release executable.

| Program | Type-check | Debug build | Release build | Runtime | Executable |
| :--- | ---: | ---: | ---: | ---: | ---: |
| `lines`, Anode | **26.4** | 312.6 | 882.6 | 453.0 | **69 KB** |
| `lines`, Rust | 75.2 | **184.1** | **243.6** | **46.4** | 132 KB |
| `sieve`, Anode | **27.1** | 301.2 | 722.2 | 769.3 | **56 KB** |
| `sieve`, Rust | 73.1 | **160.1** | **181.6** | **42.0** | 129 KB |
| `words`, Anode | **27.3** | 299.8 | 750.4 | 119.0 | **58 KB** |
| `words`, Rust | 72.4 | **195.6** | **312.3** | **52.5** | 133 KB |
| `fib`, Anode | **25.6** | 299.9 | 393.5 | 215.2 | **21 KB** |
| `fib`, Rust | 66.6 | **150.1** | **160.9** | **25.2** | 128 KB |

Across the four programs (geometric means):

- **Type-check:** Anode is 2.7× faster.
- **Executable size:** Anode's executables are 2.8× smaller.
- **Debug builds:** Rust is 1.8× faster.
- **Release builds:** Rust is 3.0× faster.
- **Runtime:** Rust is 7.7× faster.

Anode 2.0 generates C in which every value is a tagged, boxed value, and it checks integer overflow and bounds in every build. Rust's release builds do not check overflow.

## What was measured

| Step | Anode 2.0.0 | Rust 1.99.0 |
| :--- | :--- | :--- |
| Type-check | `anodec check X.an` | `rustc --edition 2021 --emit=metadata X.rs` |
| Debug build | `anodec build X.an` (MSVC `/Od`) | `rustc --edition 2021 X.rs` |
| Release build | `anodec build X.an --release` (MSVC `/O2`) | `rustc --edition 2021 -C opt-level=3 X.rs` |
| Runtime | the release executable | the release executable |

Every time is the wall time of the whole process, startup included. Anode used the released `anodec` 2.0.0 for Windows with MSVC `cl` 19.44. The machine was Windows 11 on an Intel Core Ultra 9 386H.

## Reproduce

With `ANODE_HOME` set to an Anode 2.0 SDK, and Node.js and `rustc` on `PATH`:

```bash
node measure.js . "$ANODE_HOME/bin/anodec.exe"   # writes results.json
node chart.js results.json ../assets/benchmarks-2.0.svg
```

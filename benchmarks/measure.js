// Times Anode 2.0 and Rust on the same four programs: type-check, debug build,
// release build and runtime (medians), plus release executable size.
const { spawnSync } = require("child_process"), fs = require("fs"), path = require("path");
const [root, anodec] = process.argv.slice(2);
const workloads = ["lines", "sieve", "words", "fib"];
const median = (xs) => { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; };
function time(cmd, args, cwd, runs) {
  const samples = [];
  for (let i = 0; i < runs; i++) {
    const t0 = process.hrtime.bigint();
    const r = spawnSync(cmd, args, { cwd, encoding: "utf8", env: process.env, windowsHide: true });
    const t1 = process.hrtime.bigint();
    if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed: ${r.stdout}${r.stderr}`);
    samples.push(Number(t1 - t0) / 1e6);
  }
  return { median: median(samples), samples };
}
const results = {};
for (const w of workloads) {
  const a = path.join(root, "anode"), r = path.join(root, "rust");
  results[w] = {
    anode: {
      check: time(anodec, ["check", `${w}.an`], a, 9),
      debug_build: time(anodec, ["build", `${w}.an`, "-o", `${w}-debug.exe`], a, 5),
      release_build: time(anodec, ["build", `${w}.an`, "--release", "-o", `${w}.exe`], a, 5),
    },
    rust: {
      check: time("rustc", ["--edition", "2021", "--emit=metadata", "-o", `${w}.rmeta`, `${w}.rs`], r, 9),
      debug_build: time("rustc", ["--edition", "2021", `${w}.rs`, "-o", `${w}-debug.exe`], r, 5),
      release_build: time("rustc", ["--edition", "2021", "-C", "opt-level=3", `${w}.rs`, "-o", `${w}.exe`], r, 5),
    },
  };
  results[w].anode.run = time(path.join(a, `${w}.exe`), [], a, 9);
  results[w].rust.run = time(path.join(r, `${w}.exe`), [], r, 9);
  results[w].anode.exe_bytes = fs.statSync(path.join(a, `${w}.exe`)).size;
  results[w].rust.exe_bytes = fs.statSync(path.join(r, `${w}.exe`)).size;
  const f = (x) => x.median.toFixed(1).padStart(8);
  console.log(`${w.padEnd(6)} check ${f(results[w].anode.check)} vs ${f(results[w].rust.check)} | debug ${f(results[w].anode.debug_build)} vs ${f(results[w].rust.debug_build)} | release ${f(results[w].anode.release_build)} vs ${f(results[w].rust.release_build)} | run ${f(results[w].anode.run)} vs ${f(results[w].rust.run)} | exe ${results[w].anode.exe_bytes} vs ${results[w].rust.exe_bytes}`);
}
fs.writeFileSync(path.join(root, "results.json"), JSON.stringify(results, null, 2));

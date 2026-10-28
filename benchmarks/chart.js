// Draws results.json (from measure.js) as one SVG card of horizontal bar
// charts: Anode in the site's red-orange, Rust in slate.
//   node chart.js results.json out.svg
const fs = require("fs");
const [input, output] = process.argv.slice(2);
const results = JSON.parse(fs.readFileSync(input, "utf8"));
const workloads = ["lines", "sieve", "words", "fib"];
const labels = { lines: "Line scanner", sieve: "Prime sieve", words: "Word count", fib: "Recursion" };
const metrics = [
  { key: "check", title: "Type-check", unit: "ms", noun: "faster" },
  { key: "debug_build", title: "Debug build", unit: "ms", noun: "faster" },
  { key: "release_build", title: "Release build", unit: "ms", noun: "faster" },
  { key: "run", title: "Runtime (release build)", unit: "ms", noun: "faster" },
  { key: "exe_bytes", title: "Executable size", unit: "KB", noun: "smaller" },
];
const color = { bg: "#15191c", surface: "#1c2124", line: "#2d353b", fg: "#eef1f3", muted: "#95a2ad", anode: "#e94218", rust: "#5d6f80", rustText: "#8a9aa9" };
const sans = "IBM Plex Sans, Segoe UI, system-ui, -apple-system, sans-serif";
const mono = "JetBrains Mono, Cascadia Mono, Consolas, ui-monospace, monospace";
const value = (side, metric) => metric.key === "exe_bytes" ? side.exe_bytes / 1024 : side[metric.key].median;
const shown = (x, metric) => metric.unit === "KB" ? `${Math.round(x)} KB` : x >= 100 ? `${Math.round(x)} ms` : `${x.toFixed(1)} ms`;
const geomean = (xs) => Math.exp(xs.reduce((s, x) => s + Math.log(x), 0) / xs.length);
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");

const W = 960, pad = 24, gap = 16, colW = (W - 2 * pad - gap) / 2, panelH = 252, top = 92;
const parts = [];
parts.push(`<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${top + 3 * (panelH + gap) + pad - gap}" viewBox="0 0 ${W} ${top + 3 * (panelH + gap) + pad - gap}" role="img" aria-label="Anode 2.0 compared with Rust 1.99 on four programs">`);
parts.push(`<rect width="100%" height="100%" rx="14" fill="${color.bg}"/>`);
parts.push(`<text x="${pad}" y="44" font-family="${sans}" font-size="24" font-weight="700" fill="${color.fg}">Anode 2.0 vs Rust 1.99</text>`);
parts.push(`<text x="${pad}" y="70" font-family="${sans}" font-size="13.5" fill="${color.muted}">Four matching programs, identical output. Windows 11, Intel Core Ultra 9 386H. Medians; lower is better.</text>`);
parts.push(`<rect x="${W - pad - 210}" y="34" width="12" height="12" rx="2" fill="${color.anode}"/><text x="${W - pad - 192}" y="45" font-family="${sans}" font-size="13" fill="${color.fg}">Anode 2.0</text>`);
parts.push(`<rect x="${W - pad - 110}" y="34" width="12" height="12" rx="2" fill="${color.rust}"/><text x="${W - pad - 92}" y="45" font-family="${sans}" font-size="13" fill="${color.fg}">Rust 1.99</text>`);

metrics.forEach((metric, index) => {
  const x0 = pad + (index % 2) * (colW + gap), y0 = top + Math.floor(index / 2) * (panelH + gap);
  const pairs = workloads.map((w) => ({ w, a: value(results[w].anode, metric), r: value(results[w].rust, metric) }));
  const anodeWins = geomean(pairs.map((p) => p.r / p.a)) >= 1;
  const ratio = anodeWins ? geomean(pairs.map((p) => p.r / p.a)) : geomean(pairs.map((p) => p.a / p.r));
  const verdict = `${anodeWins ? "Anode" : "Rust"} ${ratio.toFixed(1)}× ${metric.noun}`;
  const max = Math.max(...pairs.flatMap((p) => [p.a, p.r]));
  parts.push(`<rect x="${x0}" y="${y0}" width="${colW}" height="${panelH}" rx="10" fill="${color.surface}" stroke="${color.line}"/>`);
  parts.push(`<text x="${x0 + 18}" y="${y0 + 30}" font-family="${sans}" font-size="16" font-weight="700" fill="${color.fg}">${esc(metric.title)}</text>`);
  parts.push(`<text x="${x0 + colW - 18}" y="${y0 + 30}" text-anchor="end" font-family="${sans}" font-size="13.5" font-weight="700" fill="${anodeWins ? color.anode : color.rustText}">${verdict}</text>`);
  const labelW = 104, barX = x0 + 18 + labelW, barMax = colW - 36 - labelW - 70;
  pairs.forEach((p, i) => {
    const gy = y0 + 52 + i * 48;
    parts.push(`<text x="${x0 + 18}" y="${gy + 19}" font-family="${sans}" font-size="12.5" fill="${color.muted}">${labels[p.w]}</text>`);
    [[p.a, color.anode, 0], [p.r, color.rust, 19]].forEach(([v, fill, dy]) => {
      const w = Math.max(2, (v / max) * barMax);
      parts.push(`<rect x="${barX}" y="${gy + dy}" width="${w.toFixed(1)}" height="15" rx="3" fill="${fill}"/>`);
      parts.push(`<text x="${(barX + w + 7).toFixed(1)}" y="${gy + dy + 12}" font-family="${mono}" font-size="11.5" fill="${fill === color.anode ? color.fg : color.rustText}">${shown(v, metric)}</text>`);
    });
  });
});

// Sixth cell: what was measured.
const x6 = pad + colW + gap, y6 = top + 2 * (panelH + gap);
parts.push(`<rect x="${x6}" y="${y6}" width="${colW}" height="${panelH}" rx="10" fill="${color.surface}" stroke="${color.line}"/>`);
parts.push(`<text x="${x6 + 18}" y="${y6 + 30}" font-family="${sans}" font-size="16" font-weight="700" fill="${color.fg}">What was measured</text>`);
const notes = [
  ["Type-check", "anodec check vs rustc --emit=metadata"],
  ["Debug build", "anodec build vs rustc (opt-level 0)"],
  ["Release build", "anodec build --release vs rustc -C opt-level=3"],
  ["Runtime", "whole process, the release builds"],
  ["Compilers", "anodec 2.0.0 uses MSVC cl 19.44; rustc 1.99.0"],
  ["Note", "Anode checks integer overflow in every build;"],
  ["", "Rust release builds do not."],
];
notes.forEach(([k, v], i) => {
  const y = y6 + 62 + i * 25;
  parts.push(`<text x="${x6 + 18}" y="${y}" font-family="${sans}" font-size="12.5" font-weight="600" fill="${color.anode}">${esc(k)}</text>`);
  parts.push(`<text x="${x6 + 128}" y="${y}" font-family="${sans}" font-size="12.5" fill="${color.muted}">${esc(v)}</text>`);
});
parts.push(`</svg>`);
fs.writeFileSync(output, parts.join("\n") + "\n");
console.log(`wrote ${output}`);

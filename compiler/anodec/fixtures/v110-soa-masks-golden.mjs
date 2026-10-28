// Computes the known-output golden fixtures/v110-soa-masks.stdout for the default
// run (count 48, mode "run") of fixtures/v110-soa-masks.an with an independent
// BigInt model: int is two's-complement 64-bit with checked +,-,* (overflow throws),
// truncating checked / and % (sign of the dividend, so -256 % 256 == 0), u64/u32
// intrinsics follow the 0.28 contract (wrapping modulo 2^width, counts
// (uint64)bits & (width-1), logical unsigned shr), int(...)/u64(...)/u32(...) are
// checked conversions, and SoA vectors are modelled as plain arrays of records
// (push appends, pop removes the last row, replace overwrites, clone copies).
// It never runs the seed, the bootstrap frontend or generated C; verify.ps1 compares
// all of them against this golden.
// Usage (from examples/bootstrap-compiler):
//   node fixtures/v110-soa-masks-golden.mjs          (writes the golden)
//   node fixtures/v110-soa-masks-golden.mjs --check  (exit 1 unless identical)
//   node fixtures/v110-soa-masks-golden.mjs --count N [--mode MODE]  (print the model for that run;
//     a fault mode prints the stdout before the fault and the fault message on stderr, exit 1)
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const MIN = -(1n << 63n), MAX = (1n << 63n) - 1n;
const checked = v => { if (v < MIN || v > MAX) throw Error('integer overflow'); return v; };
const add = (a, b) => checked(a + b), sub = (a, b) => checked(a - b), mul = (a, b) => checked(a * b);
const div = (a, b) => { if (b === 0n) throw Error('division by zero'); return checked(a / b); }; // BigInt / truncates
const rem = (a, b) => { if (b === 0n) throw Error('remainder by zero'); return a % b; };          // sign of dividend
const convert = (v, lo, hi) => { if (v < lo || v > hi) throw Error('integer conversion out of range'); return v; };
const toInt = v => convert(v, MIN, MAX);
const toU64 = v => convert(v, 0n, (1n << 64n) - 1n);
const toU32 = v => convert(v, 0n, 0xFFFFFFFFn);
const unsigned = width => {
  const W = BigInt(width), mask = (1n << W) - 1n;
  const count = n => BigInt.asUintN(64, n) & (W - 1n);
  return {
    wrapping_mul: (a, b) => (a * b) & mask,
    bit_and: (a, b) => a & b & mask,
    bit_or: (a, b) => (a | b) & mask,
    bit_xor: (a, b) => (a ^ b) & mask,
    bit_not: a => ~a & mask,
    shl: (a, n) => (a << count(n)) & mask,
    shr: (a, n) => (a & mask) >> count(n),
  };
};
const u64 = unsigned(64), u32 = unsigned(32);
const xorInt = (a, b) => BigInt.asIntN(64, BigInt.asUintN(64, a) ^ BigInt.asUintN(64, b));

const mix = (seed, value) => { const h = u64.wrapping_mul(u64.bit_xor(seed, toU64(value)), 1099511628211n); return u64.bit_xor(h, u64.shr(h, 29n)); };
const laneOf = h => toInt(u64.bit_and(h, 255n));
const wideOf = h => toInt(u64.bit_and(h, 9223372036854775807n));
const narrowOf = v => toInt(u32.bit_and(v, 4095n));
const build = (count, seed) => {
  const rows = []; let h = seed;
  for (let i = 0n; i < count; i++) { h = mix(h, i); rows.push({ key: h, weight: sub(laneOf(h), 128n), label: String(rem(i, 7n)) }); }
  return rows;
};
const histogram = rows => {
  const lanes = Array(16).fill(0n);
  for (const row of rows) { const slot = Number(rem(laneOf(row.key), 16n)); lanes[slot] = add(lanes[slot], 1n); }
  return lanes;
};
const foldLabels = rows => {
  let total = 0n;
  for (const row of rows) total = rem(add(add(mul(total, 7n), BigInt(Buffer.byteLength(row.label, 'utf8'))), BigInt(row.label)), 1000003n);
  return total;
};
const churn = (rows, keep) => {
  let dropped = 0n;
  while (BigInt(rows.length) > keep) { const last = rows.pop(); dropped = add(add(dropped, last.weight), BigInt(Buffer.byteLength(last.label, 'utf8'))); }
  return dropped;
};
const block = (start, bias, base) => {
  let acc = 0n;
  for (let i = start; i < add(start, 4n); i++) { const up = add(i, bias), down = sub(base, i); acc = xorInt(acc, xorInt(up, down)); }
  return acc;
};

// Mirrors main() exactly, including the mode branches: a checked fault throws at the same
// point, so runMode() returns the partial stdout printed before it and the fault message.
const MIN_I64 = -(1n << 63n), MAX_I64 = (1n << 63n) - 1n;
function run(count, mode = 'run', out = []) {
  const seed = toU64(1469598103934665603n);
  let rows = build(count, seed); out.push(rows.length);
  let spread = 0n; for (const v of histogram(rows)) spread = add(mul(spread, 3n), v); out.push(spread);
  let wide = 0n, narrow = 0n, weights = 0n;
  for (const row of rows) { wide = xorInt(wide, wideOf(row.key)); narrow = add(narrow, narrowOf(toU32(add(mul(laneOf(row.key), 16n), 1n)))); weights = add(weights, row.weight); }
  out.push(wide, narrow, weights);
  if (mode === 'soa-index' && count >= BigInt(rows.length)) throw Error('out of bounds');
  const ticks = [];
  for (let i = 0n; i < count; i++) ticks.push({ at: mul(i, 3n), delta: sub(rem(i, 8n), div(i, 4n)) });
  const snapshot = ticks.map(t => ({ ...t }));
  if (ticks.length > 0) ticks[0] = { at: -1n, delta: 7n };
  let tickSum = 0n; for (const t of ticks) tickSum = add(add(tickSum, mul(t.at, 2n)), t.delta);
  let snapSum = 0n; for (const t of snapshot) snapSum = add(add(snapSum, t.at), t.delta);
  out.push(tickSum, snapSum, foldLabels(rows));
  const dropped = churn(rows, div(count, 2n)); out.push(dropped, rows.length);
  rows = build(3n, toU64(7n)); out.push(rows.length, foldLabels(rows));
  if (mode === 'pop-empty') throw Error('pop from empty vector');
  let negatives = 0n;
  for (let i = 0n; i < 6n; i++) { const v = sub(0n, mul(i, 256n)), w = sub(v, 3n); negatives = add(add(add(mul(negatives, 10n), rem(v, 256n)), div(v, 256n)), rem(w, 256n)); }
  out.push(negatives);
  let bias = 1000n, base = -1000n;
  if (mode === 'overflow') bias = sub(MAX_I64, 13n);
  if (mode === 'underflow') base = add(sub(sub(0n, MAX_I64), 1n), 9n);
  for (let b = 0n; b < 5n; b++) out.push(block(mul(b, 4n), bias, base));
  const table = []; for (let i = 0n; i < 256n; i++) table.push(mul(i, 2n));
  const probe = mix(seed, count); out.push(table[Number(laneOf(probe))]);
  if (mode === 'lane-index') { const lane = toInt(u64.bit_or(u64.bit_and(probe, 511n), 256n)); if (lane >= 256n) throw Error('out of bounds'); out.push(table[Number(lane)]); }
  if (mode === 'convert') out.push(toInt(u64.bit_and(u64.bit_or(probe, u64.shl(1n, 63n)), u64.bit_not(0n))));
  if (mode === 'zero-rem') { const zero = BigInt(mode.length) - BigInt(mode.length); out.push(rem(count, zero)); }
  return out.map(String).join('\n') + '\n';
}
function runMode(mode, count) {
  const out = [];
  try { return { stdout: run(count, mode, out), error: null }; }
  catch (error) { return { stdout: out.length ? out.map(String).join('\n') + '\n' : '', error: error.message }; }
}

const countIndex = process.argv.indexOf('--count'), modeIndex = process.argv.indexOf('--mode');
if (countIndex > 0 || modeIndex > 0) {
  // --mode MODE --count N: the model's stdout before the fault; the fault message on stderr (exit 1).
  const result = runMode(modeIndex > 0 ? process.argv[modeIndex + 1] : 'run', BigInt(countIndex > 0 ? process.argv[countIndex + 1] : '48'));
  process.stdout.write(result.stdout);
  if (result.error) { process.stderr.write(result.error + '\n'); process.exit(1); }
  process.exit(0);
}
const golden = run(48n);
const target = path.join(here, 'v110-soa-masks.stdout');
if (process.argv.includes('--check')) {
  const current = fs.readFileSync(target, 'utf8').replace(/\r\n/g, '\n');
  if (current !== golden) { console.error('v110-soa-masks.stdout differs from the independent model'); process.exit(1); }
  console.log(`v110-soa-masks.stdout matches the independent model (${golden.split('\n').length - 1} lines)`);
} else {
  fs.writeFileSync(target, golden);
  console.log(`wrote ${target} (${golden.split('\n').length - 1} lines)`);
}
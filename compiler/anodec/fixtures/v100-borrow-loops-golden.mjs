// Computes the known-output golden fixtures/v100-borrow-loops.stdout for the default
// run (count 48, mode "run") of fixtures/v100-borrow-loops.an with an independent
// BigInt model: int is two's-complement 64-bit with checked +,* (overflow throws),
// truncating checked / and %, intrinsics follow the 0.28 contract (wrapping modulo
// 2^width, counts (uint64)bits & (width-1), arithmetic signed shr, width-exact
// rotates), u32(...) is a checked conversion. It never runs the seed, the bootstrap
// frontend or generated C; verify.ps1 compares all of them against this golden.
// Usage (from examples/bootstrap-compiler):
//   node fixtures/v100-borrow-loops-golden.mjs          (writes the golden)
//   node fixtures/v100-borrow-loops-golden.mjs --check  (exit 1 unless identical)
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const MIN = -(1n << 63n), MAX = (1n << 63n) - 1n;
const checked = v => { if (v < MIN || v > MAX) throw Error('integer overflow'); return v; };
const add = (a, b) => checked(a + b), mul = (a, b) => checked(a * b);
const div = (a, b) => { if (b === 0n) throw Error('division by zero'); return checked(a / b); }; // BigInt / truncates
const rem = (a, b) => { if (b === 0n) throw Error('remainder by zero'); return a % b; };          // sign of dividend
const intr = (width, signed) => {
  const W = BigInt(width), mask = (1n << W) - 1n;
  const wrap = v => (signed ? BigInt.asIntN(width, v & mask) : v & mask);
  const raw = v => BigInt.asUintN(width, v);
  const count = n => BigInt.asUintN(64, n) & (W - 1n);
  return {
    wrapping_mul: (a, b) => wrap(raw(a) * raw(b)),
    bit_and: (a, b) => wrap(raw(a) & raw(b)),
    bit_xor: (a, b) => wrap(raw(a) ^ raw(b)),
    shl: (a, n) => wrap(raw(a) << count(n)),
    shr: (a, n) => { const x = raw(a), c = count(n); return wrap(signed && (x >> (W - 1n)) & 1n ? ~((~x & mask) >> c) & mask : x >> c); },
    rotl: (a, n) => { const x = raw(a), c = count(n); return wrap((x << c) | (x >> ((W - c) & (W - 1n)))); },
  };
};
const i64 = intr(64, true), u32 = intr(32, false);
const toU32 = v => { if (v < 0n || v > 0xFFFFFFFFn) throw Error('integer conversion out of range'); return v; };

const evens = values => values.filter(v => rem(v, 2n) === 0n);
const laneHash = values => {
  let h = 2166136261n;
  for (const v of values) { h = u32.wrapping_mul(u32.bit_xor(h, toU32(i64.bit_and(v, 255n))), 16777619n); h = u32.rotl(h, 5n); }
  return h;
};
const spread = values => {
  const counts = Array(8).fill(0n);
  for (let i = 0n; i < BigInt(values.length); i++) { const slot = Number(i % 8n), lane = i / 4n; counts[slot] = add(add(counts[slot], values[Number(i)]), lane); }
  return counts;
};
const probe = (values, index) => { let total = 0n; for (const v of values) total = add(total, v); if (index < 0 || index >= values.length) throw Error('out of bounds'); return add(total, values[index]); };
const digits = label => { const out = []; for (const b of Buffer.from(label, 'utf8')) { out.push(BigInt(b) % 16n, BigInt(b) / 16n); } return out; };

function run(count) {
  const out = [];
  const words = [];
  for (let i = 0n; i < count; i++) words.push(add(i64.bit_xor(mul(i, 37n), i64.shl(i, 3n)), -50n));
  const kept = [], shifted = [];
  for (const v of words) { if (v > 0n) kept.push(v); shifted.push(i64.shr(v, 2n)); }
  out.push(kept.length, shifted.length);
  let folded = 0n; for (const v of shifted) folded = i64.bit_xor(folded, i64.rotl(v, 7n)); out.push(folded);
  out.push(evens(words).length, laneHash(words));
  for (const c of spread(words)) out.push(c);
  if (words.length > 4) { const middle = words.slice(2, 2 + words.length - 4); out.push(evens(middle).length, laneHash(middle), probe(middle, 0)); }
  let codeSum = 0n; for (const v of digits('anode-1.0.0')) codeSum = add(mul(codeSum, 3n), v); out.push(codeSum);
  let negatives = 0n; for (const v of words) if (v < 0n) negatives = add(add(negatives, div(v, 4n)), rem(v, 4n)); out.push(negatives);
  return out.map(String).join('\n') + '\n';
}

const golden = run(48n);
const target = path.join(here, 'v100-borrow-loops.stdout');
if (process.argv.includes('--check')) {
  const current = fs.readFileSync(target, 'utf8').replace(/\r\n/g, '\n');
  if (current !== golden) { console.error('v100-borrow-loops.stdout differs from the independent model'); process.exit(1); }
  console.log(`v100-borrow-loops.stdout matches the independent model (${golden.split('\n').length - 1} lines)`);
} else {
  fs.writeFileSync(target, golden);
  console.log(`wrote ${target} (${golden.split('\n').length - 1} lines)`);
}

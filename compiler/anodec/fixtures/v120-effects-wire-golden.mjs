// Computes the known-output golden fixtures/v120-effects-wire.stdout for the
// default run of fixtures/v120-effects-wire.an with an independent BigInt model:
// int is two's-complement 64-bit with checked +,-,* (overflow throws) and
// truncating / and %; u16/u32/u64 intrinsics wrap modulo 2^width with rotate
// counts (uint64)bits & (width-1); u8/u16/u32/u64(...) and int(...) are checked
// conversions; a [u8;32] frame is a plain byte array. Big-endian writes follow
// contract N2 exactly: offset < 0, offset > len or len - offset < width returns
// false and leaves the frame unchanged, otherwise the value's low width bytes
// are stored most-significant first. Reads fail with "packet read out of
// bounds" on the same predicate. It never runs the seed, the bootstrap frontend,
// a reference program or generated C; verify.ps1 and the website example checker
// compare all of them against this golden.
// Usage (from examples/bootstrap-compiler):
//   node fixtures/v120-effects-wire-golden.mjs           (writes the golden)
//   node fixtures/v120-effects-wire-golden.mjs --check   (exit 1 unless identical)
//   node fixtures/v120-effects-wire-golden.mjs --args MODE [VALUE]
//     (prints the model for that command line; a checked fault prints the stdout
//      before the fault and the fault message on stderr, exit 1)
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const MIN = -(1n << 63n), MAX = (1n << 63n) - 1n;
const checked = v => { if (v < MIN || v > MAX) throw Error('integer overflow'); return v; };
const add = (a, b) => checked(a + b), sub = (a, b) => checked(a - b), mul = (a, b) => checked(a * b);
const div = (a, b) => { if (b === 0n) throw Error('division by zero'); return checked(a / b); };
const rem = (a, b) => { if (b === 0n) throw Error('remainder by zero'); return a % b; };
const convert = (v, lo, hi) => { if (v < lo || v > hi) throw Error('integer conversion out of range'); return v; };
const toInt = v => convert(v, MIN, MAX);
const toU8 = v => convert(v, 0n, 0xFFn);
const toU16 = v => convert(v, 0n, 0xFFFFn);
const toU32 = v => convert(v, 0n, 0xFFFFFFFFn);
const U64MAX = (1n << 64n) - 1n;
const toU64 = v => convert(v, 0n, U64MAX);
const u64add = (a, b) => { const r = a + b; if (r > U64MAX) throw Error('integer overflow'); return r; };
const u64mul = (a, b) => { const r = a * b; if (r > U64MAX) throw Error('integer overflow'); return r; };
const M32 = 0xFFFFFFFFn, M64 = U64MAX;
const wrapMul32 = (a, b) => (a * b) & M32;
const wrapAdd32 = (a, b) => (a + b) & M32;
const xor32 = (a, b) => (a ^ b) & M32;
const rotl32 = (x, n) => { const k = BigInt.asUintN(64, n) & 31n; return ((x << k) | (x >> ((32n - k) & 31n))) & M32; };
const wrapAdd64 = (a, b) => (a + b) & M64;
const not64 = a => ~a & M64;

function tryWriteBE(frame, offset, width, value) {
  const len = BigInt(frame.length), w = BigInt(width);
  if (offset < 0n || offset > len || len - offset < w) return false;
  for (let k = 0; k < width; k++) frame[Number(offset) + k] = Number((value >> BigInt(8 * (width - 1 - k))) & 0xFFn);
  return true;
}
function readBE(frame, offset, width) {
  const len = BigInt(frame.length), w = BigInt(width);
  if (offset < 0n || offset > len || len - offset < w) throw Error('packet read out of bounds');
  let v = 0n;
  for (let k = 0; k < width; k++) v = (v << 8n) | BigInt(frame[Number(offset) + k]);
  return v;
}
function at(frame, index) { if (index < 0n || index >= BigInt(frame.length)) throw Error('out of bounds'); return BigInt(frame[Number(index)]); }

const mix = (seq, lane) => xor32(rotl32(wrapMul32(seq, toU32(2654435761n)), add(toInt(lane), 5n)), wrapAdd32(lane, toU32(40503n)));
function checksum(frame) {
  let h = toU32(2166136261n);
  for (let i = 0n; i < 28n; i++) h = wrapMul32(xor32(h, toU32(at(frame, i))), toU32(16777619n));
  return h;
}
function encode(frame, seq, stamp) {
  const magic = tryWriteBE(frame, 0n, 2, toU16(41342n));
  const version = tryWriteBE(frame, 2n, 2, toU16(258n));
  const sequence = tryWriteBE(frame, 4n, 4, seq);
  const time = tryWriteBE(frame, 8n, 8, stamp);
  for (let i = 0n; i < 3n; i++) { if (!tryWriteBE(frame, add(16n, mul(i, 4n)), 4, mix(seq, toU32(i)))) throw Error('assertion failed'); }
  const sum = checksum(frame);
  const sealed = tryWriteBE(frame, 28n, 4, sum);
  if (!(magic && version && sequence && time && sealed)) throw Error('assertion failed');
  return frame;
}
const valid = frame => { const sum = checksum(frame); return readBE(frame, 0n, 2) === toU16(41342n) && readBE(frame, 28n, 4) === sum; };
function hex(frame, start, count) {
  const digits = '0123456789abcdef'; let out = ''; let i = start;
  while (i < add(start, count)) { const b = toInt(at(frame, i)); out += digits[Number(div(b, 16n))] + digits[Number(rem(b, 16n))]; i = add(i, 1n); }
  return out;
}
const parseInt64 = text => { if (!/^[+-]?[0-9]+$/.test(text)) throw Error('invalid integer text'); return checked(BigInt(text)); };

function run(args, out) {
  let mode = 'default', value = 0n;
  if (args.length >= 1) mode = args[0];
  if (args.length >= 2) value = parseInt64(args[1]);
  let frames = 4n; if (mode === 'run') frames = value;
  let total = toU64(0n); let last = new Array(32).fill(0);
  for (let n = 0n; n < frames; n = add(n, 1n)) {
    const frame = encode(new Array(32).fill(0), toU32(add(mul(n, 7n), 1n)), u64add(u64mul(toU64(n), toU64(1000000007n)), toU64(1700000000000n)));
    out.push(`frame ${n}`);
    out.push(hex(frame, 0n, 16n));
    out.push(readBE(frame, 4n, 4));
    out.push(readBE(frame, 8n, 8));
    out.push(readBE(frame, 16n, 4));
    out.push(readBE(frame, 20n, 4));
    out.push(readBE(frame, 24n, 4));
    out.push(readBE(frame, 28n, 4));
    out.push(valid(frame));
    total = wrapAdd64(total, toU64(readBE(frame, 28n, 4)));
    last = frame;
  }
  const probe = encode(new Array(32).fill(0), toU32(99n), toU64(5n));
  const before = hex(probe, 0n, 32n);
  const negative = tryWriteBE(probe, sub(0n, 1n), 4, toU32(1n));
  const edge = tryWriteBE(probe, 29n, 4, toU32(1n));
  const unchanged = hex(probe, 0n, 32n);
  let offset = 3n; if (mode === 'offset') offset = value;
  const moved = tryWriteBE(probe, offset, 8, not64(toU64(81985529216486895n)));
  const exact = tryWriteBE(probe, 30n, 2, toU16(65535n));
  const after = hex(probe, 0n, 32n);
  out.push(negative); out.push(edge); out.push(before === unchanged);
  out.push(moved); out.push(exact); out.push(after);
  out.push(valid(probe));
  out.push(hex(last, 16n, 16n));
  out.push(total);
  if (mode === 'read') out.push(readBE(probe, value, 4));
  if (mode === 'index') out.push(at(probe, value));
  if (mode === 'overflow') { const top = sub(9223372036854775807n, value); out.push(add(add(top, value), 1n)); }
  if (mode === 'convert') out.push(toU8(value));
}
const render = lines => (lines.length ? lines.map(String).join('\n') + '\n' : '');
export function model(args) {
  const out = [];
  try { run(args, out); return { stdout: render(out), error: null }; }
  catch (error) { return { stdout: render(out), error: error.message }; }
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const argsIndex = process.argv.indexOf('--args');
  if (argsIndex > 0) {
    const result = model(process.argv.slice(argsIndex + 1));
    process.stdout.write(result.stdout);
    if (result.error) { process.stderr.write(result.error + '\n'); process.exit(1); }
    process.exit(0);
  }
  const golden = model([]);
  if (golden.error) { console.error('default run faulted: ' + golden.error); process.exit(1); }
  const target = path.join(here, 'v120-effects-wire.stdout');
  if (process.argv.includes('--check')) {
    const current = fs.readFileSync(target, 'utf8').replace(/\r\n/g, '\n');
    if (current !== golden.stdout) { console.error('v120-effects-wire.stdout differs from the independent model'); process.exit(1); }
    console.log(`v120-effects-wire.stdout matches the independent model (${golden.stdout.split('\n').length - 1} lines)`);
  } else {
    fs.writeFileSync(target, golden.stdout);
    console.log(`wrote ${target} (${golden.stdout.split('\n').length - 1} lines)`);
  }
}

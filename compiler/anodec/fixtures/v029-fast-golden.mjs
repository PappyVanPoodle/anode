// Generates fixtures/v029-fast.an and its known-output golden fixtures/v029-fast.stdout.
// The golden is computed here with BigInt from the 0.28 intrinsic contract
// (modulo 2^width, counts (uint64)bits & (width-1), arithmetic signed shr,
// width-exact rotates) and truncating checked int division. It never runs the
// bootstrap frontend, generated C or the seed; the 0.28 seed independently
// executes the same fixture as a second oracle in verify.ps1.
// Usage (from examples/bootstrap-compiler): node fixtures/v029-fast-golden.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const types = [
  { name: 'int', width: 64, signed: true, a: ['(0-6148914691236517206)', -6148914691236517206n], b: ['1234567890123', 1234567890123n] },
  { name: 'i8', width: 8, signed: true, a: ['i8(-100)', -100n], b: ['i8(77)', 77n] },
  { name: 'i16', width: 16, signed: true, a: ['i16(-30000)', -30000n], b: ['i16(12345)', 12345n] },
  { name: 'i32', width: 32, signed: true, a: ['i32(-2000000000)', -2000000000n], b: ['i32(1234567891)', 1234567891n] },
  { name: 'u8', width: 8, signed: false, a: ['u8(200)', 200n], b: ['u8(77)', 77n] },
  { name: 'u16', width: 16, signed: false, a: ['u16(60000)', 60000n], b: ['u16(12345)', 12345n] },
  { name: 'u32', width: 32, signed: false, a: ['u32(4000000000)', 4000000000n], b: ['u32(1234567891)', 1234567891n] },
  { name: 'u64', width: 64, signed: false, a: ['bit_not(u64(81985529216486895))', 0xFEDCBA9876543210n], b: ['u64(1234567891)', 1234567891n] },
];
const out = (t, r) => (t.signed ? BigInt.asIntN(t.width, r) : BigInt.asUintN(t.width, r)).toString();
function op(t, name, a, b) {
  const W = BigInt(t.width), mask = (1n << W) - 1n, x = BigInt.asUintN(t.width, a), y = BigInt.asUintN(t.width, b ?? 0n);
  const n = BigInt.asUintN(64, b ?? 0n) & (W - 1n), back = (W - n) & (W - 1n);
  switch (name) {
    case 'wrapping_add': return (x + y) & mask;
    case 'wrapping_sub': return (x - y) & mask;
    case 'wrapping_mul': return (x * y) & mask;
    case 'bit_and': return x & y;
    case 'bit_or': return x | y;
    case 'bit_xor': return x ^ y;
    case 'bit_not': return ~x & mask;
    case 'shl': return (x << n) & mask;
    case 'shr': return t.signed && (x >> (W - 1n)) & 1n ? ~((~x & mask) >> n) & mask : x >> n;
    case 'rotl': return ((x << n) | (x >> back)) & mask;
    case 'rotr': return ((x >> n) | (x << back)) & mask;
  }
  throw Error(name);
}
const source = ['// v0.29 stage target and 0.28-seed oracle: per-width inline intrinsic helpers plus',
  '// proven non-negative power-of-two int division/remainder; unproven cases stay checked.',
  'fn main(){'];
const golden = [];
for (const t of types) {
  source.push(`let a_${t.name}=${t.a[0]};let b_${t.name}=${t.b[0]};`);
  for (const name of ['wrapping_add', 'wrapping_sub', 'wrapping_mul', 'bit_and', 'bit_or', 'bit_xor']) {
    source.push(`print ${name}(a_${t.name},b_${t.name});`); golden.push(out(t, op(t, name, t.a[1], t.b[1])));
  }
  source.push(`print bit_not(a_${t.name});`); golden.push(out(t, op(t, 'bit_not', t.a[1])));
  for (const count of [3, -1, t.width + 5, -(t.width + 3)]) {
    for (const name of ['shl', 'shr', 'rotl', 'rotr']) {
      source.push(`print ${name}(a_${t.name},${count});`); golden.push(out(t, op(t, name, t.a[1], BigInt(count))));
    }
  }
}
// Power-of-two division/remainder. b=97 ('a'), n=10.
const b = 97n, n = 10n, big = 9223372036854775807n;
const pow2 = [
  ['let text="anode-v029";let b=byte_at(text,0);let n=len(text);', []],
  ['print b%16;', [b % 16n]], ['print b/16;', [b / 16n]], ['print b%1;', [b % 1n]], ['print b/1;', [b / 1n]],
  ['print b%64;', [b % 64n]], ['print b/4;', [b / 4n]], ['print n%8;', [n % 8n]], ['print n/2;', [n / 2n]],
  ['print (b+n)%32;', [(b + n) % 32n]], ['print (b*n)/128;', [(b * n) / 128n]], ['print ((b%16)+1)%4;', [((b % 16n) + 1n) % 4n]],
  ['let big=9223372036854775807;print big%1024;print big/4611686018427387904;', [big % 1024n, big / 4611686018427387904n]],
  ['for i in 0..9 { print i%4;print i/8; }', [...Array(9).keys()].flatMap(i => [BigInt(i) % 4n, BigInt(i) / 8n])],
  ['let total=char_count(text)*3;print total%8;', [(10n * 3n) % 8n]],
  // Unproven: mutable binding, negative dividend, non power-of-two, narrow type.
  ['let mut moved=b;moved=moved+1;print moved%16;', [(b + 1n) % 16n]],
  ['let negative=0-b;print negative%16;print negative/16;', [(-b) % 16n, (-b) / 16n]],
  ['print (0-7)%4;print (0-7)/4;', [-7n % 4n, -7n / 4n]],
  ['print b%6;print b/6;', [b % 6n, b / 6n]],
  ['let small=u8(200);print small%u8(16);print small/u8(16);', [200n % 16n, 200n / 16n]],
  ['if args_count()>0 { let mode=arg(0);let zero=len(mode)-len(mode);if text_eq(mode,"zero-rem") { print b%zero; } else { print b/zero; } }', []],
];
for (const [line, values] of pow2) { source.push(line); for (const v of values) golden.push(v.toString()); }
source.push('}');
fs.writeFileSync(path.join(here, 'v029-fast.an'), source.join('\n') + '\n');
fs.writeFileSync(path.join(here, 'v029-fast.stdout'), golden.join('\n') + '\n');
console.log(`wrote ${source.length} source lines, ${golden.length} golden outputs`);

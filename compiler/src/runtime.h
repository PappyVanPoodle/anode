#ifndef ZENITH_RUNTIME_H
#define ZENITH_RUNTIME_H
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include <string.h>
typedef struct { char* data; size_t len; } zn_text;
/* Private compiler literals opt into tagged ownership. Existing constructors
   still return plain owned lengths. Direct C clients of the literal API must
   use zn_text_size, or materialize a plain owner with zn_to_text/zn_slice. */
#define ZN_TEXT_STATIC_TAG ((size_t)UINT64_C(0x8000000000000000))
static __forceinline size_t zn_text_size(zn_text t){return t.len & ~ZN_TEXT_STATIC_TAG;}
/* Bits cross the native compiler's internal scalar ABI; memcpy avoids aliasing. */
static __forceinline uint64_t zn_f32_bits(float v){uint32_t b;memcpy(&b,&v,4);return b;}
static __forceinline uint64_t zn_f64_bits(double v){uint64_t b;memcpy(&b,&v,8);return b;}
static __forceinline float zn_f32_value(uint64_t raw){uint32_t b=(uint32_t)raw;float v;memcpy(&v,&b,4);return v;}
static __forceinline double zn_f64_value(uint64_t b){double v;memcpy(&v,&b,8);return v;}
uint64_t zn_fp_convert(uint64_t raw,uint64_t from,uint64_t to);
uint64_t zn_fp_remainder(uint64_t a,uint64_t b,uint64_t code);
uint64_t zn_fp_sqrt(uint64_t raw,uint64_t code);
uint64_t zn_fp_abs(uint64_t raw,uint64_t code);
bool zn_fp_is_finite(uint64_t raw,uint64_t code);
bool zn_fp_is_nan(uint64_t raw,uint64_t code);
void zn_print_float(uint64_t raw,uint64_t code);
zn_text zn_float_text(uint64_t raw,uint64_t code);
bool zn_is_f64(zn_text t);
double zn_parse_f64(zn_text t);
typedef struct { void* data; uint64_t len; } zn_span;
void zn_span_region(zn_span* out,void* data,uint64_t len,int64_t start,int64_t end,uint64_t width);
zn_span zn_span_make(void* data,uint64_t len,int64_t start,int64_t end,uint64_t width);
/* A span never owns its pointer. The compiler prohibits drops and escapes. */
void zn_span_drop(zn_span* p);
/* Nominal vector views use start/count, preserving legacy span start/end. */
void zn_vector_view_region(zn_span* out,void* data,uint64_t len,int64_t start,int64_t count,uint64_t width);
zn_span zn_vector_view_make(void* data,uint64_t len,int64_t start,int64_t count,uint64_t width);
void* zn_span_slot(const zn_span* a,int64_t index,uint64_t width);
void zn_span_check_pair(const zn_span* a,int64_t left,int64_t right);
typedef struct { void* data; size_t len; size_t cap; } zn_vec;
typedef struct zn_channel_state zn_channel_state;
typedef struct { zn_channel_state* state; } zn_channel;
/* Shared endpoint owners; the callback destroys one moved element in place. */
void zn_channel_new(zn_channel* out,int64_t capacity,uint64_t width,void (*destroy)(void*));
void zn_channel_clone(zn_channel* out,const zn_channel* source);
void zn_channel_drop(zn_channel* owner);
bool zn_channel_send(const zn_channel* owner,void* moved_value);
bool zn_channel_recv(const zn_channel* owner,void* out,bool blocking);
void zn_channel_close(const zn_channel* owner);
bool zn_channel_is_closed(const zn_channel* owner);
int64_t zn_channel_len(const zn_channel* owner);
int64_t zn_channel_capacity(const zn_channel* owner);
void* zn_vec_push_slot(zn_vec* a, uint64_t width);
void* zn_vec_slot(const zn_vec* a, int64_t index, uint64_t width);
void* zn_vec_pop_slot(zn_vec* a, uint64_t width);
void zn_vec_clone_storage(zn_vec* out, const zn_vec* a, uint64_t width);
void zn_vec_release(zn_vec* a);
/* SoA layout is pairs of (record byte offset, field byte width). Each column
   starts at an 8-byte boundary and has cap contiguous field values. */
void* zn_soa_column_slot(const zn_vec* a,int64_t i,uint64_t column,uint64_t count,const uint64_t* fields);
void zn_soa_load(void* out,const zn_vec* a,int64_t i,uint64_t count,const uint64_t* fields);
void zn_soa_store(zn_vec* a,int64_t i,const void* value,uint64_t count,const uint64_t* fields);
int64_t zn_soa_push_index(zn_vec* a,uint64_t count,const uint64_t* fields);
void zn_soa_pop(void* out,zn_vec* a,uint64_t count,const uint64_t* fields);
void zn_soa_clone_storage(zn_vec* out,const zn_vec* a,uint64_t count,const uint64_t* fields);
void zn_vec_check_pair(const zn_vec* a,int64_t left,int64_t right);

/* v0.21 nominal maps: opaque 40-byte owner, deterministic open addressing.
   Entry key starts at byte 0; the compiler supplies padded entry width and
   typed value offset. key_code=0 is text, otherwise Integer::code().
   Runtime helpers manage storage only; generated code owns key/value drops. */
typedef struct { void* data; unsigned char* states; uint64_t len; uint64_t cap; uint64_t tombstones; } zn_map;
void* zn_map_find(const zn_map* m,const void* key,uint64_t key_code,uint64_t width);
void* zn_map_insert_slot(zn_map* m,const void* key,uint64_t key_code,uint64_t width,bool* replaced);
void zn_map_erase_slot(zn_map* m,void* slot,uint64_t width);
void zn_map_clone_storage(zn_map* out,const zn_map* source,uint64_t width);
void zn_map_release(zn_map* m);
void* zn_map_next_slot(const zn_map* m,uint64_t* cursor,uint64_t width);
void* zn_map_slot_at(const zn_map* m,uint64_t index,uint64_t width);
/* Instance-relative 32-bit slot / nonzero 32-bit generation identifiers.
   Occupancy 0=free, 1=live, 2=permanently retired. Generation never wraps. */
typedef struct { void* data; uint32_t* generations; unsigned char* states; uint64_t len; uint64_t slots; uint64_t cap; } zn_arena;
_Static_assert(sizeof(zn_text)==16 && offsetof(zn_text,data)==0 && offsetof(zn_text,len)==8,"text ABI");
_Static_assert(sizeof(zn_span)==16 && offsetof(zn_span,data)==0 && offsetof(zn_span,len)==8,"span ABI");
_Static_assert(sizeof(zn_vec)==24 && offsetof(zn_vec,data)==0 && offsetof(zn_vec,len)==8 && offsetof(zn_vec,cap)==16,"vector ABI");
_Static_assert(sizeof(zn_arena)==48 && offsetof(zn_arena,data)==0 && offsetof(zn_arena,generations)==8 && offsetof(zn_arena,states)==16 && offsetof(zn_arena,len)==24 && offsetof(zn_arena,slots)==32 && offsetof(zn_arena,cap)==40,"arena ABI");
_Static_assert(sizeof(zn_map)==40 && offsetof(zn_map,states)==8 && offsetof(zn_map,len)==16 && offsetof(zn_map,cap)==24 && offsetof(zn_map,tombstones)==32,"map ABI");
_Static_assert(__alignof(zn_text)==8 && __alignof(zn_span)==8 && __alignof(zn_vec)==8 && __alignof(zn_arena)==8 && __alignof(zn_map)==8,"descriptor alignment ABI");
uint64_t zn_arena_alloc_slot(zn_arena* a,uint64_t width);
bool zn_arena_contains_id(const zn_arena* a,uint64_t id);
void* zn_arena_get(const zn_arena* a,uint64_t id,uint64_t width);
void zn_arena_erase_slot(zn_arena* a,uint64_t id,uint64_t width);
void zn_arena_check_pair(const zn_arena* a,uint64_t left,uint64_t right);
void zn_arena_clone_storage(zn_arena* out,const zn_arena* source,uint64_t width);
void zn_arena_release(zn_arena* a);
void* zn_arena_next_slot(const zn_arena* a,uint64_t* cursor,uint64_t width);
void* zn_arena_slot_at(const zn_arena* a,uint64_t index,uint64_t width);
typedef struct { int64_t* data; size_t len; size_t cap; } zn_ints;
typedef struct { zn_text* data; size_t len; size_t cap; } zn_texts;
typedef struct { uint64_t tag; zn_text value; } zn_io_result;
typedef struct { int64_t status; zn_text stdout_text; zn_text stderr_text; } zn_process_output;
typedef struct { uint64_t tag; union { zn_process_output output; zn_text error; } data; } zn_process_result;
/* Direct subprocess capture: 16 MiB/stream, 30s total, strict UTF8/no NUL. */
zn_process_result zn_process_capture(zn_text program,const zn_text* args,uint64_t count);
void zn_abi_process_run(void* out,const zn_text* program,const zn_text* args,uint64_t count);
void zn_texts_drop(zn_texts* a);
void zn_texts_push(zn_texts* a, zn_text value);
zn_texts zn_texts_clone(zn_texts a);
zn_text zn_text_at(zn_texts a, int64_t index);
zn_texts zn_split(zn_text value, zn_text separator);
zn_io_result zn_read_result(zn_text path);
zn_io_result zn_write_result(zn_text path, zn_text value);
typedef struct zn_task zn_task;
__declspec(noreturn) void zn_fail(const char* message);
void* zn_alloc(size_t n);
void zn_release(void* p);
void zn_assert(bool condition, int line, const char* message);
zn_text zn_text_make(const char* s);
/* Compiler-validated permanent immutable UTF8, exactly bytes followed by NUL. */
zn_text zn_text_literal(const char* s,size_t bytes);
void zn_abi_text_literal(zn_text* out,const char* s,uint64_t bytes);
zn_text zn_text_parts(const char* const* parts, size_t count);
void zn_text_drop(zn_text* t);
zn_text zn_text_clone(zn_text t);
/* Immutable, length-delimited borrowed UTF-8. Never frees the source. */
zn_text zn_text_view_make(zn_text source, int64_t start, int64_t end);
void zn_text_view_drop(zn_text* t);
zn_text zn_to_text(zn_text t);
bool zn_view_contains(zn_text a, zn_text b);
void zn_print_text_view(zn_text t);
zn_text zn_concat(zn_text a, zn_text b);
void zn_ints_drop(zn_ints* a);
void zn_ints_grow(zn_ints* a);
zn_ints zn_ints_clone(zn_ints a);
zn_task* zn_start(int64_t (*work)(void*), void* args);
int64_t zn_join(zn_task* t);
/* A non-null packet invokes work; a null packet destroys its typed result.
   The runtime owns both packet/result lifetimes, never a caller stack address. */
zn_task* zn_start_owned(void (*work)(void*,void*), void* packet, uint64_t width);
void zn_join_owned(void* out, zn_task* t, uint64_t width);
void zn_task_drop(zn_task** t);
/* Drain workers and destroy unjoined results. Call at a quiescent host boundary;
   automatic process-exit cleanup is disabled for unloadable reload runtimes. */
void zn_task_pool_shutdown(void);
int64_t zn_monotonic_ms(void);
bool zn_file_exists(zn_text path);
zn_texts zn_list_dir(zn_text path);
void zn_exit_code(int64_t code);
int zn_process_exit_code(void);
void zn_print_int(int64_t v);
void zn_print_uint(uint64_t v);
int64_t zn_num_signed(uint64_t raw);
uint64_t zn_num_cast(uint64_t raw, uint64_t from, uint64_t to);
uint64_t zn_num_binary(uint64_t a, uint64_t b, uint64_t type, uint64_t op);
void zn_fixed_fill(void* data, uint64_t value, uint64_t len, uint64_t code);
uint64_t zn_fixed_at(const void* data, uint64_t len, int64_t index, uint64_t code);
bool zn_fixed_try_set(void* data, uint64_t len, int64_t index, uint64_t code, uint64_t value);
void zn_fixed_set(void* data, uint64_t len, int64_t index, uint64_t code, uint64_t value);
uint64_t zn_wire_read(const void* data, uint64_t len, int64_t offset, uint64_t width);
bool zn_wire_write(void* data, uint64_t len, int64_t offset, uint64_t width, uint64_t value);
void zn_print_bool(bool v);
void zn_print_text(zn_text t);
void zn_args_init(int argc, wchar_t** argv);
int64_t zn_args_count(void);
zn_text zn_arg(int64_t i);
/* Compiler-private process-test dispatch; never a source builtin. Consumes and
   hides the sole selector argument before user test code starts. */
int64_t zn_internal_test_selector(void);
int64_t zn_abi_internal_test_selector(void);
zn_text zn_read_text(zn_text path);
void zn_write_text(zn_text path, zn_text data);
bool zn_text_eq(zn_text a, zn_text b);
bool zn_contains(zn_text a, zn_text b);
int64_t zn_char_count(zn_text t);
bool zn_is_char_boundary(zn_text t, int64_t offset);
int64_t zn_codepoint_at(zn_text t, int64_t offset);
int64_t zn_next_char_boundary(zn_text t, int64_t offset);
zn_text zn_codepoint_text(int64_t codepoint);
int64_t zn_text_find(zn_text t, zn_text needle, int64_t start);
bool zn_text_starts_with(zn_text t, zn_text prefix);
bool zn_text_ends_with(zn_text t, zn_text suffix);
zn_text zn_text_trim(zn_text t);
zn_text zn_text_replace(zn_text t, zn_text needle, zn_text replacement);
zn_texts zn_text_lines(zn_text t);
zn_text zn_text_join(zn_texts parts, zn_text separator);
zn_text zn_slice(zn_text t, int64_t start, int64_t end);
int64_t zn_parse_int(zn_text t);
bool zn_is_int(zn_text t);
zn_text zn_int_text(int64_t value);
/* Compact checked paths for cold code; same operations, shared machine code. */
int64_t zn_cold_add(int64_t a, int64_t b);
int64_t zn_cold_sub(int64_t a, int64_t b);
int64_t zn_cold_mul(int64_t a, int64_t b);
int64_t zn_cold_mul_literal(int64_t a, int64_t b);
int64_t zn_cold_div(int64_t a, int64_t b);
int64_t zn_cold_mod(int64_t a, int64_t b);
int64_t zn_cold_neg(int64_t a);

/* Small checked operations live beside generated code so the native optimizer
   can inline them and prove redundant checks. No unchecked variant exists.
   Allocation, I/O, failure reporting and thread management remain out of line. */
#define ZN_INLINE static __forceinline
/* Byte access is a checked load, like array access. Keeping it visible lets
   the backend optimize text-scanning loops without an ABI call per byte. */
ZN_INLINE int64_t zn_byte_at(zn_text t, int64_t i) {
    if ((uint64_t)i >= zn_text_size(t)) zn_fail("text index out of bounds");
    return (unsigned char)t.data[(size_t)i];
}
/* Most appends have spare capacity. Keep only allocation/growth out of line. */
ZN_INLINE void zn_push(zn_ints* a, int64_t v) {
    if (a->len == a->cap) zn_ints_grow(a);
    a->data[a->len++] = v;
}
/* MSVC x64 intrinsic: a full signed 128-bit product, without C signed overflow.
   Declare only the documented signature instead of importing unrelated SIMD headers. */
__int64 _mul128(__int64 a, __int64 b, __int64* high);
#pragma intrinsic(_mul128)
ZN_INLINE int64_t zn_length(size_t n) { if (n > INT64_MAX) zn_fail("length exceeds int range"); return (int64_t)n; }
#ifdef ZN_OUTLINE_ARITHMETIC
/* Fast builds reuse the same checked helpers in the cached native runtime. */
#define zn_add zn_cold_add
#define zn_sub zn_cold_sub
#define zn_neg zn_cold_neg
#define zn_mul zn_cold_mul
#define zn_mul_literal zn_cold_mul_literal
#define zn_div zn_cold_div
#define zn_mod zn_cold_mod
#else
ZN_INLINE int64_t zn_signed_bits(uint64_t bits) { int64_t value; memcpy(&value, &bits, sizeof(value)); return value; }
ZN_INLINE int64_t zn_add(int64_t a, int64_t b) {
    uint64_t x = (uint64_t)a, y = (uint64_t)b, sum = x + y;
    if (((sum ^ x) & (sum ^ y)) >> 63) zn_fail("integer overflow");
    return zn_signed_bits(sum);
}
ZN_INLINE int64_t zn_sub(int64_t a, int64_t b) {
    uint64_t x = (uint64_t)a, y = (uint64_t)b, difference = x - y;
    if (((x ^ y) & (x ^ difference)) >> 63) zn_fail("integer overflow");
    return zn_signed_bits(difference);
}
ZN_INLINE int64_t zn_neg(int64_t a) { if (a == INT64_MIN) zn_fail("integer overflow"); return -a; }
ZN_INLINE int64_t zn_mul(int64_t a, int64_t b) {
    __int64 high;
    __int64 low = _mul128(a, b, &high);
    if (high != (low < 0 ? -1 : 0)) zn_fail("integer overflow");
    return low;
}
/* This form lets the backend fold division thresholds when the source operand
   is a literal. It is also fully checked if called with a nonconstant value. */
ZN_INLINE int64_t zn_mul_literal(int64_t a, int64_t b) {
    if (a > 0) { if ((b > 0 && a > INT64_MAX/b) || (b < 0 && b < INT64_MIN/a)) zn_fail("integer overflow"); }
    else if (a < 0) { if ((b > 0 && a < INT64_MIN/b) || (b < 0 && b < INT64_MAX/a)) zn_fail("integer overflow"); }
    return a*b;
}
ZN_INLINE int64_t zn_div(int64_t a, int64_t b) { if (!b) zn_fail("division by zero"); if (a == INT64_MIN && b == -1) zn_fail("integer overflow"); return a/b; }
/* MSVC 14.44 /O2 can lower an inlined signed `x % 2^k` as `(x & (2^k-1)) - 2^k` for every
   negative x, which is wrong when x is a multiple of 2^k (-256 % 256 gave -256). Divisors whose
   magnitude is a power of two (constant-folded here) therefore never reach a signed `%`:
   bias = 2^k-1 for negative a, so ((a + bias) & mask) - bias is the truncating remainder. */
ZN_INLINE int64_t zn_mod(int64_t a, int64_t b) {
    if (!b) zn_fail("remainder by zero");
    if (a == INT64_MIN && b == -1) zn_fail("integer overflow");
    uint64_t magnitude = b < 0 ? 0 - (uint64_t)b : (uint64_t)b;
    if (!(magnitude & (magnitude - 1))) {
        uint64_t mask = magnitude - 1, bias = (uint64_t)(a >> 63) & mask;
        return zn_signed_bits((((uint64_t)a + bias) & mask) - bias);
    }
    return a%b;
}
#endif
/* Array lengths cannot exceed SIZE_MAX / sizeof(int64_t), which is below
   INT64_MAX on this x64 backend. Converting a negative index to uint64_t
   therefore fails this SAME full-range check; no separate sign branch is needed. */
ZN_INLINE int64_t zn_at(const zn_ints* a, int64_t i) { if ((uint64_t)i >= a->len) zn_fail("array index out of bounds"); return a->data[(size_t)i]; }
ZN_INLINE void zn_set(zn_ints* a, int64_t i, int64_t v) { if ((uint64_t)i >= a->len) zn_fail("array index out of bounds"); a->data[(size_t)i] = v; }
#undef ZN_INLINE
/* v1.2 Frontier 3 (V1200 N2-N6). Only the 1.2 builtins call these helpers.
   Big-endian wire write: value comes BEFORE width (the reverse of zn_wire_write).
   width must be 2, 4 or 8 ("invalid packet width"); data may be NULL only when
   len == 0 ("packet storage overflow"). Out of range: false, buffer untouched. */
bool zn_wire_write_be(void* data, uint64_t len, int64_t offset, uint64_t value, uint64_t width);
/* Exact 64-bit FNV-1a over len bytes. data may be NULL only when len == 0. */
uint64_t zn_fnv1a_64(const void* data, uint64_t len);
/* FIPS 180-4 SHA-256 as a fresh owned 64-byte lowercase-hex text. */
zn_text zn_sha256_bytes(const void* data, uint64_t len);
void zn_abi_sha256_bytes(zn_text* out, const void* data, uint64_t len);
/* NetResult tags use the Anode enum encoding (variant index + 1), exactly like
   zn_process_result: 1 = Response, 2 = Error. Arguments are borrowed; every
   result text is fresh, NUL-terminated and owned by the caller. */
typedef struct { int64_t status; zn_text body; } zn_net_response;
typedef struct { uint64_t tag; union { zn_net_response response; zn_text error; } data; } zn_net_result;
_Static_assert(sizeof(zn_net_response)==24 && sizeof(zn_net_result)==32 && offsetof(zn_net_result,data)==8,"net result ABI");
zn_net_result zn_net_request(zn_text endpoint, zn_text payload);
void zn_abi_net_request(void* out, const zn_text* endpoint, const zn_text* payload);
/* zn-v130-begin: 1.3 OS/runtime primitives. Only the 1.3 builtins call these
   helpers, and the C backend embeds this block only in a program that uses one,
   so the emitted C of every other program is unchanged. Paths and names are
   borrowed UTF-8 texts; an interior NUL or invalid UTF-8 fails without I/O.
   Byte transfers are limited to 16 MiB. */
int64_t zn_try_read_bytes(zn_text path, void* dst, uint64_t cap);
bool zn_try_write_bytes(zn_text path, const void* src, uint64_t len);
zn_text zn_env_var(zn_text name);
bool zn_create_dir(zn_text path);
bool zn_remove_file(zn_text path);
zn_net_result zn_net_serve_once(zn_text endpoint, int64_t status, zn_text reply_body);
int64_t zn_abi_try_read_bytes(const zn_text* path, void* dst, uint64_t cap);
bool zn_abi_try_write_bytes(const zn_text* path, const void* src, uint64_t len);
void zn_abi_env_var(zn_text* out, const zn_text* name);
bool zn_abi_create_dir(const zn_text* path);
bool zn_abi_remove_file(const zn_text* path);
void zn_abi_net_serve_once(void* out, const zn_text* endpoint, int64_t status, const zn_text* reply_body);
/* keys(m) on text- or int-keyed maps (width = entry size, key first) and
   http_request over http:// or https:// through WinHTTP. */
zn_texts zn_map_keys_text(const zn_map* m, uint64_t width);
zn_ints zn_map_keys_int(const zn_map* m, uint64_t width);
void zn_abi_map_keys_text(zn_texts* out, const zn_map* m, uint64_t width);
void zn_abi_map_keys_int(zn_ints* out, const zn_map* m, uint64_t width);
zn_net_result zn_http_request(zn_text url, zn_text body);
void zn_abi_http_request(void* out, const zn_text* url, const zn_text* body);
/* zn-v130-end */
#endif

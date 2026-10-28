#ifndef ZENITH_RUNTIME_H
#define ZENITH_RUNTIME_H
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include <string.h>
typedef struct { char* data; size_t len; } zn_text;
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
typedef struct { void* data; size_t len; size_t cap; } zn_vec;
void* zn_vec_push_slot(zn_vec* a, uint64_t width);
void* zn_vec_slot(const zn_vec* a, int64_t index, uint64_t width);
void* zn_vec_pop_slot(zn_vec* a, uint64_t width);
void zn_vec_clone_storage(zn_vec* out, const zn_vec* a, uint64_t width);
void zn_vec_release(zn_vec* a);

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
typedef struct { int64_t* data; size_t len; size_t cap; } zn_ints;
typedef struct { zn_text* data; size_t len; size_t cap; } zn_texts;
typedef struct { uint64_t tag; zn_text value; } zn_io_result;
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
    if ((uint64_t)i >= t.len) zn_fail("text index out of bounds");
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
ZN_INLINE int64_t zn_mod(int64_t a, int64_t b) { if (!b) zn_fail("remainder by zero"); if (a == INT64_MIN && b == -1) zn_fail("integer overflow"); return a%b; }
#endif
/* Array lengths cannot exceed SIZE_MAX / sizeof(int64_t), which is below
   INT64_MAX on this x64 backend. Converting a negative index to uint64_t
   therefore fails this SAME full-range check; no separate sign branch is needed. */
ZN_INLINE int64_t zn_at(const zn_ints* a, int64_t i) { if ((uint64_t)i >= a->len) zn_fail("array index out of bounds"); return a->data[(size_t)i]; }
ZN_INLINE void zn_set(zn_ints* a, int64_t i, int64_t v) { if ((uint64_t)i >= a->len) zn_fail("array index out of bounds"); a->data[(size_t)i] = v; }
#undef ZN_INLINE
#endif

// Backend hints only; safety and cleanup are already fixed in checked IR.
fn inline_access_helpers() -> &'static str {
    r#"
/* Exact-width modular arithmetic: all intermediate arithmetic is unsigned. */
static __forceinline int64_t zn_v028_wrap_signed(uint64_t raw,unsigned width){
    uint64_t mask=width==64?UINT64_MAX:(UINT64_C(1)<<width)-1;
    raw&=mask;
    return (raw&(UINT64_C(1)<<(width-1))) ? -1-(int64_t)((~raw)&mask) : (int64_t)raw;
}
static __forceinline uint64_t zn_v028_intrinsic(uint64_t a,uint64_t b,unsigned width,bool sign,unsigned op){
    uint64_t mask=width==64?UINT64_MAX:(UINT64_C(1)<<width)-1;
    unsigned count=(unsigned)(b&(width-1)),reverse=(width-count)&(width-1);
    a&=mask;
    switch(op){
    case 0:return (a+b)&mask;
    case 1:return (a-b)&mask;
    case 2:return (a*b)&mask;
    case 3:return a&(b&mask);
    case 4:return (a|b)&mask;
    case 5:return (a^b)&mask;
    case 6:return (~a)&mask;
    case 7:return (a<<count)&mask;
    case 8:return (a>>count)|((sign && count && (a&(UINT64_C(1)<<(width-1))))?(mask^(mask>>count)):0);
    case 9:return ((a<<count)|(a>>reverse))&mask;
    default:return ((a>>count)|(a<<reverse))&mask;
    }
}
static __forceinline void* zn_v028_vec_slot(const zn_vec* a,int64_t index,uint64_t width){
    if((uint64_t)index>=a->len)zn_fail("vector index out of bounds");
    if(!width || (uint64_t)index>INT64_MAX || (uint64_t)index>SIZE_MAX/width)zn_fail("vector capacity overflow");
    return (unsigned char*)a->data+(uint64_t)index*width;
}
static __forceinline void* zn_v028_span_slot(const zn_span* a,int64_t index,uint64_t width){
    if(index<0 || (uint64_t)index>=a->len)zn_fail("vector region index out of bounds");
    if(!width || a->len>SIZE_MAX/width || !a->data)zn_fail("vector region storage overflow");
    return (unsigned char*)a->data+(uint64_t)index*width;
}
static __forceinline bool zn_v028_arena_contains(const zn_arena* a,uint64_t id){
    uint64_t slot=(uint32_t)id;uint32_t generation=(uint32_t)(id>>32);
    return generation!=0 && slot<a->slots && a->states[slot]==1 && a->generations[slot]==generation;
}
static __forceinline void* zn_v028_arena_slot(const zn_arena* a,uint64_t id,uint64_t width){
    if(!zn_v028_arena_contains(a,id))zn_fail("stale or invalid arena handle");
    return (unsigned char*)a->data+(uint64_t)(uint32_t)id*width;
}
static __forceinline uint64_t zn_v028_fixed_at(const void* data,uint64_t len,int64_t index,uint64_t code){
    if((uint64_t)index>=len)zn_fail("array index out of bounds");
    unsigned bits=(unsigned)(code>100?code-100:code);uint64_t value=0;
    memcpy(&value,(const unsigned char*)data+(uint64_t)index*(bits/8),bits/8);
    if(code<100 && bits<64 && (value&(UINT64_C(1)<<(bits-1))))value|=UINT64_MAX<<bits;
    return value;
}
static __forceinline bool zn_v028_fixed_try_set(void* data,uint64_t len,int64_t index,uint64_t code,uint64_t value){
    if((uint64_t)index>=len)return false;
    unsigned bits=(unsigned)(code>100?code-100:code);
    memcpy((unsigned char*)data+(uint64_t)index*(bits/8),&value,bits/8);return true;
}
static __forceinline void zn_v028_fixed_set(void* data,uint64_t len,int64_t index,uint64_t code,uint64_t value){
    if(!zn_v028_fixed_try_set(data,len,index,code,value))zn_fail("array index out of bounds");
}
static __forceinline uint64_t zn_v028_wire_read_be(const void* data,uint64_t len,int64_t offset,uint64_t width){
    if(width>len || (uint64_t)offset>len-width)zn_fail("packet read out of bounds");
    uint64_t value=0;const unsigned char* bytes=(const unsigned char*)data+(uint64_t)offset;
    for(unsigned i=0;i<width;i++)value=(value<<8)|bytes[i];return value;
}
"#
}

/// The eight checked integer widths, in a stable order for generated output.
const V029_INTEGERS: [crate::numeric::Integer; 8] = [
    crate::numeric::Integer::I8,
    crate::numeric::Integer::I16,
    crate::numeric::Integer::I32,
    crate::numeric::Integer::I64,
    crate::numeric::Integer::U8,
    crate::numeric::Integer::U16,
    crate::numeric::Integer::U32,
    crate::numeric::Integer::U64,
];
/// Exact source names of the 0.28 integer intrinsics, indexed by
/// `syntax::integer_intrinsic`.
const V029_INTRINSICS: [&str; 11] = [
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bit_not",
    "shl",
    "shr",
    "rotl",
    "rotr",
];

/// Width-specialized private helpers for 0.29. Every arithmetic step uses an
/// unsigned type at least as wide as `unsigned int` (no promotion to signed
/// int, so no signed overflow), counts are `(uint64_t)bits & (width-1)`, and
/// signed results are restored explicitly from their exact-width bit pattern.
/// The checked variants keep the exact runtime fault strings; the `load` and
/// `store` variants are emitted only after a whole-function range proof.
fn v029_helpers() -> String {
    let mut out = String::from(
        "\n/* 0.29 width-specialized inline helpers (private, generated only). */\n#include <stdlib.h>\nunsigned char __cdecl _rotl8(unsigned char value,unsigned char shift);\nunsigned short __cdecl _rotl16(unsigned short value,unsigned char shift);\nunsigned char __cdecl _rotr8(unsigned char value,unsigned char shift);\nunsigned short __cdecl _rotr16(unsigned short value,unsigned char shift);\n#pragma intrinsic(_rotl8,_rotl16,_rotr8,_rotr16,_rotl,_rotr,_rotl64,_rotr64,_byteswap_ushort,_byteswap_ulong,_byteswap_uint64)\n",
    );
    for number in V029_INTEGERS {
        let w = number.bits();
        let t = number.name();
        let c = number.c();
        let u = format!("uint{w}_t");
        // Never narrower than unsigned int: uint8/16 operands would otherwise
        // promote to signed int, where 65535*65535 overflows.
        let wide = if w == 64 { "uint64_t" } else { "uint32_t" };
        let restore = |raw: &str| {
            if number.signed() {
                format!("zn_v029_sign_{t}(({u})({raw}))")
            } else {
                format!("({u})({raw})")
            }
        };
        if number.signed() {
            let restored = if w == 64 {
                "(r>>63)?-1-(int64_t)~r:(int64_t)r".to_string()
            } else {
                format!("(r>>{})?({c})(-1-(int32_t)({u})~r):({c})r", w - 1)
            };
            out.push_str(&format!(
                "static __forceinline {c} zn_v029_sign_{t}({u} r){{return {restored};}}\n"
            ));
        }
        let x = format!("({wide})({u})a");
        let y = format!("({wide})({u})b");
        let count = format!("unsigned n=(unsigned)((uint64_t)bits&{});", w - 1);
        let (rotl, rotr, rotate_cast, count_cast) = match w {
            8 => ("_rotl8", "_rotr8", "unsigned char", "unsigned char"),
            16 => ("_rotl16", "_rotr16", "unsigned short", "unsigned char"),
            32 => ("_rotl", "_rotr", "unsigned int", "int"),
            _ => ("_rotl64", "_rotr64", "unsigned __int64", "int"),
        };
        let shr = if number.signed() {
            // Branch-free arithmetic shift on the unsigned representation:
            // s is all ones for negative values, so ~((~x)>>n) sign-fills.
            format!(
                "{u} x=({u})a;{u} s=({u})(({wide})0-({wide})(x>>{}));return {};",
                w - 1,
                restore(&format!("((({wide})(x^s))>>n)^s"))
            )
        } else {
            format!("return ({u})(({u})a>>n);")
        };
        for (op, body) in [
            (
                "wrapping_add",
                format!("return {};", restore(&format!("{x}+{y}"))),
            ),
            (
                "wrapping_sub",
                format!("return {};", restore(&format!("{x}-{y}"))),
            ),
            (
                "wrapping_mul",
                format!("return {};", restore(&format!("{x}*{y}"))),
            ),
            (
                "bit_and",
                format!("return {};", restore(&format!("{x}&{y}"))),
            ),
            (
                "bit_or",
                format!("return {};", restore(&format!("{x}|{y}"))),
            ),
            (
                "bit_xor",
                format!("return {};", restore(&format!("{x}^{y}"))),
            ),
        ] {
            out.push_str(&format!(
                "static __forceinline {c} zn_v029_{op}_{t}({c} a,{c} b){{{body}}}\n"
            ));
        }
        out.push_str(&format!(
            "static __forceinline {c} zn_v029_bit_not_{t}({c} a){{return {};}}\n",
            restore(&format!("~{x}"))
        ));
        for (op, body) in [
            ("shl", format!("return {};", restore(&format!("{x}<<n")))),
            ("shr", shr),
            (
                "rotl",
                format!(
                    "return {};",
                    restore(&format!("{rotl}(({rotate_cast})({u})a,({count_cast})n)"))
                ),
            ),
            (
                "rotr",
                format!(
                    "return {};",
                    restore(&format!("{rotr}(({rotate_cast})({u})a,({count_cast})n)"))
                ),
            ),
        ] {
            out.push_str(&format!(
                "static __forceinline {c} zn_v029_{op}_{t}({c} a,int64_t bits){{{count}{body}}}\n"
            ));
        }
        // Fixed-buffer element access: memcpy loads/stores (no alignment or
        // aliasing assumptions), unsigned index comparison rejects negatives.
        out.push_str(&format!(
            "static __forceinline {c} zn_v029_fixed_load_{t}(const void* data,int64_t index){{{c} v;memcpy(&v,(const unsigned char*)data+(uint64_t)index*{bytes},{bytes});return v;}}\n\
static __forceinline void zn_v029_fixed_store_{t}(void* data,int64_t index,{c} value){{memcpy((unsigned char*)data+(uint64_t)index*{bytes},&value,{bytes});}}\n\
static __forceinline {c} zn_v029_fixed_at_{t}(const void* data,uint64_t len,int64_t index){{if((uint64_t)index>=len)zn_fail(\"array index out of bounds\");return zn_v029_fixed_load_{t}(data,index);}}\n\
static __forceinline void zn_v029_fixed_set_{t}(void* data,uint64_t len,int64_t index,{c} value){{if((uint64_t)index>=len)zn_fail(\"array index out of bounds\");zn_v029_fixed_store_{t}(data,index,value);}}\n\
static __forceinline bool zn_v029_fixed_try_set_{t}(void* data,uint64_t len,int64_t index,{c} value){{if((uint64_t)index>=len)return false;zn_v029_fixed_store_{t}(data,index,value);return true;}}\n",
            bytes = w / 8
        ));
    }
    for (w, swap, swap_cast) in [
        (16, "_byteswap_ushort", "unsigned short"),
        (32, "_byteswap_ulong", "unsigned long"),
        (64, "_byteswap_uint64", "unsigned __int64"),
    ] {
        let u = format!("uint{w}_t");
        let bytes = w / 8;
        for (order, load, store) in [
            ("le", "v".to_string(), "value".to_string()),
            (
                "be",
                format!("({u}){swap}(({swap_cast})v)"),
                format!("({u}){swap}(({swap_cast})value)"),
            ),
        ] {
            out.push_str(&format!(
                "static __forceinline {u} zn_v029_load_u{w}_{order}(const void* data,int64_t offset){{{u} v;memcpy(&v,(const unsigned char*)data+(uint64_t)offset,{bytes});return {load};}}\n\
static __forceinline {u} zn_v029_read_u{w}_{order}(const void* data,uint64_t len,int64_t offset){{if(len<{bytes} || (uint64_t)offset>len-{bytes})zn_fail(\"packet read out of bounds\");return zn_v029_load_u{w}_{order}(data,offset);}}\n\
static __forceinline void zn_v029_store_u{w}_{order}(void* data,int64_t offset,{u} value){{{u} v={store};memcpy((unsigned char*)data+(uint64_t)offset,&v,{bytes});}}\n\
static __forceinline bool zn_v029_try_write_u{w}_{order}(void* data,uint64_t len,int64_t offset,{u} value){{if(len<{bytes} || (uint64_t)offset>len-{bytes})return false;zn_v029_store_u{w}_{order}(data,offset,value);return true;}}\n"
            ));
        }
    }
    out
}

/// 1.0 B loop-versioning guards (private, generated only). `zn_v100_term`
/// adds or subtracts one loop-invariant value in unsigned arithmetic and
/// clears `*ok` on signed overflow; `zn_v100_fits` is true exactly when
/// `sum+low` and `sum+high` (and so every value between) are within i64.
fn v100_helpers() -> &'static str {
    r#"
/* 1.0 width-exact loop-versioning guards (private, generated only). */
static __forceinline int64_t zn_v100_term(int64_t sum,int64_t value,int64_t sign,bool* ok){
    uint64_t a=(uint64_t)sum,b=(uint64_t)value,r=sign>0?a+b:a-b;int64_t out;
    if(sign>0?(((r^a)&(r^b))>>63):(((a^b)&(a^r))>>63))*ok=false;
    memcpy(&out,&r,sizeof(out));return out;
}
static __forceinline bool zn_v100_fits(int64_t sum,int64_t low,int64_t high){
    return (low>=0 || sum>=INT64_MIN-low) && (high<=0 || sum<=INT64_MAX-high);
}
"#
}

/// 1.1 B (contract M5) pinned VectorView entry versioning helpers (private,
/// generated only). `zn_v110_view_valid` is exactly the runtime invariant
/// `len <= INT64_MAX/width && (len == 0 || data != NULL)` that every
/// runtime-built view satisfies. The other two are emitted only inside the
/// body that this test selected, for the tested descriptor values:
/// - `len <= INT64_MAX` there, so one unsigned compare rejects exactly the
///   indices `index < 0 || index >= len` that `zn_v028_span_slot` and
///   `zn_span_check_pair` reject, with the same message;
/// - an index below `len` implies `len != 0`, so `data != NULL`, and
///   `len*width <= INT64_MAX`: the storage-overflow fault is unreachable;
/// - the pair check keeps `zn_span_check_pair`'s order: both indices first
///   ("vector region index out of bounds"), then distinctness.
fn v110_helpers() -> &'static str {
    r#"
/* 1.1 pinned VectorView entry versioning (private, generated only). */
static __forceinline bool zn_v110_view_valid(const zn_span* v,uint64_t width){
    return v->len<=(uint64_t)INT64_MAX/width && (v->len==0 || v->data!=NULL);
}
static __forceinline void* zn_v110_span_slot_fast(void* data,uint64_t len,int64_t index,uint64_t width){
    if((uint64_t)index>=len)zn_fail("vector region index out of bounds");
    return (unsigned char*)data+(uint64_t)index*width;
}
static __forceinline void zn_v110_span_check_pair(uint64_t len,int64_t left,int64_t right){
    if((uint64_t)left>=len || (uint64_t)right>=len)zn_fail("vector region index out of bounds");
    if(left==right)zn_fail("vector pair indices must be distinct");
}
"#
}

/// 1.2 B (contract N2) checked big-endian wire writes (private, generated
/// only when a program uses one). Exactly the N2 predicate: `false`, buffer
/// untouched, when `offset < 0 || offset > len || len - offset < width`;
/// otherwise the value's `width` bytes most-significant first, then `true`.
/// Proven writes on `[u8; N]` use `zn_v029_store_u{16,32,64}_be` directly.
fn v120_wire_helpers() -> &'static str {
    r#"
/* 1.2 checked big-endian wire writes (contract N2; private, generated only). */
static __forceinline bool zn_v120_try_write_u16_be(void* data,uint64_t len,int64_t offset,uint16_t value){if(offset<0 || (uint64_t)offset>len || len-(uint64_t)offset<2)return false;zn_v029_store_u16_be(data,offset,value);return true;}
static __forceinline bool zn_v120_try_write_u32_be(void* data,uint64_t len,int64_t offset,uint32_t value){if(offset<0 || (uint64_t)offset>len || len-(uint64_t)offset<4)return false;zn_v029_store_u32_be(data,offset,value);return true;}
static __forceinline bool zn_v120_try_write_u64_be(void* data,uint64_t len,int64_t offset,uint64_t value){if(offset<0 || (uint64_t)offset>len || len-(uint64_t)offset<8)return false;zn_v029_store_u64_be(data,offset,value);return true;}
"#
}

fn channel_declarations(types: &crate::types::TypeTable, ty: Type) -> String {
    let c = ty.c();
    let element = types.channel_element(ty).unwrap();
    let e = element.c();
    let drop = if element.owned() {
        format!("{}(({e}*)p);", element.drop_fn())
    } else {
        String::new()
    };
    format!("typedef zn_channel {c};\n_Static_assert(sizeof({c})==8,\"channel layout mismatch\");\nstatic void {c}_destroy(void* p){{{drop}memset(p,0,sizeof({e}));}}\nstatic {c} {c}_new(int64_t capacity){{{c} out={{0}};zn_channel_new(&out,capacity,sizeof({e}),{c}_destroy);return out;}}\nstatic void {c}_drop({c}* p){{zn_channel_drop(p);}}\nstatic {c} {c}_clone({c} source){{{c} out={{0}};zn_channel_clone(&out,&source);return out;}}\n")
}
fn arena_declarations(types: &crate::types::TypeTable, ty: Type) -> String {
    let c = ty.c();
    let element = types.arena_element(ty).unwrap();
    let e = element.c();
    let drop = if element.owned() {
        format!("{}(slot);", element.drop_fn())
    } else {
        String::new()
    };
    let cloned = if element.owned() {
        format!("{e}_clone(*slot)")
    } else {
        "*slot".into()
    };
    let mut out = format!(
        "typedef zn_arena {c};\n_Static_assert(sizeof({c})==48,\"arena layout mismatch\");\n"
    );
    out.push_str(&format!("static void {c}_drop({c}* p){{uint64_t cursor=0;{e}* slot;while((slot=({e}*)zn_arena_next_slot(p,&cursor,sizeof({e})))!=NULL){{{drop}}}zn_arena_release(p);}}\n"));
    out.push_str(&format!("static {c} {c}_clone({c} source){{{c} out={{0}};zn_arena_clone_storage(&out,&source,sizeof({e}));"));
    if element.owned() {
        out.push_str(&format!("uint64_t cursor=0;{e}* slot;while((slot=({e}*)zn_arena_next_slot(&out,&cursor,sizeof({e})))!=NULL)*slot={e}_clone(*({e}*)zn_arena_slot_at(&source,cursor-1,sizeof({e})));"));
    }
    out.push_str("return out;}\n");
    out.push_str(&format!("static __forceinline {e}* {c}_get(const {c}* p,uint64_t id){{return ({e}*)zn_v028_arena_slot(p,id,sizeof({e}));}}\n"));
    out.push_str(&format!("static {e} {c}_clone_get(const {c}* p,uint64_t id){{{e}* slot={c}_get(p,id);return {cloned};}}\n"));
    out.push_str(&format!("static uint64_t {c}_alloc({c}* p,{e} value){{uint64_t id=zn_arena_alloc_slot(p,sizeof({e}));*{c}_get(p,id)=value;return id;}}\n"));
    out.push_str(&format!("static bool {c}_free({c}* p,uint64_t id){{if(!zn_arena_contains_id(p,id))return false;{e}* slot={c}_get(p,id);{drop}zn_arena_erase_slot(p,id,sizeof({e}));return true;}}\n"));
    out
}
/// 1.0 B: true when dropping a value of `ty` has no effect other than zeroing
/// its own storage (scalars, fixed buffers, and records/enums whose owned
/// fields are all such values). Containers, text, tasks and views are not.
fn trivially_dropped(types: &crate::types::TypeTable, ty: Type, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    if !ty.owned() || matches!(ty, Type::Fixed(..)) {
        return true;
    }
    let Type::Named(id) = ty else {
        return false;
    };
    match &types.definitions[id].kind {
        DataKind::Record(fields) | DataKind::Enum(fields) => fields
            .iter()
            .all(|f| trivially_dropped(types, f.ty, depth + 1)),
        _ => false,
    }
}
fn soa_declarations(types: &crate::types::TypeTable, ty: Type) -> String {
    let c = ty.c();
    let element = types.soa_element(ty).unwrap();
    let e = element.c();
    let columns = types.soa_columns(ty).unwrap();
    let count = columns.len();
    let meta = columns
        .iter()
        .flat_map(|(offset, width)| [offset.to_string(), width.to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let mut out=format!("typedef zn_vec {c};\n_Static_assert(sizeof({c})==24,\"soa layout mismatch\");\nstatic const uint64_t {c}_columns[]={{{meta}}};\n");
    // 1.0 B: direct column addressing. SoA storage is only allocated by the
    // runtime (`zn_soa_push_index`, `zn_soa_clone_storage`) after its checked
    // `zn_soa_offset(cap,count)` computation, so for a valid layout (1..=256
    // nonzero-width columns) every column offset `sum((cap*w+7)&~7)` and row
    // offset `i*w` with `i < len <= cap` is in range; the runtime's per-column
    // capacity re-checks cannot fail. The index check, its message and its
    // position before any access are unchanged; the push row is `len-1`.
    if (1..=256).contains(&count) && columns.iter().all(|&(_, width)| width > 0) {
        let mut load = String::new();
        let mut store = String::new();
        let mut offset = "0".to_string();
        for &(field, width) in &columns {
            load.push_str(&format!(
                "memcpy((char*)&value+{field},base+({offset})+(size_t)i*{width},{width});"
            ));
            store.push_str(&format!(
                "memcpy(base+({offset})+(size_t)i*{width},(const char*)value+{field},{width});"
            ));
            offset = format!("{offset}+((cap*{width}+7)&~(size_t)7)");
        }
        out.push_str(&format!("static __forceinline {e} {c}_load(const {c}* p,int64_t i){{{e} value={{0}};if((uint64_t)i>=p->len)zn_fail(\"vector index out of bounds\");{{const char* base=(const char*)p->data;size_t cap=p->cap;{load}}}return value;}}\nstatic __forceinline void {c}_store_row({c}* p,int64_t i,const {e}* value){{char* base=(char*)p->data;size_t cap=p->cap;{store}}}\nstatic __forceinline void {c}_store({c}* p,int64_t i,const {e}* value){{if((uint64_t)i>=p->len)zn_fail(\"vector index out of bounds\");{c}_store_row(p,i,value);}}\n"));
        let drop = if trivially_dropped(types, element, 0) {
            String::new()
        } else {
            format!("for(size_t i=p->len;i;i--){{{e} value={c}_load(p,(int64_t)i-1);{e}_drop(&value);}}")
        };
        out.push_str(&format!("static void {c}_drop({c}* p){{{drop}zn_vec_release(p);}}\nstatic __forceinline void {c}_push({c}* p,{e} value){{int64_t i=zn_soa_push_index(p,{count},{c}_columns);{c}_store_row(p,i,&value);}}\n"));
    } else {
        out.push_str(&format!("static {e} {c}_load(const {c}* p,int64_t i){{{e} value={{0}};zn_soa_load(&value,p,i,{count},{c}_columns);return value;}}\nstatic void {c}_store({c}* p,int64_t i,const {e}* value){{zn_soa_store(p,i,value,{count},{c}_columns);}}\n"));
        out.push_str(&format!("static void {c}_drop({c}* p){{for(size_t i=p->len;i;i--){{{e} value={c}_load(p,(int64_t)i-1);{e}_drop(&value);}}zn_vec_release(p);}}\nstatic void {c}_push({c}* p,{e} value){{int64_t i=zn_soa_push_index(p,{count},{c}_columns);{c}_store(p,i,&value);}}\n"));
    }
    out.push_str(&format!("static {c} {c}_clone({c} source){{{c} out={{0}};zn_soa_clone_storage(&out,&source,{count},{c}_columns);for(size_t i=0;i<source.len;i++){{{e} value={e}_clone({c}_load(&source,(int64_t)i));{c}_store(&out,(int64_t)i,&value);}}return out;}}\n"));
    out.push_str(&format!("static {e} {c}_clone_at(const {c}* p,int64_t i){{return {e}_clone({c}_load(p,i));}}\nstatic {e} {c}_pop({c}* p){{{e} out={{0}};zn_soa_pop(&out,p,{count},{c}_columns);return out;}}\nstatic void {c}_replace({c}* p,int64_t i,{e} value){{{e} old={c}_load(p,i);{e}_drop(&old);{c}_store(p,i,&value);}}\n"));
    out
}

fn map_declarations(types: &crate::types::TypeTable, ty: Type) -> String {
    let layout = types.map_entry_layout(ty).unwrap();
    let c = ty.c();
    let k = layout.key.c();
    let v = layout.value.c();
    let width = layout.width;
    let offset = layout.value_offset;
    let code = layout.key_code;
    let key_drop = if layout.key.owned() {
        format!("{}(&entry->key);", layout.key.drop_fn())
    } else {
        String::new()
    };
    let value_drop = if layout.value.owned() {
        format!("{}(&entry->value);", layout.value.drop_fn())
    } else {
        String::new()
    };
    let cloned = if layout.value.owned() {
        format!("{v}_clone(*value)")
    } else {
        "*value".into()
    };
    let mut out=format!("typedef zn_map {c};\ntypedef struct{{{k} key;{v} value;}} {c}_entry;\n_Static_assert(sizeof({c})==40,\"map layout mismatch\");\n_Static_assert(sizeof({c}_entry)=={width},\"map entry layout mismatch\");\n_Static_assert(offsetof({c}_entry,value)=={offset},\"map value layout mismatch\");\n");
    out.push_str(&format!("static void {c}_drop({c}* p){{uint64_t cursor=0;{c}_entry* entry;while((entry=({c}_entry*)zn_map_next_slot(p,&cursor,{width}))!=NULL){{{value_drop}{key_drop}}}zn_map_release(p);}}\n"));
    out.push_str(&format!("static {c} {c}_clone({c} source){{{c} out={{0}};zn_map_clone_storage(&out,&source,{width});"));
    if layout.key.owned() || layout.value.owned() {
        out.push_str(&format!("uint64_t cursor=0;{c}_entry* entry;while((entry=({c}_entry*)zn_map_next_slot(&out,&cursor,{width}))!=NULL){{{c}_entry* original=({c}_entry*)zn_map_slot_at(&source,cursor-1,{width});"));
        if layout.key.owned() {
            out.push_str(&format!("entry->key={k}_clone(original->key);"));
        }
        if layout.value.owned() {
            out.push_str(&format!("entry->value={v}_clone(original->value);"));
        }
        out.push('}');
    }
    out.push_str("return out;}\n");
    out.push_str(&format!("static bool {c}_contains_key(const {c}* p,{k} key){{return zn_map_find(p,&key,{code},{width})!=NULL;}}\n"));
    out.push_str(&format!("static {v}* {c}_get(const {c}* p,{k} key){{{c}_entry* entry=({c}_entry*)zn_map_find(p,&key,{code},{width});if(!entry)zn_fail(\"map key not found\");return &entry->value;}}\n"));
    out.push_str(&format!("static {v} {c}_clone_get(const {c}* p,{k} key){{{v}* value={c}_get(p,key);return {cloned};}}\n"));
    out.push_str(&format!("static void {c}_insert({c}* p,{k} key,{v} value){{bool replaced;{c}_entry* entry=({c}_entry*)zn_map_insert_slot(p,&key,{code},{width},&replaced);if(replaced){{{value_drop}{key_drop}}}entry->key=key;entry->value=value;}}\n"));
    out.push_str(&format!("static bool {c}_remove({c}* p,{k} key){{{c}_entry* entry=({c}_entry*)zn_map_find(p,&key,{code},{width});if(!entry)return false;{value_drop}{key_drop}zn_map_erase_slot(p,entry,{width});return true;}}\n"));
    out
}

fn walk_function<'a>(
    f: &'a Function,
    mut visit: impl FnMut(&'a Expr, bool),
    mut statement: impl FnMut(&'a Stmt),
) {
    let mut statements: Vec<_> = f.body.iter().map(|s| (s, false)).collect();
    let mut expressions: Vec<_> = f
        .requires
        .iter()
        .chain(&f.ensures)
        .map(|e| (e, false))
        .collect();
    while let Some((s, in_loop)) = statements.pop() {
        statement(s);
        match &s.kind {
            StmtKind::ForRange {
                start, end, body, ..
            } => {
                expressions.extend([start, end].map(|e| (e, in_loop)));
                statements.extend(body.iter().map(|s| (s, true)));
            }
            StmtKind::ForEach { owner, body, .. } => {
                expressions.push((owner, in_loop));
                statements.extend(body.iter().map(|s| (s, true)));
            }
            StmtKind::Element {
                owner, index, body, ..
            } => {
                expressions.extend([owner, index].map(|e| (e, in_loop)));
                statements.extend(body.iter().map(|s| (s, in_loop)));
            }
            StmtKind::ElementPair {
                owner,
                indices,
                body,
                ..
            } => {
                expressions.push((owner, in_loop));
                expressions.extend(indices.iter().map(|e| (e, in_loop)));
                statements.extend(body.iter().map(|s| (s, in_loop)));
            }
            StmtKind::Region {
                owner,
                start,
                end,
                body,
                ..
            } => {
                expressions.extend([owner, start, end].map(|e| (e, in_loop)));
                statements.extend(body.iter().map(|s| (s, in_loop)));
            }
            StmtKind::Let(_, _, e)
            | StmtKind::Assign(_, e)
            | StmtKind::Expr(e)
            | StmtKind::Print(e)
            | StmtKind::Assert(e) => expressions.push((e, in_loop)),
            StmtKind::AssignField(a, b) => {
                expressions.push((a, in_loop));
                expressions.push((b, in_loop));
            }
            StmtKind::Return(e) => expressions.extend(e.iter().map(|e| (e, in_loop))),
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::If(e, a, b) => {
                expressions.push((e, in_loop));
                statements.extend(a.iter().chain(b).map(|s| (s, in_loop)));
            }
            StmtKind::While(e, body) => {
                expressions.push((e, true));
                statements.extend(body.iter().map(|s| (s, true)));
            }
            StmtKind::Match(e, arms) => {
                expressions.push((e, in_loop));
                for arm in arms {
                    statements.extend(arm.body.iter().map(|s| (s, in_loop)));
                }
            }
        }
    }
    while let Some((e, in_loop)) = expressions.pop() {
        visit(e, in_loop);
        match &e.kind {
            ExprKind::MethodCall(receiver, _, args) => {
                expressions.push((receiver, in_loop));
                expressions.extend(args.iter().map(|e| (e, in_loop)));
            }
            ExprKind::Call(_, args) | ExprKind::Spawn(_, args) | ExprKind::Array(args) => {
                expressions.extend(args.iter().map(|e| (e, in_loop)))
            }
            ExprKind::Unary(_, e)
            | ExprKind::Field(e, _)
            | ExprKind::Repeat(e, _)
            | ExprKind::Try(e) => expressions.push((e, in_loop)),
            ExprKind::Binary(_, a, b) => {
                expressions.push((a, in_loop));
                expressions.push((b, in_loop));
            }
            ExprKind::Record(_, fields) => {
                expressions.extend(fields.iter().map(|(_, e)| (e, in_loop)))
            }
            _ => {}
        }
    }
}
fn worker_targets(functions: &[Function]) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    for f in functions {
        walk_function(
            f,
            |e, _| {
                if let ExprKind::Spawn(n, _) = &e.kind {
                    out.insert(n.as_str());
                }
            },
            |_| {},
        );
    }
    out
}
fn inline_helpers(functions: &[Function]) -> BTreeSet<&str> {
    let mut usage = BTreeMap::<&str, (usize, bool)>::new();
    for f in functions {
        walk_function(
            f,
            |e, in_loop| {
                if let ExprKind::Call(n, _) = &e.kind {
                    let entry = usage.entry(n.as_str()).or_default();
                    entry.0 += 1;
                    entry.1 |= in_loop;
                }
            },
            |_| {},
        );
    }
    functions
        .iter()
        .filter(|f| {
            f.name != "main"
                && !f.ret.owned()
                && f.params
                    .iter()
                    .all(|p| !p.ty.owned() || p.borrow != Borrow::Owned)
                && usage.get(f.name.as_str()).is_some_and(|&(n, l)| n > 1 || l)
                && small_leaf(f)
        })
        .map(|f| f.name.as_str())
        .collect()
}
fn small_leaf(f: &Function) -> bool {
    let mut exprs = 0;
    let mut stmts = 0;
    let mut safe_expr = true;
    let mut safe_stmt = true;
    walk_function(
        f,
        |e, _| {
            exprs += 1;
            match &e.kind {
                ExprKind::Call(n, _) if ["len", "at", "byte_at", "set"].contains(&n.as_str()) => {}
                ExprKind::Call(_, _)
                | ExprKind::Spawn(_, _)
                | ExprKind::Text(_)
                | ExprKind::Array(_)
                | ExprKind::Record(_, _) => safe_expr = false,
                _ => {}
            }
        },
        |s| {
            stmts += 1;
            if matches!(
                s.kind,
                StmtKind::While(_, _)
                    | StmtKind::ForRange { .. }
                    | StmtKind::ForEach { .. }
                    | StmtKind::Match(_, _)
            ) {
                safe_stmt = false;
            }
        },
    );
    exprs + stmts <= 48 && safe_expr && safe_stmt
}
fn hot_functions<'a>(functions: &'a [Function], inline: &BTreeSet<&'a str>) -> BTreeSet<&'a str> {
    let mut hot = inline.clone();
    let mut calls = BTreeMap::<&str, BTreeSet<&str>>::new();
    for f in functions {
        let mut loop_present = false;
        let mut edges = BTreeSet::new();
        walk_function(
            f,
            |e, _| {
                if let ExprKind::Call(n, _) = &e.kind {
                    edges.insert(n.as_str());
                }
            },
            |s| {
                if matches!(
                    s.kind,
                    StmtKind::While(_, _) | StmtKind::ForRange { .. } | StmtKind::ForEach { .. }
                ) {
                    loop_present = true;
                }
            },
        );
        if loop_present || edges.contains(f.name.as_str()) {
            hot.insert(f.name.as_str());
        }
        calls.insert(f.name.as_str(), edges);
    }
    loop {
        let mut changed = false;
        for name in hot.clone() {
            if let Some(edges) = calls.get(name) {
                for edge in edges {
                    if calls.contains_key(edge) {
                        changed |= hot.insert(edge);
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    hot
}

/* Stage1 value support. Source functions and control flow are emitted by cgen.an.
   Every B is either an independent owner or a scalar. Scoped B* aliases never
   own; b_take clears transferred storage, including partially moved fields. */
#ifndef ANODE_BOOTSTRAP_VALUE_H
#define ANODE_BOOTSTRAP_VALUE_H
#include "runtime.h"
#include <stdlib.h>
#include <stdio.h>
#include <math.h>
#include <wchar.h>
typedef struct B B;
typedef struct BO { size_t len,cap; B* data; uint32_t* generations; unsigned char* live; size_t count,free_head; size_t* next_free; } BO;
struct B { int kind; int64_t i; double f; zn_text t; BO* o; uint32_t slot,generation; B* alias; size_t view_len; };
enum { BU,BI,BB,BT,BF,BID,BREF,BVIEW,BROW,BSEQ,BREC,BENUM,BMAP,BARENA,BSOA };
static int64_t b_owners=0;
static unsigned b_depth=0;
static B b_unit(void) { B v={0}; return v; }
static B b_int(int64_t n) { B v={0};v.kind=BI;v.i=n;v.slot=64;return v; }
static B b_number(uint64_t raw,uint64_t code) { B v=b_int(zn_num_signed(raw));v.slot=(uint32_t)code;return v; }
static uint64_t b_raw(const B* p) { uint64_t raw;memcpy(&raw,&p->i,8);return raw; }
static B b_intrinsic(unsigned op,const B* a,const B* b) {
    unsigned width=a->slot>100?a->slot-100:a->slot;
    if(a->kind!=BI || (width!=8 && width!=16 && width!=32 && width!=64))zn_fail("integer intrinsic requires integer operands");
    uint64_t mask=width==64?UINT64_MAX:(UINT64_C(1)<<width)-1;
    uint64_t x=b_raw(a)&mask,y=b?b_raw(b)&mask:0,r=0;
    unsigned n=b?(unsigned)(b_raw(b)&(width-1)):0,reverse=(width-n)&(width-1);
    switch(op){
    case 0:r=x+y;break;case 1:r=x-y;break;case 2:r=x*y;break;
    case 3:r=x&y;break;case 4:r=x|y;break;case 5:r=x^y;break;case 6:r=~x;break;
    case 7:r=x<<n;break;
    case 8:r=x>>n;if(a->slot<100 && n && (x&(UINT64_C(1)<<(width-1))))r|=mask^(mask>>n);break;
    case 9:r=(x<<n)|(x>>reverse);break;case 10:r=(x>>n)|(x<<reverse);break;
    default:zn_fail("invalid integer intrinsic");}
    r&=mask;if(a->slot<100 && width<64 && (r&(UINT64_C(1)<<(width-1))))r|=~mask;
    return b_number(r,a->slot);
}
static B b_bool(bool n) { B v=b_int(n);v.kind=BB;return v; }
static B b_float(double n) { B v={0};v.kind=BF;v.f=n;return v; }
static B b_text(zn_text t) { B v={0};v.kind=BT;v.t=t;++b_owners;return v; }
static B b_literal(const char* s) { return b_text(zn_text_make(s)); }
static B* b_resolve(B* p) { while(p && p->kind==BREF)p=p->alias;return p; }
static const B* b_const(const B* p) { return b_resolve((B*)p); }
static B b_take(B* p) { p=b_resolve(p); B out=*p;*p=b_unit();return out; }
static void b_drop(B* p);
static B b_clone(const B* p);
static B* b_at(B* p,int64_t index);
static B b_seq(int kind,size_t len) {
    if(len>SIZE_MAX/sizeof(B))zn_fail("collection too large");
    B v={0};v.kind=kind;v.o=(BO*)zn_alloc(sizeof(BO));
    memset(v.o,0,sizeof(BO));v.o->len=len;v.o->cap=len;v.o->data=(B*)zn_alloc(len*sizeof(B));
    if(len)memset(v.o->data,0,len*sizeof(B));++b_owners;return v;
}
static void b_drop(B* p) {
    if(p->kind==BT){zn_text_drop(&p->t);--b_owners;}
    else if(p->kind>=BSEQ){for(size_t i=p->o->len;i>0;--i)b_drop(&p->o->data[i-1]);zn_release(p->o->data);zn_release(p->o->generations);zn_release(p->o->live);zn_release(p->o->next_free);zn_release(p->o);--b_owners;}
    *p=b_unit();
}
static B b_clone(const B* p) {
    p=b_const(p);
    if(p->kind==BROW){B out=b_seq(BREC,p->o->len);for(size_t i=0;i<p->o->len;++i)out.o->data[i]=b_clone(b_at((B*)p,(int64_t)i));return out;}
    if(p->kind==BVIEW){B out=b_seq(BSEQ,p->view_len);for(size_t i=0;i<p->view_len;++i)out.o->data[i]=b_clone(&p->alias[i]);return out;}
    if(p->kind==BT)return b_text(zn_text_clone(p->t));
    if(p->kind>=BSEQ){B out=b_seq(p->kind,p->o->len);out.i=p->i;out.o->count=p->o->count;if(p->kind==BARENA){out.o->count=p->o->count;out.o->free_head=p->o->free_head;out.o->generations=(uint32_t*)zn_alloc(out.o->cap*sizeof(uint32_t));out.o->live=(unsigned char*)zn_alloc(out.o->cap);out.o->next_free=(size_t*)zn_alloc(out.o->cap*sizeof(size_t));if(out.o->cap){memcpy(out.o->generations,p->o->generations,out.o->cap*sizeof(uint32_t));memcpy(out.o->live,p->o->live,out.o->cap);memcpy(out.o->next_free,p->o->next_free,out.o->cap*sizeof(size_t));}}for(size_t i=0;i<p->o->len;++i)out.o->data[i]=b_clone(&p->o->data[i]);return out;}
    return *p;
}
static void b_assign(B* p,B* value) {
    p=b_resolve(p);value=b_resolve(value);if(p==value)return;
    if(p->kind==BROW){if(value->kind!=BREC || value->o->len!=p->o->len)zn_fail("SoA replacement requires complete record");for(size_t i=0;i<p->o->len;++i)b_assign(b_at(p,(int64_t)i),&value->o->data[i]);b_drop(value);return;}
    B fresh=b_take(value);b_drop(p);*p=fresh;
}
static int64_t b_integer(const B* p) { p=b_const(p); if(p->kind!=BI)zn_fail("expected integer");return p->i; }
static bool b_boolean(const B* p) { p=b_const(p); if(p->kind!=BB)zn_fail("expected boolean");return p->i!=0; }
static zn_text b_string(const B* p) { p=b_const(p); if(p->kind!=BT)zn_fail("expected text");return p->t; }
static int64_t b_len(const B* p) {
    p=b_const(p);if(p->kind==BVIEW)return zn_length(p->view_len);
    if(p->kind==BT)return zn_length(p->t.len);
    if(p->kind<BSEQ)zn_fail("expected collection");
    return zn_length((p->kind==BARENA || p->kind==BSOA)?p->o->count:(p->kind==BMAP?p->o->len/2:p->o->len));
}
static B* b_at(B* p,int64_t index) {
    p=b_resolve(p);
    if(p->kind==BVIEW){if((uint64_t)index>=p->view_len)zn_fail("collection index out of bounds");return &p->alias[(size_t)index];}
    if(p->kind==BROW){if((uint64_t)index>=p->o->len)zn_fail("record field out of bounds");return b_at(&p->o->data[(size_t)index],p->i);}
    if(p->kind<BSEQ || p->kind==BSOA || (uint64_t)index>=p->o->len)zn_fail("collection index out of bounds");
    return &p->o->data[(size_t)index];
}
/* v0.26: real per-field columns; row/span descriptors are nonowning stack values.
   No row-array fallback, per-region allocation, or frozen runtime changes. */
static B b_soa(size_t fields) {
    if(!fields)zn_fail("SoA requires nonempty record");B out=b_seq(BSOA,fields);
    for(size_t i=0;i<fields;++i)out.o->data[i]=b_seq(BSEQ,0);return out;
}
static B b_select(B* p,int64_t index) {
    p=b_resolve(p);B out=b_unit();
    if(p->kind==BSOA){if((uint64_t)index>=p->o->count)zn_fail("collection index out of bounds");out.kind=BROW;out.o=p->o;out.i=index;}
    else{out.kind=BREF;out.alias=b_at(p,index);}return out;
}
static B b_span(B* p,int64_t start,int64_t count) {
    p=b_resolve(p);if(p->kind!=BSEQ && p->kind!=BVIEW)zn_fail("region requires contiguous vector");
    int64_t length=b_len(p);
    if(start<0 || count<0 || start>length || count>length-start)zn_fail("vector region out of bounds");
    if((uint64_t)start>SIZE_MAX/sizeof(B))zn_fail("vector region out of bounds");
    B out=b_unit();out.kind=BVIEW;B* data=p->kind==BVIEW?p->alias:p->o->data;
    out.alias=start?data+(size_t)start:data;out.view_len=(size_t)count;return out;
}
static B b_int_region(B* p,int64_t start,int64_t end) {
    int64_t length=b_len(p);if(start<0 || end<start || end>length)zn_fail("integer region out of bounds");return b_span(p,start,end-start);
}
static void b_pair_check(B* p,int64_t left,int64_t right) {
    int64_t length=b_len(p);if(left<0 || right<0 || left>=length || right>=length)zn_fail("collection index out of bounds");if(left==right)zn_fail("at_pair requires distinct indices");
}
static void b_push(B* p,B* value) {
    p=b_resolve(p);value=b_resolve(value);
    if(p->kind==BSOA){if(value->kind!=BREC || value->o->len!=p->o->len)zn_fail("SoA push requires complete record");if(p->o->count==INT64_MAX)zn_fail("collection too large");for(size_t i=0;i<p->o->len;++i)b_push(&p->o->data[i],&value->o->data[i]);++p->o->count;b_drop(value);return;}
    if(p->kind!=BSEQ)zn_fail("push requires vector");
    if(p->o->len==p->o->cap){size_t old=p->o->cap;if(old>SIZE_MAX/(2*sizeof(B)))zn_fail("collection too large");size_t cap=old?old*2:4;B* fresh=(B*)zn_alloc(cap*sizeof(B));if(old)memcpy(fresh,p->o->data,p->o->len*sizeof(B));zn_release(p->o->data);p->o->data=fresh;p->o->cap=cap;}
    p->o->data[p->o->len++]=b_take(value);
}
static B b_pop(B* p) {
    p=b_resolve(p);
    if(p->kind==BSOA){if(!p->o->count)zn_fail("pop from empty vector");B out=b_seq(BREC,p->o->len);for(size_t i=0;i<p->o->len;++i)out.o->data[i]=b_pop(&p->o->data[i]);--p->o->count;return out;}
    if(p->kind!=BSEQ || !p->o->len)zn_fail("pop from empty vector");return b_take(&p->o->data[--p->o->len]);
}
static void b_replace(B* p,int64_t index,B* value) { B row=b_select(p,index);b_assign(&row,value); }
static void b_swap(B* p,int64_t left,int64_t right) {
    p=b_resolve(p);int64_t length=b_len(p);if(left<0 || right<0 || left>=length || right>=length)zn_fail("collection index out of bounds");
    if(p->kind==BSOA){for(size_t i=0;i<p->o->len;++i)b_swap(&p->o->data[i],left,right);return;}
    B* a=b_at(p,left);B* b=b_at(p,right);if(a==b)return;B t=b_take(a);*a=b_take(b);*b=b_take(&t);
}
static bool b_equal(const B* a,const B* b) { a=b_const(a);b=b_const(b);
    if(a->kind!=b->kind)zn_fail("key type mismatch");
    if(a->kind==BT)return zn_text_eq(a->t,b->t);
    if(a->kind==BI || a->kind==BB)return a->i==b->i;
    if(a->kind==BID)return a->slot==b->slot && a->generation==b->generation;
    zn_fail("unsupported map key");return false;
}
static int64_t b_find(B* map,const B* key) {
    if(map->kind!=BMAP)zn_fail("expected map");
    for(size_t i=0;i<map->o->len;i+=2)if(b_equal(&map->o->data[i],key))return zn_length(i);
    return -1;
}
static B* b_arena_get(B* arena,const B* id);
static B* b_get(B* map,const B* key) { if(map->kind==BARENA)return b_arena_get(map,key); int64_t i=b_find(map,key);if(i<0)zn_fail("map key not found");return b_at(map,i+1); }
static B b_enum(int64_t tag,B* payload) { B v=b_seq(BENUM,payload?1:0);v.i=tag;if(payload)v.o->data[0]=b_take(payload);return v; }
static void b_print(const B* p) { p=b_const(p);
    if(p->kind==BI){if(p->slot>100)zn_print_uint(b_raw(p));else zn_print_int(p->i);}else if(p->kind==BB)zn_print_bool(p->i!=0);else if(p->kind==BT)zn_print_text(p->t);else if(p->kind==BF)zn_print_float(zn_f64_bits(p->f),64);else zn_fail("unsupported print value");
}
static B b_binary(const char* op,const B* a,const B* b) { a=b_const(a);b=b_const(b);
    if(a->kind!=b->kind || (a->kind==BI && a->slot!=b->slot))zn_fail("operator type mismatch");
    if(a->kind==BID){bool equal=b_equal(a,b);if(!strcmp(op,"=="))return b_bool(equal);if(!strcmp(op,"!="))return b_bool(!equal);zn_fail("id supports only equality");}
    if(!strcmp(op,"=="))return b_bool(a->kind==BF?a->f==b->f:a->i==b->i);
    if(!strcmp(op,"!="))return b_bool(a->kind==BF?a->f!=b->f:a->i!=b->i);
    if(!strcmp(op,"<"))return b_bool(a->kind==BF?a->f<b->f:(a->kind==BI && a->slot>100?b_raw(a)<b_raw(b):a->i<b->i));
    if(!strcmp(op,"<="))return b_bool(a->kind==BF?a->f<=b->f:(a->kind==BI && a->slot>100?b_raw(a)<=b_raw(b):a->i<=b->i));
    if(!strcmp(op,">"))return b_bool(a->kind==BF?a->f>b->f:(a->kind==BI && a->slot>100?b_raw(a)>b_raw(b):a->i>b->i));
    if(!strcmp(op,">="))return b_bool(a->kind==BF?a->f>=b->f:(a->kind==BI && a->slot>100?b_raw(a)>=b_raw(b):a->i>=b->i));
    if(a->kind==BF){if(!strcmp(op,"+"))return b_float(a->f+b->f);if(!strcmp(op,"-"))return b_float(a->f-b->f);if(!strcmp(op,"*"))return b_float(a->f*b->f);if(!strcmp(op,"/"))return b_float(a->f/b->f);if(!strcmp(op,"%"))return b_float(fmod(a->f,b->f));}
    if(a->kind==BI){unsigned code=99;if(!strcmp(op,"+"))code=0;else if(!strcmp(op,"-"))code=1;else if(!strcmp(op,"*"))code=2;else if(!strcmp(op,"/"))code=3;else if(!strcmp(op,"%"))code=4;if(code<5)return b_number(zn_num_binary(b_raw(a),b_raw(b),a->slot,code),a->slot);}
    zn_fail("unsupported binary operator");return b_unit();
}
static B b_texts(zn_texts lines) { B out=b_seq(BSEQ,lines.len);for(size_t i=0;i<lines.len;++i)out.o->data[i]=b_text(lines.data[i]);zn_release(lines.data);return out; }
/* v0.25 support authored here; frozen runtime.c/runtime.h remain byte-identical.
   Nominal source types distinguish arena ids. Their runtime bits are relative
   slot/generation values, exactly as in 0.24, not global instance provenance.
   Independent same-type arenas may accept equal live ids; clones own payloads. */
static B b_arena(void) { B out=b_seq(BARENA,0);out.o->free_head=SIZE_MAX;return out; }
static bool b_contains_id(B* arena,const B* id) {
    if(arena->kind!=BARENA || id->kind!=BID)zn_fail("expected arena and id");
    return id->generation!=0 && id->slot<arena->o->len && arena->o->live[id->slot]==1 && arena->o->generations[id->slot]==id->generation;
}
static B* b_arena_get(B* arena,const B* id) { if(!b_contains_id(arena,id))zn_fail("stale or invalid arena handle");return &arena->o->data[id->slot]; }
static B b_alloc(B* arena,B* value) {
    if(arena->kind!=BARENA)zn_fail("alloc requires arena");BO* o=arena->o;size_t slot=o->free_head;
    if(slot!=SIZE_MAX)o->free_head=o->next_free[slot];
    else {
        if(o->len>UINT32_MAX)zn_fail("arena slot space exhausted");slot=o->len;
        if(o->len==o->cap){size_t old=o->cap;if(old>SIZE_MAX/(2*sizeof(B)))zn_fail("arena too large");size_t cap=old?old*2:4;
            B* data=(B*)zn_alloc(cap*sizeof(B));uint32_t* generations=(uint32_t*)zn_alloc(cap*sizeof(uint32_t));unsigned char* live=(unsigned char*)zn_alloc(cap);size_t* next=(size_t*)zn_alloc(cap*sizeof(size_t));
            memset(data,0,cap*sizeof(B));memset(generations,0,cap*sizeof(uint32_t));memset(live,0,cap);memset(next,0,cap*sizeof(size_t));
            if(o->len){memcpy(data,o->data,o->len*sizeof(B));memcpy(generations,o->generations,o->len*sizeof(uint32_t));memcpy(live,o->live,o->len);memcpy(next,o->next_free,o->len*sizeof(size_t));}
            zn_release(o->data);zn_release(o->generations);zn_release(o->live);zn_release(o->next_free);o->data=data;o->generations=generations;o->live=live;o->next_free=next;o->cap=cap;
        }++o->len;o->generations[slot]=1;
    }
    o->data[slot]=b_take(value);o->live[slot]=1;++o->count;B id=b_unit();id.kind=BID;id.slot=(uint32_t)slot;id.generation=o->generations[slot];return id;
}
static bool b_free(B* arena,const B* id) {
    if(!b_contains_id(arena,id))return false;BO* o=arena->o;b_drop(&o->data[id->slot]);--o->count;
    if(o->generations[id->slot]==UINT32_MAX)o->live[id->slot]=2;
    else{++o->generations[id->slot];o->live[id->slot]=0;o->next_free[id->slot]=o->free_head;o->free_head=id->slot;}
    return true;
}
static bool b_try_failed(B* value) { if(value->kind!=BENUM || (value->i!=0 && value->i!=1))zn_fail("try requires two-variant result");return value->i==1; }
static B* b_payload(B* value) { if(value->kind!=BENUM || value->o->len>1)zn_fail("invalid enum payload");return value->o->len?&value->o->data[0]:NULL; }
static B b_try_unwrap(B* value) { if(b_try_failed(value))zn_fail("unhandled try failure");B* payload=b_payload(value);return payload?b_take(payload):b_unit(); }
static B b_call(const char* name,B* a,B* b,B* c) {
    a=b_resolve(a);b=b_resolve(b);c=b_resolve(c);
    if(!strcmp(name,"wrapping_add"))return b_intrinsic(0,a,b);
    if(!strcmp(name,"wrapping_sub"))return b_intrinsic(1,a,b);
    if(!strcmp(name,"wrapping_mul"))return b_intrinsic(2,a,b);
    if(!strcmp(name,"bit_and"))return b_intrinsic(3,a,b);
    if(!strcmp(name,"bit_or"))return b_intrinsic(4,a,b);
    if(!strcmp(name,"bit_xor"))return b_intrinsic(5,a,b);
    if(!strcmp(name,"bit_not"))return b_intrinsic(6,a,b);
    if(!strcmp(name,"shl"))return b_intrinsic(7,a,b);
    if(!strcmp(name,"shr"))return b_intrinsic(8,a,b);
    if(!strcmp(name,"rotl"))return b_intrinsic(9,a,b);
    if(!strcmp(name,"rotr"))return b_intrinsic(10,a,b);
    if(!strcmp(name,"i8")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,8),8);}
    if(!strcmp(name,"i16")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,16),16);}
    if(!strcmp(name,"i32")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,32),32);}
    if(!strcmp(name,"u8")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,108),108);}
    if(!strcmp(name,"u16")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,116),116);}
    if(!strcmp(name,"u32")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,132),132);}
    if(!strcmp(name,"u64")){if(a->kind!=BI)zn_fail("integer conversion requires integer scalar");return b_number(zn_num_cast(b_raw(a),a->slot,164),164);}
    if(!strcmp(name,"alloc"))return b_alloc(a,b);
    if(!strcmp(name,"free"))return b_bool(b_free(a,b));
    if(!strcmp(name,"contains_id"))return b_bool(b_contains_id(a,b));
    if(!strcmp(name,"clone"))return b_clone(a);
    if(!strcmp(name,"ints") || !strcmp(name,"texts"))return b_seq(BSEQ,0);
    if(!strcmp(name,"len"))return b_int(b_len(a));
    if(!strcmp(name,"capacity"))return b_int(zn_length(a->kind==BSOA?a->o->data[0].o->cap:a->o->cap));
    if(!strcmp(name,"push")){b_push(a,b);return b_unit();}
    if(!strcmp(name,"at") || !strcmp(name,"clone_at") || !strcmp(name,"text_at")){B row=b_select(a,b_integer(b));return b_clone(&row);}
    if(!strcmp(name,"set") || !strcmp(name,"replace")){b_replace(a,b_integer(b),c);return b_unit();}
    if(!strcmp(name,"pop"))return b_pop(a);
    if(!strcmp(name,"swap")){b_swap(a,b_integer(b),b_integer(c));return b_unit();}
    if(!strcmp(name,"insert")){int64_t i=b_find(a,b);if(i>=0){b_drop(b);b_assign(b_at(a,i+1),c);}else{a->kind=BSEQ;b_push(a,b);b_push(a,c);a->kind=BMAP;}return b_unit();}
    if(!strcmp(name,"contains_key"))return b_bool(b_find(a,b)>=0);
    if(!strcmp(name,"clone_get"))return b_clone(b_get(a,b));
    if(!strcmp(name,"remove")){int64_t i=b_find(a,b);if(i<0)return b_bool(false);b_drop(b_at(a,i));b_drop(b_at(a,i+1));size_t remaining=a->o->len-(size_t)i-2;if(remaining)memmove(a->o->data+i,a->o->data+i+2,remaining*sizeof(B));a->o->len-=2;return b_bool(true);}
    if(!strcmp(name,"args_count"))return b_int(zn_args_count());
    if(!strcmp(name,"arg"))return b_text(zn_arg(b_integer(a)));
    if(!strcmp(name,"int_text"))return b_text(zn_int_text(b_integer(a)));
    if(!strcmp(name,"byte_at"))return b_int(zn_byte_at(b_string(a),b_integer(b)));
    if(!strcmp(name,"char_count"))return b_int(zn_char_count(b_string(a)));
    if(!strcmp(name,"codepoint_at"))return b_int(zn_codepoint_at(b_string(a),b_integer(b)));
    if(!strcmp(name,"next_char_boundary"))return b_int(zn_next_char_boundary(b_string(a),b_integer(b)));
    if(!strcmp(name,"is_char_boundary"))return b_bool(zn_is_char_boundary(b_string(a),b_integer(b)));
    if(!strcmp(name,"text_eq"))return b_bool(zn_text_eq(b_string(a),b_string(b)));
    if(!strcmp(name,"contains"))return b_bool(zn_contains(b_string(a),b_string(b)));
    if(!strcmp(name,"text_starts_with"))return b_bool(zn_text_starts_with(b_string(a),b_string(b)));
    if(!strcmp(name,"text_ends_with"))return b_bool(zn_text_ends_with(b_string(a),b_string(b)));
    if(!strcmp(name,"concat"))return b_text(zn_concat(b_string(a),b_string(b)));
    if(!strcmp(name,"slice") || !strcmp(name,"region"))return b_text(zn_slice(b_string(a),b_integer(b),b_integer(c)));
    if(!strcmp(name,"text_trim"))return b_text(zn_text_trim(b_string(a)));
    if(!strcmp(name,"to_text"))return b_clone(a);
    if(!strcmp(name,"is_int"))return b_bool(zn_is_int(b_string(a)));
    if(!strcmp(name,"parse_int"))return b_int(zn_parse_int(b_string(a)));
    if(!strcmp(name,"is_f64"))return b_bool(zn_is_f64(b_string(a)));
    if(!strcmp(name,"parse_f64"))return b_float(zn_parse_f64(b_string(a)));
    if(!strcmp(name,"text_lines"))return b_texts(zn_text_lines(b_string(a)));
    if(!strcmp(name,"split"))return b_texts(zn_split(b_string(a),b_string(b)));
    if(!strcmp(name,"text_join")){zn_texts parts={0};parts.len=parts.cap=a->o->len;if(parts.len>SIZE_MAX/sizeof(zn_text))zn_fail("text list too large");parts.data=(zn_text*)zn_alloc(parts.len*sizeof(zn_text));for(size_t i=0;i<parts.len;++i)parts.data[i]=b_string(&a->o->data[i]);B out=b_text(zn_text_join(parts,b_string(b)));zn_release(parts.data);return out;}
    if(!strcmp(name,"read_text"))return b_text(zn_read_text(b_string(a)));
    if(!strcmp(name,"try_read_text")){zn_io_result r=zn_read_result(b_string(a));if(r.tag!=1 && r.tag!=2)zn_fail("invalid IO result tag");B payload=b_text(r.value);return b_enum((int64_t)r.tag-1,&payload);}
    if(!strcmp(name,"write_text")){zn_write_text(b_string(a),b_string(b));return b_unit();}
    if(!strcmp(name,"int") || !strcmp(name,"i64")){if(a->kind==BI)return b_number(zn_num_cast(b_raw(a),a->slot,64),64);if(a->kind!=BF || !isfinite(a->f) || a->f<(-9223372036854775807.0-1.0) || a->f>=9223372036854775808.0)zn_fail("numeric conversion out of range");return b_int((int64_t)a->f);}
    if(!strcmp(name,"f64")){if(a->kind==BF)return *a;return b_float(a->slot>100?(double)b_raw(a):(double)b_integer(a));}
    zn_fail("unsupported codegen builtin");return b_unit();
}
static void b_enter(void) { if(++b_depth>512)zn_fail("generated call depth exceeds 512"); }
static void b_leave(void) { if(!b_depth)zn_fail("call depth underflow");--b_depth; }
static void b_clean(void) { if(b_owners || b_depth)zn_fail("generated owner accounting mismatch"); }
#endif

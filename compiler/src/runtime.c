/* Trusted native runtime for the restricted Zenith prototype. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <process.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <limits.h>
#include "runtime.h"
#include <math.h>
#include <intrin.h>
#if (defined(_M_X64) || defined(__SSE2__)) && !defined(ZN_MAP_SCALAR)
#include <emmintrin.h>
#define ZN_MAP_SSE2 1
#endif

/* One removable COMDAT per cold function's diagnostics. Keeping its byte arrays
   together removes per-literal COFF metadata without adding a pointer load or
   retaining messages from another function. selectany keeps this removable even
   for callers using the older flags without /Gw. */
#define ZN_COLD_ERROR_FIELD(name, message) char name[sizeof(message)];
#define ZN_COLD_ERROR_TEXT(name, message) message,
#define ZN_COLD_ERRORS(name, messages) \
    __declspec(selectany) const struct { messages(ZN_COLD_ERROR_FIELD) } name = { messages(ZN_COLD_ERROR_TEXT) }

/* Opaque public handles are unique tokens, never addresses of freed storage.
   Private layout: queue_next doubles as the free-list link while recycled. */
struct zn_task {
    uintptr_t token;
    struct zn_task *queue_next,*queue_prev,*registry_next;
    int64_t legacy_result;
    int64_t (*legacy_work)(void*);
    void (*work)(void*,void*);
    void* args;
    void* result;
    uint64_t width;
    volatile LONG state;
    bool claimed;
    bool recyclable;
};
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
int64_t zn_cold_add(int64_t a, int64_t b) { return zn_add(a,b); }
int64_t zn_cold_sub(int64_t a, int64_t b) { return zn_sub(a,b); }
int64_t zn_cold_mul(int64_t a, int64_t b) { return zn_mul(a,b); }
int64_t zn_cold_mul_literal(int64_t a, int64_t b) { return zn_mul_literal(a,b); }
int64_t zn_cold_div(int64_t a, int64_t b) { return zn_div(a,b); }
int64_t zn_cold_mod(int64_t a, int64_t b) { return zn_mod(a,b); }
int64_t zn_cold_neg(int64_t a) { return zn_neg(a); }
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
/* Compact only cold failure/OS paths; restore speed before hot helpers. */
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
__declspec(noreturn) void zn_fail(const char* message) { fprintf(stderr, "Zenith runtime error: %s\n", message); fflush(stderr); ExitProcess(1); }
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
void* zn_alloc(size_t n) { void* p = malloc(n ? n : 1); if (!p) zn_fail("allocation failed"); return p; }
void zn_release(void* p) { free(p); }
struct zn_channel_state {
    SRWLOCK lock;
    CONDITION_VARIABLE readable,writable;
    volatile LONG64 references;
    uint64_t capacity,width,head,len;
    bool closed;
    void (*destroy)(void*);
    unsigned char* data;
};
static zn_channel_state* zn_channel_checked(const zn_channel* owner) {
    if(!owner || !owner->state)zn_fail("use of moved channel endpoint");
    return owner->state;
}
void zn_channel_new(zn_channel* out,int64_t capacity,uint64_t width,void (*destroy)(void*)) {
    if(capacity<=0)zn_fail("channel capacity must be positive");
    if(!width || width>65536 || (uint64_t)capacity>SIZE_MAX/width || !destroy)zn_fail("channel capacity overflow or invalid layout");
    zn_channel_state* s=(zn_channel_state*)zn_alloc(sizeof(*s));memset(s,0,sizeof(*s));
    InitializeSRWLock(&s->lock);InitializeConditionVariable(&s->readable);InitializeConditionVariable(&s->writable);
    s->references=1;s->capacity=(uint64_t)capacity;s->width=width;s->destroy=destroy;
    s->data=(unsigned char*)zn_alloc((size_t)s->capacity*(size_t)width);memset(s->data,0,(size_t)s->capacity*(size_t)width);out->state=s;
}
void zn_channel_clone(zn_channel* out,const zn_channel* source) {
    zn_channel_state* s=zn_channel_checked(source);
    if(InterlockedIncrement64(&s->references)<=1)zn_fail("channel reference count overflow");out->state=s;
}
void zn_channel_drop(zn_channel* owner) {
    zn_channel_state* s=owner->state;if(!s)return;owner->state=NULL;
    if(InterlockedDecrement64(&s->references))return;
    /* Every operation retains its endpoint's source owner. With the last
       endpoint gone no operation can still access this storage. */
    for(uint64_t i=0;i<s->len;i++)s->destroy(s->data+((s->head+i)%s->capacity)*s->width);
    free(s->data);free(s);
}
bool zn_channel_send(const zn_channel* owner,void* moved_value) {
    zn_channel_state* s=zn_channel_checked(owner);AcquireSRWLockExclusive(&s->lock);
    while(s->len==s->capacity && !s->closed)if(!SleepConditionVariableSRW(&s->writable,&s->lock,INFINITE,0))zn_fail("channel send wait failed");
    bool sent=!s->closed;
    if(sent){memcpy(s->data+((s->head+s->len)%s->capacity)*s->width,moved_value,(size_t)s->width);s->len++;WakeConditionVariable(&s->readable);}
    ReleaseSRWLockExclusive(&s->lock);
    if(!sent)s->destroy(moved_value);
    memset(moved_value,0,(size_t)s->width);return sent;
}
bool zn_channel_recv(const zn_channel* owner,void* out,bool blocking) {
    zn_channel_state* s=zn_channel_checked(owner);AcquireSRWLockExclusive(&s->lock);
    while(blocking && !s->len && !s->closed)if(!SleepConditionVariableSRW(&s->readable,&s->lock,INFINITE,0))zn_fail("channel receive wait failed");
    bool received=s->len!=0;
    if(received){s->destroy(out);void* slot=s->data+s->head*s->width;memcpy(out,slot,(size_t)s->width);memset(slot,0,(size_t)s->width);s->head=(s->head+1)%s->capacity;s->len--;WakeConditionVariable(&s->writable);}
    ReleaseSRWLockExclusive(&s->lock);return received;
}
void zn_channel_close(const zn_channel* owner) {
    zn_channel_state* s=zn_channel_checked(owner);AcquireSRWLockExclusive(&s->lock);s->closed=true;
    WakeAllConditionVariable(&s->readable);WakeAllConditionVariable(&s->writable);ReleaseSRWLockExclusive(&s->lock);
}
bool zn_channel_is_closed(const zn_channel* owner) {
    zn_channel_state* s=zn_channel_checked(owner);AcquireSRWLockShared(&s->lock);bool value=s->closed;ReleaseSRWLockShared(&s->lock);return value;
}
int64_t zn_channel_len(const zn_channel* owner) {
    zn_channel_state* s=zn_channel_checked(owner);AcquireSRWLockShared(&s->lock);int64_t value=(int64_t)s->len;ReleaseSRWLockShared(&s->lock);return value;
}
int64_t zn_channel_capacity(const zn_channel* owner) {return (int64_t)zn_channel_checked(owner)->capacity;}
/* Storage is private to checked arena owners. Generated typed glue performs
   payload clone/drop; these helpers never copy an owning payload implicitly. */
/* The state allocation stores cap occupancy bytes followed by an unaligned
   uint64 free-list head and cap uint64 links. memcpy avoids alignment casts;
   a separate 64-bit sentinel permits the full 32-bit slot index space. */
static uint64_t zn_arena_link(const zn_arena* a,uint64_t index){uint64_t value;memcpy(&value,a->states+a->cap+8*index,8);return value;}
static void zn_arena_set_link(zn_arena* a,uint64_t index,uint64_t value){memcpy(a->states+a->cap+8*index,&value,8);}
bool zn_arena_contains_id(const zn_arena* a,uint64_t id) {
    uint64_t slot=(uint32_t)id;uint32_t generation=(uint32_t)(id>>32);
    return generation!=0 && slot<a->slots && a->states[slot]==1 && a->generations[slot]==generation;
}
void* zn_arena_get(const zn_arena* a,uint64_t id,uint64_t width) {
    if(!zn_arena_contains_id(a,id))zn_fail("stale or invalid arena handle");
    return (unsigned char*)a->data+(uint64_t)(uint32_t)id*width;
}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
static __declspec(noinline) void zn_arena_grow(zn_arena* a,uint64_t width,uint64_t head) {
    uint64_t cap=a->cap?a->cap*2:4;
    if(cap>UINT64_C(4294967296))cap=UINT64_C(4294967296);
    if(!width || cap>SIZE_MAX/width || cap>SIZE_MAX/sizeof(uint32_t) || cap>(SIZE_MAX-8)/9)zn_fail("arena too large");
    void* data=zn_alloc((size_t)(cap*width));
    uint32_t* generations=(uint32_t*)zn_alloc((size_t)(cap*sizeof(uint32_t)));
    unsigned char* states=(unsigned char*)zn_alloc((size_t)(cap*9+8));
    memset(data,0,(size_t)(cap*width));memset(states,0,(size_t)(cap*9+8));memset(generations,0,(size_t)(cap*sizeof(uint32_t)));
    if(a->slots){memcpy(data,a->data,(size_t)(a->slots*width));memcpy(generations,a->generations,(size_t)(a->slots*sizeof(uint32_t)));memcpy(states,a->states,(size_t)a->slots);}
    memcpy(states+cap,&head,8);
    for(uint64_t i=0;i<a->slots;i++){uint64_t link=zn_arena_link(a,i+1);memcpy(states+cap+8*(i+1),&link,8);}
    free(a->data);free(a->generations);free(a->states);a->data=data;a->generations=generations;a->states=states;a->cap=cap;
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
uint64_t zn_arena_alloc_slot(zn_arena* a,uint64_t width) {
    uint64_t head=a->cap?zn_arena_link(a,0):UINT64_MAX;
    uint64_t slot=head==UINT64_MAX?a->slots:head;
    if(head!=UINT64_MAX)zn_arena_set_link(a,0,zn_arena_link(a,slot+1));
    if(slot==a->slots){
        if(a->slots>UINT32_MAX)zn_fail("arena slot space exhausted");
        if(a->slots==a->cap)zn_arena_grow(a,width,head);
        a->generations[slot]=1;a->slots++;
    }
    a->states[slot]=1;a->len++;return ((uint64_t)a->generations[slot]<<32)|(uint32_t)slot;
}
void zn_arena_erase_slot(zn_arena* a,uint64_t id,uint64_t width) {
    if(!zn_arena_contains_id(a,id))zn_fail("stale or invalid arena handle");
    uint64_t slot=(uint32_t)id;memset((unsigned char*)a->data+slot*width,0,(size_t)width);a->len--;
    if(a->generations[slot]==UINT32_MAX)a->states[slot]=2;
    else{a->generations[slot]++;a->states[slot]=0;zn_arena_set_link(a,slot+1,zn_arena_link(a,0));zn_arena_set_link(a,0,slot);}
}
void zn_arena_check_pair(const zn_arena* a,uint64_t left,uint64_t right) {
    if(!zn_arena_contains_id(a,left)||!zn_arena_contains_id(a,right))zn_fail("stale or invalid arena handle");
    if((uint32_t)left==(uint32_t)right)zn_fail("arena pair handles must be distinct");
}
void zn_arena_clone_storage(zn_arena* out,const zn_arena* source,uint64_t width) {
    memset(out,0,sizeof(*out));if(!source->slots)return;
    if(!width || source->slots>SIZE_MAX/width)zn_fail("arena too large");
    out->data=zn_alloc((size_t)(source->slots*width));out->generations=(uint32_t*)zn_alloc((size_t)(source->slots*sizeof(uint32_t)));out->states=(unsigned char*)zn_alloc((size_t)(source->slots*9+8));
    memcpy(out->data,source->data,(size_t)(source->slots*width));memcpy(out->generations,source->generations,(size_t)(source->slots*sizeof(uint32_t)));memcpy(out->states,source->states,(size_t)source->slots);
    out->len=source->len;out->slots=out->cap=source->slots;
    zn_arena_set_link(out,0,zn_arena_link(source,0));for(uint64_t i=0;i<source->slots;i++)zn_arena_set_link(out,i+1,zn_arena_link(source,i+1));
}
void zn_arena_release(zn_arena* a){free(a->data);free(a->generations);free(a->states);memset(a,0,sizeof(*a));}
void* zn_arena_next_slot(const zn_arena* a,uint64_t* cursor,uint64_t width){while(*cursor<a->slots){uint64_t slot=(*cursor)++;if(a->states[slot]==1)return (unsigned char*)a->data+slot*width;}return NULL;}
void* zn_arena_slot_at(const zn_arena* a,uint64_t index,uint64_t width){if(index>=a->slots||a->states[index]!=1)zn_fail("invalid arena storage slot");return (unsigned char*)a->data+index*width;}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
void zn_assert(bool condition, int line, const char* message) { if (!condition) { fprintf(stderr, "line %d: ", line); zn_fail(message); } }
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
zn_text zn_text_make(const char* s) { size_t n = strlen(s); if (n > INT64_MAX) zn_fail("text too long"); zn_text t = { (char*)zn_alloc(n+1), n }; memcpy(t.data,s,n+1); return t; }
zn_text zn_text_literal(const char* s,size_t bytes) {
    if(!s || bytes>INT64_MAX)zn_fail("invalid static text layout");
    zn_text t={(char*)s,bytes|ZN_TEXT_STATIC_TAG};return t;
}
void zn_abi_text_literal(zn_text* out,const char* s,uint64_t bytes){*out=zn_text_literal(s,(size_t)bytes);}
/* Compiler-only constructor for independently bounded C literal segments. */
zn_text zn_text_parts(const char* const* parts,size_t count) {
    size_t size=0;
    for(size_t i=0;i<count;i++){size_t n=strlen(parts[i]);if(n>(size_t)INT64_MAX-size)zn_fail("text too long");size+=n;}
    zn_text out={(char*)zn_alloc(size+1),size};size_t at=0;
    for(size_t i=0;i<count;i++){size_t n=strlen(parts[i]);memcpy(out.data+at,parts[i],n);at+=n;}
    out.data[size]=0;return out;
}
void zn_text_drop(zn_text* t) { if(!(t->len&ZN_TEXT_STATIC_TAG))free(t->data); t->data = NULL; t->len = 0; }
zn_text zn_text_clone(zn_text t) {
    if(t.len&ZN_TEXT_STATIC_TAG)return t;
    if(zn_text_size(t)==SIZE_MAX || (zn_text_size(t) && !t.data)) zn_fail("invalid text storage");
    zn_text out={(char*)zn_alloc(zn_text_size(t)+1),zn_text_size(t)};
    if(zn_text_size(t)) memcpy(out.data,t.data,zn_text_size(t));
    out.data[zn_text_size(t)]=0;return out;
}
zn_text zn_concat(zn_text a, zn_text b) { size_t al=zn_text_size(a),bl=zn_text_size(b);if(al>(size_t)INT64_MAX-bl)zn_fail("text too long"); zn_text t = { (char*)zn_alloc(al+bl+1), al+bl }; memcpy(t.data,a.data,al); memcpy(t.data+al,b.data,bl); t.data[t.len] = 0; return t; }
void zn_ints_drop(zn_ints* a) { free(a->data); a->data = NULL; a->len = a->cap = 0; }
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
/* Growth policy (not a language promise): an empty owner's first allocation
   has 16 slots, then capacity doubles. The overflow checks are unchanged. */
void zn_ints_grow(zn_ints* a) {
    if (a->cap > SIZE_MAX / (2*sizeof(int64_t))) zn_fail("array too large");
    size_t cap = a->cap ? a->cap*2 : 16;
    void* p = realloc(a->data, cap*sizeof(int64_t)); if (!p) zn_fail("allocation failed");
    a->data = (int64_t*)p; a->cap = cap;
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
/* Clones allocate exact storage once (len == cap), like zn_vec_clone_storage.
   An empty source yields the empty owner {NULL,0,0}. */
zn_ints zn_ints_clone(zn_ints a) {
    zn_ints b = {0};
    if (!a.len) return b;
    if (a.len > SIZE_MAX / sizeof(int64_t)) zn_fail("array too large");
    b.data = (int64_t*)zn_alloc(a.len*sizeof(int64_t));
    memcpy(b.data, a.data, a.len*sizeof(int64_t));
    b.len = b.cap = a.len; return b;
}
/* push transfers ownership. text_at returns an explicit independent copy. */
void zn_texts_push(zn_texts* a, zn_text value) {
    if(a->len == a->cap) {
        if(a->cap > SIZE_MAX/(2*sizeof(zn_text))) zn_fail("array too large");
        size_t cap=a->cap ? a->cap*2 : 16;
        void* p=realloc(a->data,cap*sizeof(zn_text));
        if(!p) zn_fail("allocation failed");
        a->data=(zn_text*)p; a->cap=cap;
    }
    a->data[a->len++]=value;
}
void zn_texts_drop(zn_texts* a) {
    for(size_t i=0;i<a->len;i++) zn_text_drop(&a->data[i]);
    free(a->data); a->data=NULL; a->len=a->cap=0;
}
zn_texts zn_texts_clone(zn_texts a) {
    zn_texts out={0};
    if(!a.len) return out;
    if(a.len > SIZE_MAX/sizeof(zn_text)) zn_fail("array too large");
    out.data=(zn_text*)zn_alloc(a.len*sizeof(zn_text)); out.cap=a.len;
    /* Element clones run in source order, so a fault names the same element. */
    for(size_t i=0;i<a.len;i++) { out.data[i]=zn_text_clone(a.data[i]); out.len=i+1; }
    return out;
}
/* Element clone with the static-literal case inline: tagged text is shared in
   O(1) exactly as zn_text_clone returns it; owned text still gets a copy. */
static __forceinline zn_text zn_text_at_ref(const zn_texts* a,int64_t i) {
    if((uint64_t)i>=a->len) zn_fail("text collection index out of bounds");
    zn_text t=a->data[(size_t)i];
    return (t.len&ZN_TEXT_STATIC_TAG)?t:zn_text_clone(t);
}
zn_text zn_text_at(zn_texts a,int64_t i) {return zn_text_at_ref(&a,i);}
zn_texts zn_split(zn_text value,zn_text separator) {
    if(!zn_text_size(separator)) zn_fail("split separator must not be empty");
    zn_texts out={0}; size_t start=0;
    for(;;) {
        const char* next=strstr(value.data+start,separator.data);
        size_t end=next ? (size_t)(next-value.data) : zn_text_size(value);
        zn_texts_push(&out,zn_slice(value,(int64_t)start,(int64_t)end));
        if(!next) return out;
        start=end+zn_text_size(separator);
    }
}
#define ZN_POOL_MAX_WORKERS 32
#define ZN_POOL_QUEUE_LIMIT 1024
#define ZN_POOL_BUCKETS 1024
/* Results of at most ZN_TASK_INLINE_RESULT bytes live inside one fixed-size,
   16-byte-aligned task block. A bounded lock-protected free list recycles such
   blocks (memory only: tokens stay monotonic and are never reused); shutdown
   releases it. Larger results keep one exact allocation that join frees. */
#define ZN_TASK_INLINE_RESULT 64
#define ZN_TASK_FREELIST_CAP 64
/* A joiner whose task runs elsewhere spins this many pause instructions once,
   without the lock, before it sleeps on the completion condition. */
#define ZN_POOL_SPIN 1024
/* A worker that finds the queue empty polls the queue head without the lock
   for at most this many pause instructions, once per idle period, before it
   sleeps. At most ZN_POOL_MAX_SPINNERS workers poll at the same time: one
   spinner picks up the next submission without a wake syscall, and chained
   wakeups (below) bring in further workers; more spinners only added lock
   contention in measurement. */
#define ZN_POOL_IDLE_SPIN 2048
#define ZN_POOL_MAX_SPINNERS 1
static SRWLOCK zn_pool_lock=SRWLOCK_INIT;
static CONDITION_VARIABLE zn_pool_ready=CONDITION_VARIABLE_INIT,zn_pool_changed=CONDITION_VARIABLE_INIT;
static HANDLE zn_pool_workers[ZN_POOL_MAX_WORKERS];
static zn_task* zn_pool_registry[ZN_POOL_BUCKETS];
static zn_task *zn_pool_head,*zn_pool_tail,*zn_pool_free;
/* zn_pool_waiters counts joiners/shutdown currently asleep on zn_pool_changed.
   It changes only under zn_pool_lock immediately around the sleep, so a
   completion that observes zero has no sleeper to wake (no lost wakeup). */
static size_t zn_pool_queued,zn_pool_active,zn_pool_waiters,zn_pool_free_count;
/* Worker wakeup elision; every counter changes only under zn_pool_lock.
   zn_pool_idle:     workers registered asleep on zn_pool_ready (incremented
                     immediately before SleepConditionVariableSRW, decremented
                     when it returns, wakeup or spurious).
   zn_pool_spinning: workers polling the queue head without the lock; each
                     rechecks the queue under the lock before it may sleep.
   zn_pool_signals:  zn_pool_ready wakeups issued whose woken worker has not
                     yet reacquired the lock. It is raised only from 0 while
                     zn_pool_idle>0 and lowered by every return from that
                     sleep, so signals <= workers on their way back.
   A worker is "searching" while spinning or signalled. Queued work wakes one
   sleeper only when nobody is searching (a searcher always looks at the queue
   under the lock before sleeping), and a worker that takes a task while more
   remain queued wakes the next one, so idle workers join a burst one by one
   without a wake syscall per submission. With every worker busy or searching
   no wakeup is needed: each rechecks the queue before it sleeps. */
static size_t zn_pool_idle,zn_pool_spinning,zn_pool_signals;
static unsigned zn_pool_count;
static uintptr_t zn_pool_token;
static bool zn_pool_stopping;
#ifndef ZN_TASK_POOL_NO_ATEXIT
static bool zn_pool_exit_registered;
#endif
static __declspec(thread) unsigned zn_pool_executing;
static unsigned __stdcall zn_worker_entry(void* unused);
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
/* Cold: lock held, runs once per pool generation. */
static __declspec(noinline) void zn_pool_initialize(void) {
    if(zn_pool_count)return;
    SYSTEM_INFO info;GetSystemInfo(&info);
    unsigned count=info.dwNumberOfProcessors;
    if(count<2)count=2;if(count>ZN_POOL_MAX_WORKERS)count=ZN_POOL_MAX_WORKERS;
#ifndef ZN_TASK_POOL_NO_ATEXIT
    if(!zn_pool_exit_registered){if(atexit(zn_task_pool_shutdown))zn_fail("worker cleanup registration failed");zn_pool_exit_registered=true;}
#endif
    for(unsigned i=0;i<count;i++){
        HANDLE h=(HANDLE)_beginthreadex(NULL,0,zn_worker_entry,NULL,0,NULL);
        if(!h)zn_fail("thread creation failed");zn_pool_workers[zn_pool_count++]=h;
    }
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
/* Lock held. Removing a particular awaited task avoids building a deep chain
   of unrelated parent tasks while a worker is waiting for its own child. */
static zn_task* zn_pool_take(zn_task* t) {
    if(!t)t=zn_pool_head;
    if(!t || t->state)return NULL;
    if(t->queue_prev)t->queue_prev->queue_next=t->queue_next;else zn_pool_head=t->queue_next;
    if(t->queue_next)t->queue_next->queue_prev=t->queue_prev;else zn_pool_tail=t->queue_prev;
    t->queue_next=t->queue_prev=NULL;zn_pool_queued--;t->state=1;zn_pool_active++;return t;
}
/* Every execution (worker, joiner helping, saturated inline, shutdown drain)
   passes here, so zn_pool_executing marks logical-task context for exit_code.
   Called without the lock; returns with zn_pool_lock HELD (every caller needs
   it next, so completion and the caller's next step share one acquisition). */
static void zn_pool_execute(zn_task* t) {
    zn_pool_executing++;
    if(t->work)t->work(t->args,t->result);else t->legacy_result=t->legacy_work(t->args);
    zn_pool_executing--;
    AcquireSRWLockExclusive(&zn_pool_lock);t->state=2;zn_pool_active--;
    /* Only sleepers that can observe this transition are woken: claimants and
       shutdown on zn_pool_changed; workers only need it for stopping && idle. */
    if(zn_pool_waiters)WakeAllConditionVariable(&zn_pool_changed);
    if(zn_pool_stopping && !zn_pool_active)WakeAllConditionVariable(&zn_pool_ready);
}
/* Lock held. Decides whether queued work needs one more worker; the caller
   issues the wakeup after releasing the lock (see zn_pool_idle). */
static __forceinline bool zn_pool_need_searcher(void) {
    if(!zn_pool_head || zn_pool_spinning || zn_pool_signals || !zn_pool_idle)return false;
    zn_pool_signals++;return true;
}
static unsigned __stdcall zn_worker_entry(void* unused) {
    (void)unused;
    bool spun=false;
    AcquireSRWLockExclusive(&zn_pool_lock);
    for(;;){
        zn_task* t=zn_pool_take(NULL);
        if(t){
            bool wake=zn_pool_need_searcher();spun=false;
            ReleaseSRWLockExclusive(&zn_pool_lock);
            if(wake)WakeConditionVariable(&zn_pool_ready);
            zn_pool_execute(t);continue;
        }
        if(zn_pool_stopping){
            if(!zn_pool_active){ReleaseSRWLockExclusive(&zn_pool_lock);return 0;}
        }else if(!spun && zn_pool_spinning<ZN_POOL_MAX_SPINNERS){
            /* Lock-free reads are only hints; the loop retakes under the lock. */
            spun=true;zn_pool_spinning++;ReleaseSRWLockExclusive(&zn_pool_lock);
            for(unsigned i=0;i<ZN_POOL_IDLE_SPIN && !*(zn_task* volatile*)&zn_pool_head && !*(volatile bool*)&zn_pool_stopping;i++)YieldProcessor();
            AcquireSRWLockExclusive(&zn_pool_lock);zn_pool_spinning--;
            continue;
        }
        zn_pool_idle++;
        BOOL woke=SleepConditionVariableSRW(&zn_pool_ready,&zn_pool_lock,INFINITE,0);
        zn_pool_idle--;if(zn_pool_signals)zn_pool_signals--;
        if(!woke)zn_fail("worker wait failed");
        spun=false;
    }
}
#define ZN_TASK_HEADER (((size_t)sizeof(zn_task)+15)&~(size_t)15)
static zn_task* zn_task_allocate(uint64_t width) {
    bool small=width<=ZN_TASK_INLINE_RESULT;
    size_t bytes=ZN_TASK_HEADER+(size_t)(small?ZN_TASK_INLINE_RESULT:width);
    zn_task* t=(zn_task*)zn_alloc(bytes);memset(t,0,bytes);t->recyclable=small;return t;
}
/* Lock held. Returns a block to free after unlocking, or NULL once cached. */
static zn_task* zn_task_retire(zn_task* t) {
    if(!t->recyclable || zn_pool_free_count>=ZN_TASK_FREELIST_CAP || !zn_pool_count)return t;
    t->queue_next=zn_pool_free;zn_pool_free=t;zn_pool_free_count++;return NULL;
}
static zn_task* zn_task_submit(int64_t (*legacy)(void*),void (*work)(void*,void*),void* args,uint64_t width) {
    zn_task* t=NULL;
    AcquireSRWLockExclusive(&zn_pool_lock);
    if(width<=ZN_TASK_INLINE_RESULT && zn_pool_free){
        t=zn_pool_free;zn_pool_free=t->queue_next;zn_pool_free_count--;
        memset(t,0,ZN_TASK_HEADER+ZN_TASK_INLINE_RESULT);t->recyclable=true;
    }else{
        /* Allocation stays outside the lock; only the first batch reaches it. */
        ReleaseSRWLockExclusive(&zn_pool_lock);t=zn_task_allocate(width);AcquireSRWLockExclusive(&zn_pool_lock);
    }
    t->result=(unsigned char*)t+ZN_TASK_HEADER;t->width=width;t->legacy_work=legacy;t->work=work;t->args=args;
    if(zn_pool_stopping && !zn_pool_executing)zn_fail("worker pool shutting down");
    zn_pool_initialize();
    if(zn_pool_token==(uintptr_t)INT64_MAX)zn_fail("worker handle space exhausted");
    t->token=++zn_pool_token;size_t bucket=t->token&(ZN_POOL_BUCKETS-1);
    t->registry_next=zn_pool_registry[bucket];zn_pool_registry[bucket]=t;
    zn_task* handle=(zn_task*)t->token;
    bool inline_work=zn_pool_queued==ZN_POOL_QUEUE_LIMIT,wake=false;
    if(inline_work){t->state=1;zn_pool_active++;}
    else{
        t->queue_prev=zn_pool_tail;
        if(zn_pool_tail)zn_pool_tail->queue_next=t;else zn_pool_head=t;
        zn_pool_tail=t;zn_pool_queued++;wake=zn_pool_need_searcher();
    }
    ReleaseSRWLockExclusive(&zn_pool_lock);
    if(wake)WakeConditionVariable(&zn_pool_ready);
    if(inline_work){zn_pool_execute(t);ReleaseSRWLockExclusive(&zn_pool_lock);}
    return handle;
}
static zn_task** zn_task_lookup(zn_task* handle) {
    uintptr_t token=(uintptr_t)handle;
    zn_task** slot=&zn_pool_registry[token&(ZN_POOL_BUCKETS-1)];
    while(*slot && (*slot)->token!=token)slot=&(*slot)->registry_next;
    return slot;
}
/* Claim and validate before dereferencing task metadata. Consumed tokens are
   removed, never reused, so stale/double/forged handles cannot reach freed data.
   Returns with zn_pool_lock HELD; the claimed task is private to the caller. */
static zn_task* zn_task_wait(zn_task* handle,int kind,uint64_t width) {
    AcquireSRWLockExclusive(&zn_pool_lock);
    zn_task* t=*zn_task_lookup(handle);
    if(!t || t->claimed)zn_fail("invalid or already joined worker handle");
    if((kind==0 && t->work) || (kind==1 && (!t->work || t->width!=width)))zn_fail("worker result type mismatch");
    t->claimed=true;
    bool spun=false;
    while(t->state!=2){
        /* A still-queued claimed task runs inline at once; otherwise help. */
        zn_task* next=t->state==0?zn_pool_take(t):zn_pool_take(NULL);
        if(next){ReleaseSRWLockExclusive(&zn_pool_lock);zn_pool_execute(next);}
        else if(!spun){
            /* Claimed storage cannot be freed or recycled by anyone else, so a
               lock-free volatile read of state is only a hint; the loop rechecks
               it under the lock. New queued work also ends the spin. */
            spun=true;ReleaseSRWLockExclusive(&zn_pool_lock);
            for(unsigned i=0;i<ZN_POOL_SPIN && t->state!=2 && !*(zn_task* volatile*)&zn_pool_head;i++)YieldProcessor();
            AcquireSRWLockExclusive(&zn_pool_lock);
        }else{
            zn_pool_waiters++;
            BOOL woke=SleepConditionVariableSRW(&zn_pool_changed,&zn_pool_lock,INFINITE,0);
            zn_pool_waiters--;if(!woke)zn_fail("worker join failed");
        }
    }
    zn_task** slot=zn_task_lookup(handle);*slot=t->registry_next;
    return t;
}
zn_task* zn_start(int64_t (*work)(void*),void* args) {
    if(!work)zn_fail("invalid worker callback");
    return zn_task_submit(work,NULL,args,0);
}
int64_t zn_join(zn_task* handle) {
    zn_task* t=zn_task_wait(handle,0,0);int64_t result=t->legacy_result;
    t=zn_task_retire(t);ReleaseSRWLockExclusive(&zn_pool_lock);free(t);return result;
}
zn_task* zn_start_owned(void (*work)(void*,void*),void* packet,uint64_t width) {
    if(!work || !packet || width>65536)zn_fail("invalid worker packet or result layout");
    return zn_task_submit(NULL,work,packet,width);
}
void zn_join_owned(void* out,zn_task* handle,uint64_t width) {
    if(width && !out)zn_fail("worker result type mismatch");
    zn_task* t=zn_task_wait(handle,1,width);
    if(!t->recyclable){ReleaseSRWLockExclusive(&zn_pool_lock);memcpy(out,t->result,(size_t)width);free(t);return;}
    if(width)memcpy(out,t->result,(size_t)width);
    t=zn_task_retire(t);ReleaseSRWLockExclusive(&zn_pool_lock);free(t);
}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
void zn_task_drop(zn_task** owner) {
    if(!owner)zn_fail("invalid worker owner");zn_task* handle=*owner;if(!handle)return;*owner=NULL;
    zn_task* t=zn_task_wait(handle,2,0);ReleaseSRWLockExclusive(&zn_pool_lock);
    if(t->work)t->work(NULL,t->result);
    AcquireSRWLockExclusive(&zn_pool_lock);t=zn_task_retire(t);ReleaseSRWLockExclusive(&zn_pool_lock);free(t);
}
void zn_task_pool_shutdown(void) {
    if(zn_pool_executing)zn_fail("worker pool shutdown requires a quiescent host");
    AcquireSRWLockExclusive(&zn_pool_lock);
    if(!zn_pool_count){ReleaseSRWLockExclusive(&zn_pool_lock);return;}
    if(zn_pool_stopping)zn_fail("worker pool shutdown already in progress");
    zn_pool_stopping=true;WakeAllConditionVariable(&zn_pool_ready);
    while(zn_pool_head || zn_pool_active){
        zn_task* next=zn_pool_take(NULL);
        if(next){ReleaseSRWLockExclusive(&zn_pool_lock);zn_pool_execute(next);}
        else{
            zn_pool_waiters++;
            BOOL woke=SleepConditionVariableSRW(&zn_pool_changed,&zn_pool_lock,INFINITE,0);
            zn_pool_waiters--;if(!woke)zn_fail("worker shutdown wait failed");
        }
    }
    unsigned count=zn_pool_count;ReleaseSRWLockExclusive(&zn_pool_lock);
    for(unsigned i=0;i<count;i++){
        if(WaitForSingleObject(zn_pool_workers[i],INFINITE)!=WAIT_OBJECT_0 || !CloseHandle(zn_pool_workers[i]))zn_fail("worker close failed");
        zn_pool_workers[i]=NULL;
    }
    /* A typed result destructor may itself consume another completed task.
       Remove one registry entry at a time and keep callbacks outside the lock. */
    for(size_t i=0;i<ZN_POOL_BUCKETS;i++)for(;;){
        AcquireSRWLockExclusive(&zn_pool_lock);zn_task* t=zn_pool_registry[i];
        if(t)zn_pool_registry[i]=t->registry_next;ReleaseSRWLockExclusive(&zn_pool_lock);
        if(!t)break;if(t->claimed)zn_fail("worker shutdown requires a quiescent host");
        if(t->work)t->work(NULL,t->result);free(t);
    }
    /* Destructors above may have recycled blocks; release the whole cache. */
    AcquireSRWLockExclusive(&zn_pool_lock);
    zn_task* cached=zn_pool_free;zn_pool_free=NULL;zn_pool_free_count=0;zn_pool_count=0;zn_pool_stopping=false;
    ReleaseSRWLockExclusive(&zn_pool_lock);
    while(cached){zn_task* next=cached->queue_next;free(cached);cached=next;}
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
void zn_print_int(int64_t v) { printf("%" PRId64 "\n", v); }
void zn_print_uint(uint64_t v) { printf("%" PRIu64 "\n", v); }
void zn_span_region(zn_span* out,void* data,uint64_t len,int64_t start,int64_t end,uint64_t width) {
    if(start<0 || end<start || (uint64_t)end>len) zn_fail("slice range out of bounds");
    /* Do not perform even +0 pointer arithmetic on an empty null allocation. */
    out->data=data ? (unsigned char*)data+(size_t)start*width : NULL;
    out->len=(uint64_t)(end-start);
}
zn_span zn_span_make(void* data,uint64_t len,int64_t start,int64_t end,uint64_t width) {
    zn_span out;zn_span_region(&out,data,len,start,end,width);return out;
}
void zn_span_drop(zn_span* p) { (void)p; }
void zn_vector_view_region(zn_span* out,void* data,uint64_t len,int64_t start,int64_t count,uint64_t width) {
    if(start<0 || count<0 || (uint64_t)start>len || (uint64_t)count>len-(uint64_t)start)
        zn_fail("vector region start/count out of bounds or overflow");
    if(!width || len>SIZE_MAX/width || (len && !data))zn_fail("vector region storage overflow");
    out->data=data ? (unsigned char*)data+(uint64_t)start*width : NULL;out->len=(uint64_t)count;
}
zn_span zn_vector_view_make(void* data,uint64_t len,int64_t start,int64_t count,uint64_t width) {
    zn_span out;zn_vector_view_region(&out,data,len,start,count,width);return out;
}
void* zn_span_slot(const zn_span* a,int64_t index,uint64_t width) {
    if(index<0 || (uint64_t)index>=a->len)zn_fail("vector region index out of bounds");
    if(!width || a->len>SIZE_MAX/width || !a->data)zn_fail("vector region storage overflow");
    return (unsigned char*)a->data+(uint64_t)index*width;
}
void zn_span_check_pair(const zn_span* a,int64_t left,int64_t right) {
    if(left<0 || right<0 || (uint64_t)left>=a->len || (uint64_t)right>=a->len)zn_fail("vector region index out of bounds");
    if(left==right)zn_fail("vector pair indices must be distinct");
}

/* Integer registers are canonical 64-bit values. Only aggregate storage is
   narrow. All conversions are checked; memcpy avoids aliasing and signed-cast
   implementation dependencies. The target ABI is Windows x64, little endian. */
int64_t zn_num_signed(uint64_t raw) { int64_t v; memcpy(&v,&raw,8); return v; }
static unsigned zn_num_bits(uint64_t code) { return (unsigned)(code>100 ? code-100 : code); }
static uint64_t zn_num_max(uint64_t code) {
    unsigned bits=zn_num_bits(code);
    return code>100 ? (bits==64 ? UINT64_MAX : (UINT64_C(1)<<bits)-1) : (UINT64_C(1)<<(bits-1))-1;
}
static int64_t zn_num_min(uint64_t code) { return -((int64_t)zn_num_max(code))-1; }
uint64_t zn_num_cast(uint64_t raw,uint64_t from,uint64_t to) {
    if(from<100 && zn_num_signed(raw)<0) {
        if(to>100 || zn_num_signed(raw)<zn_num_min(to)) zn_fail("integer conversion out of range");
    } else if(raw>zn_num_max(to)) zn_fail("integer conversion out of range");
    return raw;
}
uint64_t zn_num_binary(uint64_t a,uint64_t b,uint64_t code,uint64_t op) {
    uint64_t max=zn_num_max(code);
    if(code>100) {
        switch(op) {
        case 0: if(a>max-b) zn_fail("integer overflow"); return a+b;
        case 1: if(a<b) zn_fail("integer overflow"); return a-b;
        case 2: if(b && a>max/b) zn_fail("integer overflow"); return a*b;
        case 3: if(!b) zn_fail("division by zero"); return a/b;
        case 4: if(!b) zn_fail("remainder by zero"); return a%b;
        case 5: return a>max-b ? max : a+b;
        case 6: return a<b ? 0 : a-b;
        }
    } else {
        int64_t x=zn_num_signed(a),y=zn_num_signed(b),r=0,min=zn_num_min(code),hi=(int64_t)max;
        if((op==3 || op==4) && x==min && y==-1) zn_fail("integer overflow");
        switch(op) {
        case 0: r=zn_add(x,y); break;
        case 1: r=zn_sub(x,y); break;
        case 2: r=zn_mul(x,y); break;
        case 3: r=zn_div(x,y); break;
        case 4: r=zn_mod(x,y); break;
        case 5:
            if(y>0 && x>hi-y) return (uint64_t)hi;
            if(y<0 && x<min-y) return (uint64_t)min;
            return (uint64_t)(x+y);
        case 6:
            if(y<0 && x>hi+y) return (uint64_t)hi;
            if(y>0 && x<min+y) return (uint64_t)min;
            return (uint64_t)(x-y);
        }
        if(r<min || r>hi) zn_fail("integer overflow");
        return (uint64_t)r;
    }
    zn_fail("invalid integer operation");
}
void zn_fixed_fill(void* data,uint64_t value,uint64_t len,uint64_t code) {
    size_t width=zn_num_bits(code)/8;
    for(size_t i=0;i<len;i++) memcpy((unsigned char*)data+i*width,&value,width);
}
uint64_t zn_fixed_at(const void* data,uint64_t len,int64_t index,uint64_t code) {
    if((uint64_t)index>=len) zn_fail("array index out of bounds");
    unsigned bits=zn_num_bits(code); uint64_t value=0;
    memcpy(&value,(const unsigned char*)data+(size_t)index*(bits/8),bits/8);
    if(code<100 && bits<64 && (value & (UINT64_C(1)<<(bits-1)))) value|=UINT64_MAX<<bits;
    return value;
}
bool zn_fixed_try_set(void* data,uint64_t len,int64_t index,uint64_t code,uint64_t value) {
    if((uint64_t)index>=len) return false;
    size_t width=zn_num_bits(code)/8;
    memcpy((unsigned char*)data+(size_t)index*width,&value,width); return true;
}
void zn_fixed_set(void* data,uint64_t len,int64_t index,uint64_t code,uint64_t value) {
    if(!zn_fixed_try_set(data,len,index,code,value)) zn_fail("array index out of bounds");
}
uint64_t zn_wire_read(const void* data,uint64_t len,int64_t offset,uint64_t width) {
    if(width>len || (uint64_t)offset>len-width) zn_fail("packet read out of bounds");
    uint64_t value=0; const unsigned char* p=(const unsigned char*)data+(size_t)offset;
    for(unsigned i=0;i<width;i++) value|=(uint64_t)p[i]<<(8*i);
    return value;
}
bool zn_wire_write(void* data,uint64_t len,int64_t offset,uint64_t width,uint64_t value) {
    if(width>len || (uint64_t)offset>len-width) return false;
    unsigned char* p=(unsigned char*)data+(size_t)offset;
    for(unsigned i=0;i<width;i++) p[i]=(unsigned char)(value>>(8*i));
    return true;
}
void zn_print_bool(bool v) { puts(v ? "true" : "false"); }
void zn_print_text(zn_text t) { printf("%s\n", t.data); }

/* Initialized before any user thread is started; immutable until process exit.
   Each arg() call owns a newly allocated UTF-8 copy, never argv storage. */
static int zn_argc;
static wchar_t** zn_argv;
static DWORD zn_main_thread;
static int zn_exit_status;
void zn_args_init(int argc, wchar_t** argv) { zn_argc = argc; zn_argv = argv; zn_main_thread=GetCurrentThreadId();zn_exit_status=0; }
int64_t zn_monotonic_ms(void) { ULONGLONG ticks=GetTickCount64();if(ticks>INT64_MAX)zn_fail("monotonic clock exceeds int range");return (int64_t)ticks; }
void zn_exit_code(int64_t code) { if(code<0 || code>255)zn_fail("exit code must be from 0 to 255");if(zn_pool_executing || GetCurrentThreadId()!=zn_main_thread)zn_fail("exit_code is restricted to the main thread");zn_exit_status=(int)code; }
int zn_process_exit_code(void) { return zn_exit_status; }
int64_t zn_args_count(void) { return zn_argc > 0 ? zn_argc - 1 : 0; }
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
int64_t zn_internal_test_selector(void) {
    if(GetCurrentThreadId()!=zn_main_thread || zn_argc!=2)zn_fail("invalid internal test selector invocation");
    zn_text text=zn_arg(0);int64_t selector=zn_parse_int(text);zn_text_drop(&text);
    if(selector<0)zn_fail("invalid internal test selector");zn_argc=1;return selector;
}
int64_t zn_abi_internal_test_selector(void) { return zn_internal_test_selector(); }
zn_text zn_arg(int64_t i) {
    if ((uint64_t)i >= (uint64_t)zn_args_count()) zn_fail("argument index out of bounds");
    const wchar_t* w = zn_argv[(size_t)i + 1];
    int bytes = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, w, -1, NULL, 0, NULL, NULL);
    if (!bytes) zn_fail("invalid Unicode argument");
    zn_text t = { (char*)zn_alloc((size_t)bytes), (size_t)bytes - 1 };
    if (!WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, w, -1, t.data, bytes, NULL, NULL)) zn_fail("argument conversion failed");
    return t;
}
static wchar_t* zn_path(zn_text path) {
    if (!zn_text_size(path) || zn_text_size(path) > INT_MAX || memchr(path.data, 0, zn_text_size(path))) return NULL;
    int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path.data, (int)zn_text_size(path), NULL, 0);
    if (!count) return NULL;
    wchar_t* out = (wchar_t*)zn_alloc(((size_t)count + 1) * sizeof(wchar_t));
    if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path.data, (int)zn_text_size(path), out, count)) { free(out); return NULL; }
    out[count] = 0;
    return out;
}
bool zn_file_exists(zn_text path) {
    wchar_t* name=zn_path(path);if(!name)zn_fail("invalid UTF-8 file path");
    DWORD attributes=GetFileAttributesW(name);DWORD error=GetLastError();free(name);
    if(attributes!=INVALID_FILE_ATTRIBUTES)return true;
    if(error==ERROR_FILE_NOT_FOUND || error==ERROR_PATH_NOT_FOUND)return false;
    zn_fail("cannot inspect file path");
}
static int zn_directory_compare(const void* left,const void* right) {
    const zn_text* a=(const zn_text*)left;const zn_text* b=(const zn_text*)right;
    size_t al=zn_text_size(*a),bl=zn_text_size(*b),width=al<bl?al:bl;int order=width?memcmp(a->data,b->data,width):0;
    return order?order:(al>bl)-(al<bl);
}
#define ZN_COLD_MESSAGES_ZN_LIST_DIR(X) \
    X(invalid_utf_8_directory_path, "invalid UTF-8 directory path") \
    X(cannot_open_directory, "cannot open directory") \
    X(directory_path_too_long, "directory path too long") \
    X(cannot_enumerate_directory, "cannot enumerate directory") \
    X(directory_entry_is_not_valid_unicode, "directory entry is not valid Unicode") \
    X(directory_unicode_conversion_failed, "directory Unicode conversion failed") \
    X(directory_enumeration_failed, "directory enumeration failed")
ZN_COLD_ERRORS(zn_cold_errors_list_dir, ZN_COLD_MESSAGES_ZN_LIST_DIR);
#undef ZN_COLD_MESSAGES_ZN_LIST_DIR

zn_texts zn_list_dir(zn_text path) {
    wchar_t* name=zn_path(path);if(!name)zn_fail(zn_cold_errors_list_dir.invalid_utf_8_directory_path);
    DWORD attributes=GetFileAttributesW(name);
    if(attributes==INVALID_FILE_ATTRIBUTES || !(attributes&FILE_ATTRIBUTE_DIRECTORY)){free(name);zn_fail(zn_cold_errors_list_dir.cannot_open_directory);}
    size_t len=wcslen(name);if(len>(SIZE_MAX/sizeof(wchar_t))-3){free(name);zn_fail(zn_cold_errors_list_dir.directory_path_too_long);}
    wchar_t* pattern=(wchar_t*)zn_alloc((len+3)*sizeof(wchar_t));memcpy(pattern,name,len*sizeof(wchar_t));free(name);
    if(len && pattern[len-1]!=L'\\' && pattern[len-1]!=L'/')pattern[len++]=L'\\';pattern[len++]=L'*';pattern[len]=0;
    WIN32_FIND_DATAW found;HANDLE handle=FindFirstFileW(pattern,&found);DWORD error=GetLastError();free(pattern);zn_texts result={0};
    if(handle==INVALID_HANDLE_VALUE){if(error==ERROR_FILE_NOT_FOUND)return result;zn_fail(zn_cold_errors_list_dir.cannot_enumerate_directory);}
    for(;;){
        if(wcscmp(found.cFileName,L".") && wcscmp(found.cFileName,L"..")){
            int bytes=WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,found.cFileName,-1,NULL,0,NULL,NULL);
            if(!bytes){FindClose(handle);zn_texts_drop(&result);zn_fail(zn_cold_errors_list_dir.directory_entry_is_not_valid_unicode);}
            zn_text entry={(char*)zn_alloc((size_t)bytes),(size_t)bytes-1};
            if(!WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,found.cFileName,-1,entry.data,bytes,NULL,NULL)){FindClose(handle);zn_text_drop(&entry);zn_texts_drop(&result);zn_fail(zn_cold_errors_list_dir.directory_unicode_conversion_failed);}
            zn_texts_push(&result,entry);
        }
        if(!FindNextFileW(handle,&found)){error=GetLastError();break;}
    }
    if(!FindClose(handle) || error!=ERROR_NO_MORE_FILES){zn_texts_drop(&result);zn_fail(zn_cold_errors_list_dir.directory_enumeration_failed);}
    if(result.len>1)qsort(result.data,result.len,sizeof(zn_text),zn_directory_compare);return result;
}
static zn_io_result zn_io_error(const char* message) { zn_io_result r={2,zn_text_make(message)}; return r; }
#define ZN_COLD_MESSAGES_ZN_READ_RESULT(X) \
    X(invalid_utf_8_file_path, "invalid UTF-8 file path") \
    X(cannot_open_input_file, "cannot open input file") \
    X(cannot_seek_input_file, "cannot seek input file") \
    X(input_text_limit_is_64_mib, "input text limit is 64 MiB") \
    X(input_file_changed_or_read_failed, "input file changed or read failed") \
    X(input_must_be_utf_8_text_without_nul_bytes, "input must be UTF-8 text without NUL bytes")
ZN_COLD_ERRORS(zn_cold_errors_read_result, ZN_COLD_MESSAGES_ZN_READ_RESULT);
#undef ZN_COLD_MESSAGES_ZN_READ_RESULT

zn_io_result zn_read_result(zn_text path) {
    wchar_t* name = zn_path(path);
    if(!name) return zn_io_error(zn_cold_errors_read_result.invalid_utf_8_file_path);
    FILE* file = NULL;
    errno_t error = _wfopen_s(&file, name, L"rb");
    free(name);
    if (error || !file) return zn_io_error(zn_cold_errors_read_result.cannot_open_input_file);
    if (_fseeki64(file, 0, SEEK_END)) { fclose(file); return zn_io_error(zn_cold_errors_read_result.cannot_seek_input_file); }
    __int64 size = _ftelli64(file);
    if (size < 0 || size > 64 * 1024 * 1024) { fclose(file); return zn_io_error(zn_cold_errors_read_result.input_text_limit_is_64_mib); }
    if (_fseeki64(file, 0, SEEK_SET)) { fclose(file); return zn_io_error(zn_cold_errors_read_result.cannot_seek_input_file); }
    zn_text t = { (char*)zn_alloc((size_t)size + 1), (size_t)size };
    size_t got = fread(t.data, 1, t.len, file);
    int failed = ferror(file);
    if (fclose(file)) failed = 1;
    if (failed || got != t.len) { free(t.data); return zn_io_error(zn_cold_errors_read_result.input_file_changed_or_read_failed); }
    t.data[t.len] = 0;
    if (memchr(t.data, 0, t.len) || (t.len && !MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, t.data, (int)t.len, NULL, 0))) {
        free(t.data); return zn_io_error(zn_cold_errors_read_result.input_must_be_utf_8_text_without_nul_bytes);
    }
    zn_io_result result={1,t}; return result;
}
#define ZN_COLD_MESSAGES_ZN_WRITE_RESULT(X) \
    X(invalid_utf_8_file_path, "invalid UTF-8 file path") \
    X(cannot_open_output_file, "cannot open output file") \
    X(cannot_write_output_file, "cannot write output file")
ZN_COLD_ERRORS(zn_cold_errors_write_result, ZN_COLD_MESSAGES_ZN_WRITE_RESULT);
#undef ZN_COLD_MESSAGES_ZN_WRITE_RESULT

zn_io_result zn_write_result(zn_text path, zn_text data) {
    wchar_t* name = zn_path(path);
    if(!name) return zn_io_error(zn_cold_errors_write_result.invalid_utf_8_file_path);
    FILE* file = NULL;
    errno_t error = _wfopen_s(&file, name, L"wb");
    free(name);
    if (error || !file) return zn_io_error(zn_cold_errors_write_result.cannot_open_output_file);
    size_t written = fwrite(data.data, 1, zn_text_size(data), file);
    int failed = written != zn_text_size(data);
    if (fclose(file)) failed = 1;
    if (failed) return zn_io_error(zn_cold_errors_write_result.cannot_write_output_file);
    zn_io_result result={1,{0}}; return result;
}
zn_text zn_read_text(zn_text path) { zn_io_result r=zn_read_result(path); if(r.tag==2) zn_fail(r.value.data); return r.value; }
void zn_write_text(zn_text path,zn_text text) { zn_io_result r=zn_write_result(path,text); if(r.tag==2) zn_fail(r.value.data); }
/* Use memcpy across the compiler's equivalent tagged-union layout: no C
   aliasing assumption about the anonymous generated data types is needed. */
void zn_abi_try_read_text(void* out,const zn_text* path) {zn_io_result r=zn_read_result(*path); memcpy(out,&r,sizeof(r));}
void zn_abi_try_write_text(void* out,const zn_text* path,const zn_text* text) {zn_io_result r=zn_write_result(*path,*text); memcpy(out,&r,sizeof(r));}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
bool zn_text_eq(zn_text a, zn_text b) { size_t n=zn_text_size(a); return n == zn_text_size(b) && (!n || a.data == b.data || !memcmp(a.data, b.data, n)); }
bool zn_contains(zn_text a, zn_text b) { return strstr(a.data, b.data) != NULL; }
zn_text zn_slice(zn_text t, int64_t start, int64_t end) {
    if (start < 0 || end < start || (uint64_t)end > zn_text_size(t)) zn_fail("text slice out of bounds");
    if (((size_t)start < zn_text_size(t) && ((unsigned char)t.data[start] & 0xc0) == 0x80)
        || ((size_t)end < zn_text_size(t) && ((unsigned char)t.data[end] & 0xc0) == 0x80)) zn_fail("text slice splits a UTF-8 character");
    size_t count = (size_t)(end-start);
    zn_text out = { (char*)zn_alloc(count + 1), count };
    memcpy(out.data, t.data + start, count); out.data[count] = 0;
    return out;
}
/* Text offsets are UTF-8 bytes. Character traversal operates on scalar values,
   not grapheme clusters, and never yields a pointer into owned storage. */
bool zn_is_char_boundary(zn_text t,int64_t at) {
    return at>=0 && (uint64_t)at<=zn_text_size(t)
        && ((uint64_t)at==zn_text_size(t) || ((unsigned char)t.data[(size_t)at]&0xc0)!=0x80);
}
static size_t zn_utf8_step(zn_text t,size_t at,uint32_t* out) {
    if(at>=zn_text_size(t))zn_fail("character offset out of bounds");
    unsigned char first=(unsigned char)t.data[at];
    size_t width;uint32_t cp,min;
    if(first>0 && first<0x80){width=1;cp=first;min=1;}
    else if(first>=0xc2 && first<=0xdf){width=2;cp=first&0x1f;min=0x80;}
    else if(first>=0xe0 && first<=0xef){width=3;cp=first&0x0f;min=0x800;}
    else if(first>=0xf0 && first<=0xf4){width=4;cp=first&7;min=0x10000;}
    else {zn_fail("invalid UTF-8 character boundary");}
    if(width>zn_text_size(t)-at)zn_fail("incomplete UTF-8 character");
    for(size_t i=1;i<width;i++){
        unsigned char c=(unsigned char)t.data[at+i];
        if((c&0xc0)!=0x80)zn_fail("invalid UTF-8 character");
        cp=(cp<<6)|(c&0x3f);
    }
    if(cp<min || cp>0x10ffff || (cp>=0xd800 && cp<=0xdfff))zn_fail("invalid Unicode scalar");
    *out=cp;return at+width;
}
int64_t zn_char_count(zn_text t) {
    size_t i=0,n=0;uint32_t cp;
    while(i<zn_text_size(t)){i=zn_utf8_step(t,i,&cp);n++;}
    return zn_length(n);
}
int64_t zn_codepoint_at(zn_text t,int64_t at) {
    if(at<0)zn_fail("character offset out of bounds");
    uint32_t cp;zn_utf8_step(t,(size_t)at,&cp);return cp;
}
int64_t zn_next_char_boundary(zn_text t,int64_t at) {
    if(!zn_is_char_boundary(t,at))zn_fail("invalid UTF-8 character boundary");
    if((uint64_t)at==zn_text_size(t))return at;
    uint32_t cp;return zn_length(zn_utf8_step(t,(size_t)at,&cp));
}
zn_text zn_codepoint_text(int64_t value) {
    if(value<=0 || value>0x10ffff || (value>=0xd800 && value<=0xdfff))
        zn_fail("codepoint must be a non-NUL Unicode scalar");
    uint32_t cp=(uint32_t)value;unsigned char b[5]={0};size_t n;
    if(cp<0x80){n=1;b[0]=(unsigned char)cp;}
    else if(cp<0x800){n=2;b[0]=(unsigned char)(0xc0|(cp>>6));b[1]=(unsigned char)(0x80|(cp&63));}
    else if(cp<0x10000){n=3;b[0]=(unsigned char)(0xe0|(cp>>12));b[1]=(unsigned char)(0x80|((cp>>6)&63));b[2]=(unsigned char)(0x80|(cp&63));}
    else {n=4;b[0]=(unsigned char)(0xf0|(cp>>18));b[1]=(unsigned char)(0x80|((cp>>12)&63));b[2]=(unsigned char)(0x80|((cp>>6)&63));b[3]=(unsigned char)(0x80|(cp&63));}
    zn_text out={(char*)zn_alloc(n+1),n};memcpy(out.data,b,n+1);return out;
}
static size_t zn_find_bytes(zn_text t,zn_text needle,size_t start) {
    if(!zn_text_size(needle))return start;
    if(zn_text_size(needle)>zn_text_size(t)-start)return SIZE_MAX;
    size_t limit=zn_text_size(t)-zn_text_size(needle);
    while(start<=limit){
        const char* p=(const char*)memchr(t.data+start,(unsigned char)needle.data[0],limit-start+1);
        if(!p)return SIZE_MAX;
        size_t at=(size_t)(p-t.data);
        if(!memcmp(p,needle.data,zn_text_size(needle)))return at;
        start=at+1;
    }
    return SIZE_MAX;
}
int64_t zn_text_find(zn_text t,zn_text needle,int64_t start) {
    if(!zn_is_char_boundary(t,start))zn_fail("search start must be a UTF-8 boundary in range");
    size_t at=zn_find_bytes(t,needle,(size_t)start);return at==SIZE_MAX?-1:zn_length(at);
}
bool zn_text_starts_with(zn_text t,zn_text prefix) {
    return zn_text_size(prefix)<=zn_text_size(t) && !memcmp(t.data,prefix.data,zn_text_size(prefix));
}
bool zn_text_ends_with(zn_text t,zn_text suffix) {
    return zn_text_size(suffix)<=zn_text_size(t) && !memcmp(t.data+zn_text_size(t)-zn_text_size(suffix),suffix.data,zn_text_size(suffix));
}
/* Fixed White_Space set; no locale-dependent classification or case folding. */
static bool zn_unicode_space(uint32_t c) {
    return (c>=9 && c<=13) || c==32 || c==0x85 || c==0xa0 || c==0x1680
        || (c>=0x2000 && c<=0x200a) || c==0x2028 || c==0x2029
        || c==0x202f || c==0x205f || c==0x3000;
}
zn_text zn_text_trim(zn_text t) {
    size_t at=0,start=zn_text_size(t),end=zn_text_size(t);bool found=false;
    while(at<zn_text_size(t)){
        uint32_t c;size_t next=zn_utf8_step(t,at,&c);
        if(!zn_unicode_space(c)){if(!found){start=at;found=true;}end=next;}
        at=next;
    }
    return zn_slice(t,zn_length(start),zn_length(end));
}
static size_t zn_replaced_size(size_t len,size_t count,size_t before,size_t after) {
    if(after>before){size_t extra=after-before;
        if(count>((size_t)INT64_MAX-len)/extra)zn_fail("replacement text too long");
        return len+count*extra;
    }
    return len-count*(before-after);
}
zn_text zn_text_replace(zn_text t,zn_text needle,zn_text replacement) {
    zn_length(zn_text_size(t));size_t count=0,start=0,match;
    if(!zn_text_size(needle)){count=(size_t)zn_char_count(t)+1;}
    else while((match=zn_find_bytes(t,needle,start))!=SIZE_MAX){count++;start=match+zn_text_size(needle);}
    size_t size=zn_replaced_size(zn_text_size(t),count,zn_text_size(needle),zn_text_size(replacement));
    zn_text out={(char*)zn_alloc(size+1),size};size_t written=0;start=0;
    if(!zn_text_size(needle)){
        for(;;){
            memcpy(out.data+written,replacement.data,zn_text_size(replacement));written+=zn_text_size(replacement);
            if(start==zn_text_size(t))break;
            uint32_t cp;size_t end=zn_utf8_step(t,start,&cp);
            memcpy(out.data+written,t.data+start,end-start);written+=end-start;start=end;
        }
    }else{
        while((match=zn_find_bytes(t,needle,start))!=SIZE_MAX){
            memcpy(out.data+written,t.data+start,match-start);written+=match-start;
            memcpy(out.data+written,replacement.data,zn_text_size(replacement));written+=zn_text_size(replacement);
            start=match+zn_text_size(needle);
        }
        memcpy(out.data+written,t.data+start,zn_text_size(t)-start);written+=zn_text_size(t)-start;
    }
    if(written!=size)zn_fail("replacement length invariant failed");
    out.data[size]=0;return out;
}
zn_texts zn_text_lines(zn_text t) {
    zn_texts out={0};size_t start=0;
    for(size_t i=0;i<zn_text_size(t);i++)if(t.data[i]=='\n'){
        size_t end=i;if(end>start && t.data[end-1]=='\r')end--;
        zn_texts_push(&out,zn_slice(t,zn_length(start),zn_length(end)));start=i+1;
    }
    if(start<zn_text_size(t))zn_texts_push(&out,zn_slice(t,zn_length(start),zn_length(zn_text_size(t))));
    return out;
}
zn_text zn_text_join(zn_texts parts,zn_text separator) {
    size_t size=0;
    for(size_t i=0;i<parts.len;i++){
        if(zn_text_size(parts.data[i])>(size_t)INT64_MAX-size)zn_fail("joined text too long");
        size+=zn_text_size(parts.data[i]);
        if(i){if(zn_text_size(separator)>(size_t)INT64_MAX-size)zn_fail("joined text too long");size+=zn_text_size(separator);}
    }
    zn_text out={(char*)zn_alloc(size+1),size};size_t at=0;
    for(size_t i=0;i<parts.len;i++){
        if(i){memcpy(out.data+at,separator.data,zn_text_size(separator));at+=zn_text_size(separator);}
        memcpy(out.data+at,parts.data[i].data,zn_text_size(parts.data[i]));at+=zn_text_size(parts.data[i]);
    }
    out.data[at]=0;return out;
}
/* Native backend passes aggregate arguments/results by address. */
int64_t zn_abi_char_count(const zn_text* t){return zn_char_count(*t);}
bool zn_abi_is_char_boundary(const zn_text* t,int64_t at){return zn_is_char_boundary(*t,at);}
int64_t zn_abi_codepoint_at(const zn_text* t,int64_t at){return zn_codepoint_at(*t,at);}
int64_t zn_abi_next_char_boundary(const zn_text* t,int64_t at){return zn_next_char_boundary(*t,at);}
void zn_abi_codepoint_text(zn_text* out,int64_t cp){*out=zn_codepoint_text(cp);}
int64_t zn_abi_text_find(const zn_text* t,const zn_text* n,int64_t start){return zn_text_find(*t,*n,start);}
bool zn_abi_text_starts_with(const zn_text* t,const zn_text* p){return zn_text_starts_with(*t,*p);}
bool zn_abi_text_ends_with(const zn_text* t,const zn_text* s){return zn_text_ends_with(*t,*s);}
void zn_abi_text_trim(zn_text* out,const zn_text* t){*out=zn_text_trim(*t);}
void zn_abi_text_replace(zn_text* out,const zn_text* t,const zn_text* n,const zn_text* r){*out=zn_text_replace(*t,*n,*r);}
void zn_abi_text_lines(zn_texts* out,const zn_text* t){*out=zn_text_lines(*t);}
void zn_abi_text_join(zn_text* out,const zn_texts* t,const zn_text* separator){*out=zn_text_join(*t,*separator);}

static bool zn_try_int(zn_text t, int64_t* out) {
    if (!zn_text_size(t)) return false;
    bool negative = t.data[0] == '-';
    size_t at = negative || t.data[0] == '+' ? 1 : 0;
    if (at == zn_text_size(t)) return false;
    uint64_t value = 0, limit = (uint64_t)INT64_MAX + (negative ? 1 : 0);
    for (; at < zn_text_size(t); at++) {
        unsigned char c = (unsigned char)t.data[at];
        if (c < '0' || c > '9') return false;
        uint64_t digit = c - '0';
        if (value > (limit-digit)/10) return false;
        value = value*10 + digit;
    }
    *out = negative ? (value == (uint64_t)INT64_MAX+1 ? INT64_MIN : -(int64_t)value) : (int64_t)value;
    return true;
}
bool zn_is_int(zn_text t) { int64_t result; return zn_try_int(t, &result); }
int64_t zn_parse_int(zn_text t) { int64_t result; if (!zn_try_int(t, &result)) zn_fail("invalid or overflowing integer text"); return result; }
/* Exactly the bytes of "%" PRId64 (optional '-', no leading zeros, "0" for 0)
   without the CRT formatter: decimal digits are written backwards two at a
   time from the unsigned magnitude, which is exact even for INT64_MIN. The
   result is an ordinary owned text with a plain length. */
static const char zn_decimal_pairs[201] =
    "00010203040506070809101112131415161718192021222324252627282930313233343536373839"
    "40414243444546474849505152535455565758596061626364656667686970717273747576777879"
    "8081828384858687888990919293949596979899";
zn_text zn_int_text(int64_t value) {
    char buffer[24];char* end=buffer+sizeof(buffer);char* at=end;
    uint64_t magnitude=value<0?UINT64_C(0)-(uint64_t)value:(uint64_t)value;
    while(magnitude>=100){unsigned pair=(unsigned)(magnitude%100);magnitude/=100;at-=2;memcpy(at,zn_decimal_pairs+2*pair,2);}
    if(magnitude>=10){at-=2;memcpy(at,zn_decimal_pairs+2*(unsigned)magnitude,2);}
    else *--at=(char)('0'+(unsigned)magnitude);
    if(value<0)*--at='-';
    size_t count=(size_t)(end-at);
    zn_text t={(char*)zn_alloc(count+1),count};memcpy(t.data,at,count);t.data[count]=0;return t;
}

/* Explicit pointer ABI for direct native backends. Ownership checks stay in
   the shared frontend; all dynamic bounds/UTF-8/arithmetic checks stay here. */
zn_text zn_text_view_make(zn_text source,int64_t start,int64_t end){
    if(start<0 || end<start || (uint64_t)end>zn_text_size(source)) zn_fail("text region out of bounds");
    if(!zn_is_char_boundary(source,start) || !zn_is_char_boundary(source,end)) zn_fail("text region splits UTF-8 code point");
    zn_text result={source.data ? source.data+(size_t)start : NULL,(size_t)(end-start)};
    return result;
}
void zn_text_view_drop(zn_text* t){(void)t;}
zn_text zn_to_text(zn_text t){return zn_slice(t,0,zn_length(zn_text_size(t)));}
bool zn_view_contains(zn_text a,zn_text b){return zn_text_find(a,b,0)>=0;}
void zn_print_text_view(zn_text t){if(zn_text_size(t))fwrite(t.data,1,zn_text_size(t),stdout);fputc('\n',stdout);}
void zn_abi_text_region(zn_text* out,const zn_text* source,int64_t start,int64_t end){*out=zn_text_view_make(*source,start,end);}
void zn_abi_to_text(zn_text* out,const zn_text* t){*out=zn_to_text(*t);}
bool zn_abi_view_contains(const zn_text* a,const zn_text* b){return zn_view_contains(*a,*b);}
void zn_abi_print_text_view(const zn_text* t){zn_print_text_view(*t);}
void zn_abi_text_make(zn_text* out,const char* s){*out=zn_text_make(s);}
void zn_abi_text_clone(zn_text* out,const zn_text* in){*out=zn_text_clone(*in);}
void zn_abi_ints_clone(zn_ints* out,const zn_ints* in){*out=zn_ints_clone(*in);}
void zn_abi_texts_clone(zn_texts* out,const zn_texts* in){*out=zn_texts_clone(*in);}
void zn_abi_texts_push(zn_texts* a,const zn_text* value){zn_texts_push(a,*value);}
void zn_abi_text_at(zn_text* out,const zn_texts* a,int64_t i){*out=zn_text_at_ref(a,i);}
void zn_abi_split(zn_texts* out,const zn_text* value,const zn_text* separator){*out=zn_split(*value,*separator);}
void zn_abi_concat(zn_text* out,const zn_text* a,const zn_text* b){*out=zn_concat(*a,*b);}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
void zn_abi_arg(zn_text* out,int64_t i){*out=zn_arg(i);}
void zn_abi_read_text(zn_text* out,const zn_text* p){*out=zn_read_text(*p);}
bool zn_abi_file_exists(const zn_text* p){return zn_file_exists(*p);}
void zn_abi_list_dir(zn_texts* out,const zn_text* p){*out=zn_list_dir(*p);}
void zn_abi_write_text(const zn_text* p,const zn_text* t){zn_write_text(*p,*t);}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
bool zn_abi_text_eq(const zn_text* a,const zn_text* b){return zn_text_eq(*a,*b);}
bool zn_abi_contains(const zn_text* a,const zn_text* b){return zn_contains(*a,*b);}
void zn_abi_slice(zn_text* out,const zn_text* t,int64_t a,int64_t b){*out=zn_slice(*t,a,b);}
int64_t zn_abi_byte_at(const zn_text* t,int64_t i){return zn_byte_at(*t,i);}
int64_t zn_abi_parse_int(const zn_text* t){return zn_parse_int(*t);}
bool zn_abi_is_int(const zn_text* t){return zn_is_int(*t);}
void zn_abi_int_text(zn_text* out,int64_t n){*out=zn_int_text(n);}
void zn_abi_print_text(const zn_text* t){zn_print_text(*t);}
int64_t zn_abi_length(const zn_text* t){return zn_length(zn_text_size(*t));}
int64_t zn_abi_at(const zn_ints* a,int64_t i){return zn_at(a,i);}
void zn_abi_push(zn_ints* a,int64_t n){zn_push(a,n);}
void zn_abi_set(zn_ints* a,int64_t i,int64_t n){zn_set(a,i,n);}

/* IEEE arithmetic permits non-finite results. Integer conversion never relies
   on an undefined C float-to-int conversion: truncate, then check exact powers
   of two as exclusive upper bounds (including u64 and i64). */
uint64_t zn_fp_convert(uint64_t raw,uint64_t from,uint64_t to){
    if(to==232 || to==264){
        if(from==232){float v=zn_f32_value(raw);return to==232?raw:zn_f64_bits((double)v);}
        if(from==264){double v=zn_f64_value(raw);return to==232?zn_f32_bits((float)v):raw;}
        if(from<100){int64_t v=zn_num_signed(raw);return to==232?zn_f32_bits((float)v):zn_f64_bits((double)v);}
        return to==232?zn_f32_bits((float)raw):zn_f64_bits((double)raw);
    }
    double v=from==232?(double)zn_f32_value(raw):zn_f64_value(raw);
    if(!isfinite(v))zn_fail("non-finite float cannot convert to integer");
    v=trunc(v);
    bool sign=to<100;unsigned bits=(unsigned)(sign?to:to-100);
    double limit=ldexp(1.0,sign?(int)bits-1:(int)bits);
    if(v >= limit || v < (sign?-limit:0.0))zn_fail("float to integer conversion out of range");
    return sign?(uint64_t)(int64_t)v:(uint64_t)v;
}
uint64_t zn_fp_remainder(uint64_t a,uint64_t b,uint64_t code){return code==232?zn_f32_bits(fmodf(zn_f32_value(a),zn_f32_value(b))):zn_f64_bits(fmod(zn_f64_value(a),zn_f64_value(b)));}
uint64_t zn_fp_sqrt(uint64_t raw,uint64_t code){return code==232?zn_f32_bits(sqrtf(zn_f32_value(raw))):zn_f64_bits(sqrt(zn_f64_value(raw)));}
uint64_t zn_fp_abs(uint64_t raw,uint64_t code){return raw & (code==232?UINT64_C(0x7fffffff):UINT64_C(0x7fffffffffffffff));}
bool zn_fp_is_finite(uint64_t raw,uint64_t code){return code==232?isfinite(zn_f32_value(raw)):isfinite(zn_f64_value(raw));}
bool zn_fp_is_nan(uint64_t raw,uint64_t code){return code==232?isnan(zn_f32_value(raw)):isnan(zn_f64_value(raw));}
static void zn_fp_format(char* out,size_t size,uint64_t raw,uint64_t code){
    double v=code==232?(double)zn_f32_value(raw):zn_f64_value(raw);
    int n;
    if(isnan(v))n=snprintf(out,size,"nan");
    else if(!isfinite(v))n=snprintf(out,size,signbit(v)?"-inf":"inf");
    else n=snprintf(out,size,code==232?"%.9g":"%.17g",v);
    if(n<0 || (size_t)n>=size)zn_fail("float formatting failed");
}
void zn_print_float(uint64_t raw,uint64_t code){char out[64];zn_fp_format(out,sizeof(out),raw,code);puts(out);}
zn_text zn_float_text(uint64_t raw,uint64_t code){char out[64];zn_fp_format(out,sizeof(out),raw,code);return zn_text_make(out);}
static bool zn_try_f64(zn_text t,double* out){
    size_t i=0,digits=0;
    if(i<zn_text_size(t) && (t.data[i]=='+'||t.data[i]=='-'))i++;
    while(i<zn_text_size(t) && t.data[i]>='0'&&t.data[i]<='9'){i++;digits++;}
    if(i<zn_text_size(t) && t.data[i]=='.'){
        i++;while(i<zn_text_size(t) && t.data[i]>='0'&&t.data[i]<='9'){i++;digits++;}
    }
    if(!digits)return false;
    if(i<zn_text_size(t) && (t.data[i]=='e'||t.data[i]=='E')){
        i++;if(i<zn_text_size(t) && (t.data[i]=='+'||t.data[i]=='-'))i++;
        size_t start=i;while(i<zn_text_size(t) && t.data[i]>='0'&&t.data[i]<='9')i++;
        if(start==i)return false;
    }
    if(i!=zn_text_size(t))return false;
    char* end;double v=strtod(t.data,&end);
    if(end!=t.data+zn_text_size(t) || !isfinite(v))return false;
    *out=v;return true;
}
bool zn_is_f64(zn_text t){double v;return zn_try_f64(t,&v);}
double zn_parse_f64(zn_text t){double v;if(!zn_try_f64(t,&v))zn_fail("invalid or overflowing float text");return v;}
uint64_t zn_abi_parse_f64(const zn_text* t){return zn_f64_bits(zn_parse_f64(*t));}
bool zn_abi_is_f64(const zn_text* t){return zn_is_f64(*t);}
void zn_abi_float_text(zn_text* out,uint64_t raw,uint64_t code){*out=zn_float_text(raw,code);}

/* Typed vectors: the compiler supplies checked element layouts and drop/clone glue. */
static __forceinline size_t zn_vec_bytes(size_t count, uint64_t width) {
    /* For width > 0, count*width fits size_t exactly when count <= SIZE_MAX/width:
       one 128-bit product instead of a division, with an identical failure set. */
    uint64_t high, bytes = _umul128((uint64_t)count, width, &high);
    if (!width || count > INT64_MAX || high) zn_fail("vector capacity overflow");
    return (size_t)bytes;
}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
/* First allocation 16 slots, then doubling (as zn_ints_grow). SoA push keeps
   its own policy (4, then doubling). */
static __declspec(noinline) void zn_vec_grow(zn_vec* a,uint64_t width) {
    size_t cap = a->cap ? a->cap : 16;
    if (a->cap) { if (cap > INT64_MAX / 2) zn_fail("vector capacity overflow"); cap *= 2; }
    void* data = zn_alloc(zn_vec_bytes(cap, width));
    if (a->len) memcpy(data, a->data, zn_vec_bytes(a->len, width));
    zn_release(a->data); a->data=data; a->cap=cap;
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
void* zn_vec_push_slot(zn_vec* a, uint64_t width) {
    if (a->len == a->cap)zn_vec_grow(a,width);
    void* slot=(char*)a->data + zn_vec_bytes(a->len, width);
    a->len++; return slot;
}
void* zn_vec_slot(const zn_vec* a, int64_t index, uint64_t width) {
    if ((uint64_t)index >= a->len) zn_fail("vector index out of bounds");
    return (char*)a->data + zn_vec_bytes((size_t)index, width);
}
void* zn_vec_pop_slot(zn_vec* a, uint64_t width) {
    if (!a->len) zn_fail("pop from empty vector");
    a->len--; return (char*)a->data + zn_vec_bytes(a->len, width);
}
void zn_vec_clone_storage(zn_vec* out, const zn_vec* a, uint64_t width) {
    out->len=a->len;out->cap=a->len;out->data=NULL;
    if (a->len) {size_t bytes=zn_vec_bytes(a->len,width);out->data=zn_alloc(bytes);memcpy(out->data,a->data,bytes);}
}
void zn_vec_release(zn_vec* a) {zn_release(a->data);memset(a,0,sizeof(*a));}
void zn_vec_check_pair(const zn_vec* a,int64_t left,int64_t right) {
    if((uint64_t)left>=a->len || (uint64_t)right>=a->len) zn_fail("vector index out of bounds");
    if(left==right) zn_fail("at_pair indices must be distinct");
}
/* SoA layout (both backends compute it inline; runtime invariant R1): column k
   starts at the sum over j<k of round_up_8(zn_vec_bytes(cap,width_j)). Storage
   is allocated only by zn_soa_push_index/zn_soa_clone_storage after the whole
   layout passed these checks, so it always has 1..=256 columns of nonzero
   width. zn_soa_advance is the checked step from column j's start to column
   j+1's: zn_vec_bytes faults first ("vector capacity overflow"), then the
   padded running total ("soa capacity overflow"). */
static __forceinline size_t zn_soa_advance(size_t bytes,size_t cap,uint64_t width) {
    size_t column=zn_vec_bytes(cap,width);
    if(column>SIZE_MAX-7 || bytes>SIZE_MAX-((column+7)&~(size_t)7)) zn_fail("soa capacity overflow");
    return bytes+((column+7)&~(size_t)7);
}
static size_t zn_soa_offset(size_t cap,uint64_t column,uint64_t count,const uint64_t* fields) {
    size_t bytes=0;
    if(!count || count>256 || column>count) zn_fail("invalid soa layout");
    for(uint64_t k=0;k<column;k++) bytes=zn_soa_advance(bytes,cap,fields[k*2+1]);
    return bytes;
}
void* zn_soa_column_slot(const zn_vec* a,int64_t i,uint64_t column,uint64_t count,const uint64_t* fields) {
    if((uint64_t)i>=a->len) zn_fail("vector index out of bounds");
    if(column>=count) zn_fail("invalid soa column");
    /* Sequenced: the column start's checks precede the row offset's, the order
       every 1.0 recipe evaluated this formerly unsequenced sum in. */
    size_t start=zn_soa_offset(a->cap,column,count,fields);
    return (char*)a->data+start+zn_vec_bytes((size_t)i,fields[column*2+1]);
}
/* Whole-row access walks the columns once with a running offset (O(count)).
   It evaluates exactly the checks of the former per-column
   zn_soa_column_slot(a,i,k) loop, in its order: no checks at all when
   count==0; the index check; the layout check; then, for each column k, the
   capacity step of column k-1 (the only check column k's start adds to those
   already passed) before column k's row offset and copy. The 1.0 runtime
   evaluated zn_soa_offset before the row offset in every backend recipe. */
#define ZN_SOA_ROW_WALK(COPY) do { \
    if(!count) break; \
    if((uint64_t)i>=a->len) zn_fail("vector index out of bounds"); \
    if(count>256) zn_fail("invalid soa layout"); \
    size_t column=0; \
    for(uint64_t k=0;;) { \
        char* slot=(char*)a->data+column+zn_vec_bytes((size_t)i,fields[k*2+1]); \
        COPY; \
        if(++k==count) break; \
        column=zn_soa_advance(column,a->cap,fields[(k-1)*2+1]); \
    } \
} while(0)
void zn_soa_load(void* out,const zn_vec* a,int64_t i,uint64_t count,const uint64_t* fields) {
    ZN_SOA_ROW_WALK(memcpy((char*)out+fields[k*2],slot,(size_t)fields[k*2+1]));
}
void zn_soa_store(zn_vec* a,int64_t i,const void* value,uint64_t count,const uint64_t* fields) {
    ZN_SOA_ROW_WALK(memcpy(slot,(const char*)value+fields[k*2],(size_t)fields[k*2+1]));
}
/* Validates the whole layout at cap in column order and returns the total
   size: the exact checks and order of zn_soa_offset(cap,count,count,fields). */
static size_t zn_soa_total(size_t cap,uint64_t count,const uint64_t* fields) {
    if(!count || count>256) zn_fail("invalid soa layout");
    size_t bytes=0;
    for(uint64_t k=0;k<count;k++) bytes=zn_soa_advance(bytes,cap,fields[k*2+1]);
    return bytes;
}
/* Copies rows [0,len) of every column from storage laid out at from_cap into
   storage laid out at to_cap, with running offsets. Column k's start at
   to_cap and its byte count (len<=to_cap) were validated by zn_soa_total
   (to_cap); only from_cap's capacity step of column k-1 can still fault, as in
   the former per-column zn_soa_offset(from_cap,k) calls. */
static void zn_soa_copy_columns(void* to,size_t to_cap,const void* from,size_t from_cap,size_t len,uint64_t count,const uint64_t* fields) {
    size_t at=0,source=0;
    for(uint64_t k=0;k<count;k++) {
        if(k) { at=zn_soa_advance(at,to_cap,fields[(k-1)*2+1]); source=zn_soa_advance(source,from_cap,fields[(k-1)*2+1]); }
        memcpy((char*)to+at,(const char*)from+source,zn_vec_bytes(len,fields[k*2+1]));
    }
}
int64_t zn_soa_push_index(zn_vec* a,uint64_t count,const uint64_t* fields) {
    if(a->len==a->cap) {
        size_t cap=a->cap?a->cap:4;
        if(a->cap) { if(cap>INT64_MAX/2) zn_fail("soa capacity overflow");cap*=2; }
        void* data=zn_alloc(zn_soa_total(cap,count,fields));
        if(a->len) zn_soa_copy_columns(data,cap,a->data,a->cap,a->len,count,fields);
        zn_release(a->data);a->data=data;a->cap=cap;
    }
    return (int64_t)a->len++;
}
void zn_soa_pop(void* out,zn_vec* a,uint64_t count,const uint64_t* fields) {
    if(!a->len) zn_fail("pop from empty vector");
    int64_t i=(int64_t)a->len-1;zn_soa_load(out,a,i,count,fields);
    /* zn_soa_load passed every check this second walk repeats. */
    ZN_SOA_ROW_WALK(memset(slot,0,(size_t)fields[k*2+1]));
    a->len--;
}
void zn_soa_clone_storage(zn_vec* out,const zn_vec* a,uint64_t count,const uint64_t* fields) {
    memset(out,0,sizeof(*out));out->len=out->cap=a->len;
    if(a->len) {
        out->data=zn_alloc(zn_soa_total(out->cap,count,fields));
        zn_soa_copy_columns(out->data,out->cap,a->data,a->cap,a->len,count,fields);
    }
}
#undef ZN_SOA_ROW_WALK

/* Nominal maps use deterministic linear probing. Storage moves are raw owner
   transfers; only generated typed code destroys or clones entry contents.
   Runtime-private H2 fingerprints: the states allocation holds the documented
   cap control bytes (0 empty, 1 live, 2 tombstone) followed by cap private tag
   bytes. A live slot's tag is the top 7 bits of its key hash (insert, rehash
   and compaction write it; clone copies it; erase clears it; release frees the
   one allocation). Non-live tags are ignored: candidates are live slots whose
   tag equals the probe's, and key equality still decides every match. */
static __forceinline size_t zn_map_bytes(uint64_t count,uint64_t width) {
    /* count*width <= PTRDIFF_MAX is exactly count <= PTRDIFF_MAX/width (and
       implies count <= SIZE_MAX/width); one 128-bit product, no division. */
    uint64_t high,bytes=_umul128(count,width,&high);
    if(!width || width>(uint64_t)PTRDIFF_MAX || count>(uint64_t)INT64_MAX ||
       high || bytes>(uint64_t)PTRDIFF_MAX)
        zn_fail("map capacity overflow");
    return (size_t)bytes;
}
static size_t zn_map_control_bytes(uint64_t cap) {
    if(cap>(uint64_t)(SIZE_MAX/2)) zn_fail("map capacity overflow");
    return (size_t)cap*2;
}
static __forceinline void zn_map_check(const zn_map* m,uint64_t width) {
    if(!m) zn_fail("invalid map storage");
    (void)zn_map_bytes(m->cap,width);
    if(m->len>m->cap || m->tombstones>m->cap-m->len)
        zn_fail("invalid map storage");
    if(!m->cap) {
        if(m->data || m->states || m->len || m->tombstones) zn_fail("invalid map storage");
    } else if(m->cap<8 || (m->cap&(m->cap-1)) || !m->data || !m->states) {
        zn_fail("invalid map storage");
    }
}
static __forceinline size_t zn_map_key_width(uint64_t code,uint64_t width) {
    size_t size;
    switch(code) {
    case 0: size=sizeof(zn_text);break;
    case 8: case 108:size=1;break;
    case 16: case 116:size=2;break;
    case 32: case 132:size=4;break;
    case 64: case 164:size=8;break;
    default:zn_fail("invalid map key type");
    }
    if(width<size) zn_fail("invalid map entry width");
    return size;
}
static __forceinline zn_text zn_map_text_key(const void* key) {
    zn_text text;
    if(!key) zn_fail("invalid map key");
    memcpy(&text,key,sizeof(text));
    text.len=zn_text_size(text);
    if(text.len>(size_t)PTRDIFF_MAX || (text.len && !text.data)) zn_fail("invalid map text key");
    return text;
}
static __forceinline uint64_t zn_map_mix(uint64_t hash) {
    hash^=hash>>30;hash*=UINT64_C(0xbf58476d1ce4e5b9);
    hash^=hash>>27;hash*=UINT64_C(0x94d049bb133111eb);
    return hash^(hash>>31);
}
/* Zero-extended little-endian integer key of exactly size (1, 2, 4 or 8)
   bytes: fixed-size unaligned loads instead of a variable-length copy call. */
static __forceinline uint64_t zn_map_int_key(const void* key,size_t size) {
    switch(size) {
    case 1:{uint8_t v;memcpy(&v,key,1);return v;}
    case 2:{uint16_t v;memcpy(&v,key,2);return v;}
    case 4:{uint32_t v;memcpy(&v,key,4);return v;}
    default:{uint64_t v;memcpy(&v,key,8);return v;}
    }
}
static __forceinline uint64_t zn_map_hash(const void* key,uint64_t code,uint64_t width) {
    size_t size=zn_map_key_width(code,width);
    if(!key) zn_fail("invalid map key");
    if(code) {
        /* Narrow keys need neither alignment nor canonical register extension.
           In particular an unsigned 64-bit key retains every high bit. */
        return zn_map_mix(zn_map_int_key(key,size)^UINT64_C(0x9e3779b97f4a7c15));
    }
    /* Runtime-private word-at-a-time FNV-style text hash over the masked
       (tag-free) byte length, then the shared finalizer. Ownership tags never
       affect it. Only fixed-size in-bounds loads (no variable-length copy call):
       for len >= 8 the final 1..8 bytes come from the last 8 bytes shifted
       right, i.e. exactly the zero-padded tail word; shorter texts use two
       overlapping 4-byte loads (4..7) or three bytes (1..3), which determine
       every byte for their length. */
    zn_text text=zn_map_text_key(key);
    const unsigned char* bytes=(const unsigned char*)text.data;
    size_t n=text.len;
    uint64_t hash=(UINT64_C(14695981039346656037)^(uint64_t)n)*UINT64_C(1099511628211),word;
    if(n>=8){
        size_t at=0;
        for(;n-at>8;at+=8){memcpy(&word,bytes+at,8);hash=(hash^word)*UINT64_C(1099511628211);}
        memcpy(&word,bytes+n-8,8);word>>=8*(unsigned)(8-(n-at));
    }else if(n>=4){
        uint32_t low,high;memcpy(&low,bytes,4);memcpy(&high,bytes+n-4,4);word=low|((uint64_t)high<<32);
    }else if(n){
        word=bytes[0]|((uint64_t)bytes[n>>1]<<8)|((uint64_t)bytes[n-1]<<16);
    }else return zn_map_mix(hash);
    return zn_map_mix((hash^word)*UINT64_C(1099511628211));
}
/* The fingerprint uses the hash bits the slot index uses last (the top 7). */
static __forceinline unsigned char zn_map_tag(uint64_t hash) {return (unsigned char)(hash>>57);}
static __forceinline bool zn_map_equal(const void* entry,const void* key,uint64_t code,uint64_t width) {
    size_t size=zn_map_key_width(code,width);
    if(code) return zn_map_int_key(entry,size)==zn_map_int_key(key,size);
    zn_text a=zn_map_text_key(entry),b=zn_map_text_key(key);
    return a.len==b.len && (!a.len || a.data==b.data || memcmp(a.data,b.data,a.len)==0);
}
static unsigned char zn_map_state(const zn_map* m,uint64_t index) {
    unsigned char state=m->states[(size_t)index];
    if(state>2) zn_fail("invalid map slot state");
    return state;
}
static __forceinline void* zn_map_entry(const zn_map* m,uint64_t index,uint64_t width) {
    return (unsigned char*)m->data+zn_map_bytes(index,width);
}
/* Probe-loop slot address: zn_map_check proved cap*width <= PTRDIFF_MAX and
   every probe index is below cap, so the product cannot overflow. */
static __forceinline void* zn_map_probe_entry(const zn_map* m,uint64_t index,uint64_t width) {
    return (unsigned char*)m->data+(size_t)(index*width);
}
/* match: live slots whose tag equals the probe tag; empty/tomb from states. */
typedef struct {unsigned match,empty,tomb;} zn_map_group;
/* Classifies the count (1..16) control bytes starting at index. No padding or
   speculative overread: with cap >= 16 SIMD loads 16 bytes at index, or, when
   the group runs into the end of the arrays, the last 16 bytes of each array
   shifted so bit 0 is index. Only the count probed bytes are classified and
   validated, as before. State values remain compatible with generated typed
   iteration/destructors. */
static __forceinline zn_map_group zn_map_controls(const zn_map* m,uint64_t index,unsigned count,unsigned char tag) {
    const unsigned char* states=m->states+(size_t)index;
    const unsigned char* tags=m->states+(size_t)(m->cap+index);
    zn_map_group out={0};
#ifdef ZN_MAP_SSE2
    if(m->cap>=16){
        uint64_t base=m->cap-index>=16?index:m->cap-16;
        unsigned shift=(unsigned)(index-base),window=(1u<<count)-1;
        __m128i bytes=_mm_loadu_si128((const __m128i*)(m->states+(size_t)base));
        unsigned live=((unsigned)_mm_movemask_epi8(_mm_cmpeq_epi8(bytes,_mm_set1_epi8(1)))>>shift)&window;
        out.empty=((unsigned)_mm_movemask_epi8(_mm_cmpeq_epi8(bytes,_mm_setzero_si128()))>>shift)&window;
        out.tomb=((unsigned)_mm_movemask_epi8(_mm_cmpeq_epi8(bytes,_mm_set1_epi8(2)))>>shift)&window;
        if((live|out.empty|out.tomb)!=window)zn_fail("invalid map slot state");
        __m128i marks=_mm_loadu_si128((const __m128i*)(m->states+(size_t)(m->cap+base)));
        out.match=live&((unsigned)_mm_movemask_epi8(_mm_cmpeq_epi8(marks,_mm_set1_epi8((char)tag)))>>shift);
        return out;
    }
#endif
    for(unsigned i=0;i<count;i++){
        unsigned bit=1u<<i;
        switch(states[i]){case 0:out.empty|=bit;break;case 1:if(tags[i]==tag)out.match|=bit;break;case 2:out.tomb|=bit;break;default:zn_fail("invalid map slot state");}
    }
    return out;
}
static __forceinline unsigned zn_map_first(unsigned bits) {
    unsigned long first;_BitScanForward(&first,bits);return (unsigned)first;
}
void* zn_map_find(const zn_map* m,const void* key,uint64_t code,uint64_t width) {
    zn_map_check(m,width);
    uint64_t hash=zn_map_hash(key,code,width);
    if(!m->cap) return NULL;
    unsigned char tag=zn_map_tag(hash);
    uint64_t index=hash&(m->cap-1);
    for(uint64_t probes=0;probes<m->cap;) {
        unsigned count=(unsigned)((m->cap-index)<16?m->cap-index:16);
        if(count>m->cap-probes)count=(unsigned)(m->cap-probes);
        zn_map_group group=zn_map_controls(m,index,count,tag);
        unsigned stop=group.empty?zn_map_first(group.empty):count;
        unsigned match=group.match&((1u<<stop)-1);
        while(match){unsigned at=zn_map_first(match);match&=match-1;
            void* entry=zn_map_probe_entry(m,index+at,width);
            if(zn_map_equal(entry,key,code,width))return entry;
        }
        if(group.empty)return NULL;
        probes+=count;index=(index+count)&(m->cap-1);
    }
    return NULL;
}
#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
#define ZN_COLD_MESSAGES_ZN_MAP_REHASH(X) \
    X(invalid_map_capacity, "invalid map capacity") \
    X(map_probing_overflow, "map probing overflow") \
    X(invalid_map_live_count, "invalid map live count")
ZN_COLD_ERRORS(zn_cold_errors_map_rehash, ZN_COLD_MESSAGES_ZN_MAP_REHASH);
#undef ZN_COLD_MESSAGES_ZN_MAP_REHASH

static void zn_map_rehash(zn_map* m,uint64_t cap,uint64_t code,uint64_t width) {
    zn_map out={0};size_t bytes=zn_map_bytes(cap,width);
    if(cap<8 || (cap&(cap-1))) zn_fail(zn_cold_errors_map_rehash.invalid_map_capacity);
    size_t controls=zn_map_control_bytes(cap);
    out.data=zn_alloc(bytes);memset(out.data,0,bytes);
    out.states=(unsigned char*)zn_alloc(controls);memset(out.states,0,controls);out.cap=cap;
    unsigned char* tags=out.states+(size_t)cap;
    for(uint64_t old=0;old<m->cap;old++) if(zn_map_state(m,old)==1) {
        void* entry=zn_map_entry(m,old,width);
        uint64_t hash=zn_map_hash(entry,code,width),index=hash&(cap-1),probes=0;
        while(out.states[(size_t)index]) {
            if(++probes==cap) zn_fail(zn_cold_errors_map_rehash.map_probing_overflow);
            index=(index+1)&(cap-1);
        }
        memcpy(zn_map_entry(&out,index,width),entry,(size_t)width);
        out.states[(size_t)index]=1;tags[(size_t)index]=zn_map_tag(hash);out.len++;
    }
    if(out.len!=m->len) zn_fail(zn_cold_errors_map_rehash.invalid_map_live_count);
    zn_release(m->data);zn_release(m->states);*m=out;
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
/* Single hash, single probe. Rehash happens BEFORE probing, only when cap==0
   or live+tombstones reach 3/4 of cap (unchanged 0.28 thresholds): it doubles
   when live >= cap/2, otherwise it compacts tombstones at the same cap. Either
   way the probe then sees live+tombstones < 3/4 cap, so an empty slot exists
   and the bounded probe terminates. A replace at the threshold may compact or
   grow once without inserting; slot order is runtime-private. */
void* zn_map_insert_slot(zn_map* m,const void* key,uint64_t code,uint64_t width,bool* replaced) {
    if(!replaced) zn_fail("invalid map replacement flag");
    zn_map_check(m,width);
    uint64_t hash=zn_map_hash(key,code,width);
    if(!m->cap) zn_map_rehash(m,8,code,width);
    else if(m->len+m->tombstones>=m->cap-m->cap/4) {
        uint64_t cap=m->cap;
        if(m->len>=cap/2) {
            if(cap>(uint64_t)INT64_MAX/2) zn_fail("map capacity overflow");
            cap*=2;
        }
        zn_map_rehash(m,cap,code,width);
    }
    unsigned char* tags=m->states+(size_t)m->cap;
    unsigned char tag=zn_map_tag(hash);
    uint64_t index=hash&(m->cap-1),tomb=m->cap;
    for(uint64_t probes=0;probes<m->cap;) {
        unsigned count=(unsigned)((m->cap-index)<16?m->cap-index:16);
        if(count>m->cap-probes)count=(unsigned)(m->cap-probes);
        zn_map_group group=zn_map_controls(m,index,count,tag);
        unsigned stop=group.empty?zn_map_first(group.empty):count;
        unsigned mask=(1u<<stop)-1,match=group.match&mask;
        while(match){unsigned at=zn_map_first(match);match&=match-1;
            void* entry=zn_map_probe_entry(m,index+at,width);
            if(zn_map_equal(entry,key,code,width)){*replaced=true;return entry;}
        }
        unsigned before=group.tomb&mask;
        if(before && tomb==m->cap)tomb=index+zn_map_first(before);
        if(group.empty) {
            index+=stop;if(tomb!=m->cap) {index=tomb;m->tombstones--;}
            m->states[(size_t)index]=1;tags[(size_t)index]=tag;m->len++;*replaced=false;
            return zn_map_probe_entry(m,index,width);
        }
        probes+=count;index=(index+count)&(m->cap-1);
    }
    /* Valid load factors always leave an empty slot; a corrupt descriptor must
       fail in bounded time rather than wrap a probe indefinitely. */
    zn_fail("map probing overflow");
}
void zn_map_erase_slot(zn_map* m,void* slot,uint64_t width) {
    zn_map_check(m,width);
    uintptr_t start=(uintptr_t)m->data,address=(uintptr_t)slot;
    size_t bytes=zn_map_bytes(m->cap,width);
    if(!m->cap || !slot || address<start || address-start>=bytes || (address-start)%width)
        zn_fail("invalid map slot");
    uint64_t index=(address-start)/width;
    if(zn_map_state(m,index)!=1 || !m->len) zn_fail("invalid map slot");
    memset(slot,0,(size_t)width);m->states[(size_t)index]=2;m->states[(size_t)(m->cap+index)]=0;m->len--;m->tombstones++;
}
void zn_map_clone_storage(zn_map* out,const zn_map* source,uint64_t width) {
    zn_map_check(source,width);
    if(!out || out==source) zn_fail("invalid map clone destination");
    *out=(zn_map){0};
    if(!source->cap) return;
    size_t bytes=zn_map_bytes(source->cap,width),controls=zn_map_control_bytes(source->cap);
    out->data=zn_alloc(bytes);out->states=(unsigned char*)zn_alloc(controls);
    memcpy(out->data,source->data,bytes);memcpy(out->states,source->states,controls);
    out->cap=source->cap;out->len=source->len;out->tombstones=source->tombstones;
}
void* zn_map_next_slot(const zn_map* m,uint64_t* cursor,uint64_t width) {
    zn_map_check(m,width);
    if(!cursor || *cursor>m->cap) zn_fail("invalid map cursor");
    while(*cursor<m->cap) {
        uint64_t index=(*cursor)++;
        if(zn_map_state(m,index)==1) return zn_map_entry(m,index,width);
    }
    return NULL;
}
void* zn_map_slot_at(const zn_map* m,uint64_t index,uint64_t width) {
    zn_map_check(m,width);
    if(index>=m->cap || zn_map_state(m,index)!=1) zn_fail("invalid map slot");
    return zn_map_entry(m,index,width);
}
void zn_map_release(zn_map* m) {
    if(!m) zn_fail("invalid map storage");
    zn_release(m->data);zn_release(m->states);memset(m,0,sizeof(*m));
}

#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
/* v0.26 direct subprocesses. A job owns the process tree. Only the three
   explicitly listed standard handles cross CreateProcessW. Readers independently
   drain available pipe bytes; PeekNamedPipe keeps cancellation finite without
   closing a handle underneath a blocked CRT read. */
#define ZN_PROCESS_STREAM_LIMIT (16u*1024u*1024u)
#define ZN_PROCESS_TIMEOUT_MS 30000u
#define ZN_PROCESS_COMMAND_LIMIT 32766u
typedef struct {
    HANDLE pipe,failed;
    volatile LONG stop;
    DWORD error;
    zn_text text;
    size_t cap;
} zn_process_reader;
static DWORD WINAPI zn_process_read(void* raw) {
    zn_process_reader* r=(zn_process_reader*)raw;
    unsigned char chunk[8192];
    for(;;) {
        if(InterlockedCompareExchange(&r->stop,0,0))break;
        DWORD available=0;
        if(!PeekNamedPipe(r->pipe,NULL,0,NULL,&available,NULL)) {
            DWORD error=GetLastError();
            if(error!=ERROR_BROKEN_PIPE && error!=ERROR_PIPE_NOT_CONNECTED)r->error=error;
            break;
        }
        if(!available){Sleep(1);continue;}
        DWORD got=0,want=available>(DWORD)sizeof(chunk)?(DWORD)sizeof(chunk):available;
        if(!ReadFile(r->pipe,chunk,want,&got,NULL)) {
            DWORD error=GetLastError();
            if(error!=ERROR_BROKEN_PIPE && error!=ERROR_PIPE_NOT_CONNECTED)r->error=error;
            break;
        }
        if(!got)break;
        if(got>ZN_PROCESS_STREAM_LIMIT-r->text.len){r->error=ERROR_BUFFER_OVERFLOW;break;}
        size_t next=r->text.len+got;
        if(next+1>r->cap) {
            size_t cap=r->cap?r->cap:8192;
            while(cap<next+1)cap=cap>ZN_PROCESS_STREAM_LIMIT/2?ZN_PROCESS_STREAM_LIMIT+1:cap*2;
            void* allocation=realloc(r->text.data,cap);
            if(!allocation)zn_fail("allocation failed");
            r->text.data=(char*)allocation;r->cap=cap;
        }
        memcpy(r->text.data+r->text.len,chunk,got);r->text.len=next;
    }
    if(r->error)SetEvent(r->failed);
    return 0;
}
static bool zn_process_text_valid(zn_text text) {
    return zn_text_size(text)<=INT_MAX && (!zn_text_size(text) || (text.data
        && !memchr(text.data,0,zn_text_size(text))
        && MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data,(int)zn_text_size(text),NULL,0)));
}
static wchar_t* zn_process_wide(zn_text text) {
    if(!zn_process_text_valid(text))return NULL;
    int width=zn_text_size(text)?MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data,(int)zn_text_size(text),NULL,0):0;
    if(width>(int)ZN_PROCESS_COMMAND_LIMIT)return NULL;
    wchar_t* out=(wchar_t*)zn_alloc(((size_t)width+1)*sizeof(wchar_t));
    if(width && !MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data,(int)zn_text_size(text),out,width)) {free(out);return NULL;}
    out[width]=0;return out;
}
/* Always quote: a run of N backslashes before a quote needs 2N+1 escapes;
   before the closing quote it needs 2N. Other backslashes stay unchanged. */
static bool zn_process_quote(wchar_t* out,size_t* used,const wchar_t* value) {
    size_t at=*used;
    if(at+1>ZN_PROCESS_COMMAND_LIMIT)return false;out[at++]=L'"';
    for(size_t i=0;;) {
        size_t slash=0;while(value[i]==L'\\'){slash++;i++;}
        bool end=value[i]==0,quote=value[i]==L'"';
        size_t copies=(end||quote)?slash*2:slash;
        if(quote)copies++;
        if(copies>ZN_PROCESS_COMMAND_LIMIT-at)return false;
        for(size_t n=0;n<copies;n++)out[at++]=L'\\';
        if(end)break;
        if(at+1>ZN_PROCESS_COMMAND_LIMIT)return false;out[at++]=value[i++];
    }
    if(at+1>ZN_PROCESS_COMMAND_LIMIT)return false;out[at++]=L'"';out[at]=0;*used=at;return true;
}
static DWORD zn_process_remaining(ULONGLONG start) {
    ULONGLONG elapsed=GetTickCount64()-start;
    return elapsed>=ZN_PROCESS_TIMEOUT_MS?0:(DWORD)(ZN_PROCESS_TIMEOUT_MS-elapsed);
}
#define ZN_COLD_MESSAGES_ZN_PROCESS_CAPTURE(X) \
    X(invalid_process_program_or_argument_count, "invalid process program or argument count") \
    X(process_program_must_be_utf_8_without_nul_and_fit_the_window, "process program must be UTF-8 without NUL and fit the Windows command limit") \
    X(process_command_exceeds_32766_utf_16_units, "process command exceeds 32766 UTF-16 units") \
    X(process_arguments_must_be_utf_8_without_nul_and_fit_the_wind, "process arguments must be UTF-8 without NUL and fit the Windows command limit") \
    X(cannot_create_process_capture_pipes, "cannot create process capture pipes") \
    X(cannot_create_null_process_stdin, "cannot create null process stdin") \
    X(cannot_create_process_job, "cannot create process job") \
    X(cannot_configure_process_job, "cannot configure process job") \
    X(cannot_initialize_process_handle_list, "cannot initialize process handle list") \
    X(cannot_restrict_process_handle_inheritance, "cannot restrict process handle inheritance") \
    X(cannot_launch_process_executable, "cannot launch process executable") \
    X(cannot_attach_process_to_cancellation_job, "cannot attach process to cancellation job") \
    X(cannot_create_capture_failure_event, "cannot create capture failure event") \
    X(cannot_start_process_capture_reader, "cannot start process capture reader") \
    X(cannot_resume_process, "cannot resume process") \
    X(process_deadline_exceeded_30000_ms, "process deadline exceeded (30000 ms)") \
    X(process_capture_failed, "process capture failed") \
    X(process_pipe_deadline_exceeded_30000_ms, "process pipe deadline exceeded (30000 ms)") \
    X(cannot_finish_process_capture, "cannot finish process capture") \
    X(process_capture_failed_or_exceeded_16_mib_stream_limit, "process capture failed or exceeded 16 MiB stream limit") \
    X(cannot_retrieve_process_status, "cannot retrieve process status") \
    X(process_output_must_be_utf_8_text_without_nul_bytes, "process output must be UTF-8 text without NUL bytes") \
    X(process_output_exceeded_16_mib_stream_limit, "process output exceeded 16 MiB stream limit") \
    X(s_windows_error_lu, "%s (Windows error %lu)") \
    X(s, "%s")
ZN_COLD_ERRORS(zn_cold_errors_process_capture, ZN_COLD_MESSAGES_ZN_PROCESS_CAPTURE);
#undef ZN_COLD_MESSAGES_ZN_PROCESS_CAPTURE

zn_process_result zn_process_capture(zn_text program,const zn_text* args,uint64_t count) {
    zn_process_result result={0};
    const char* error=NULL;DWORD system_error=0,status=0;
    HANDLE stdout_read=NULL,stdout_write=NULL,stderr_read=NULL,stderr_write=NULL;
    HANDLE input=NULL,job=NULL,failed=NULL,readers[2]={NULL,NULL};
    PROCESS_INFORMATION process={0};STARTUPINFOEXW startup={0};
    LPPROC_THREAD_ATTRIBUTE_LIST attributes=NULL;bool attributes_ready=false,launched=false;
    wchar_t* executable=NULL;wchar_t* command=NULL;
    zn_process_reader capture[2]={{0}};
    ULONGLONG started=GetTickCount64();
    if(!zn_text_size(program) || count>ZN_PROCESS_COMMAND_LIMIT || (count && !args)) {error=zn_cold_errors_process_capture.invalid_process_program_or_argument_count;goto cleanup;}
    executable=zn_process_wide(program);
    if(!executable){error=zn_cold_errors_process_capture.process_program_must_be_utf_8_without_nul_and_fit_the_window;goto cleanup;}
    command=(wchar_t*)zn_alloc((ZN_PROCESS_COMMAND_LIMIT+1)*sizeof(wchar_t));size_t used=0;
    if(!zn_process_quote(command,&used,executable)){error=zn_cold_errors_process_capture.process_command_exceeds_32766_utf_16_units;goto cleanup;}
    for(uint64_t i=0;i<count;i++) {
        wchar_t* argument=zn_process_wide(args[i]);
        if(!argument){error=zn_cold_errors_process_capture.process_arguments_must_be_utf_8_without_nul_and_fit_the_wind;goto cleanup;}
        if(used+1>ZN_PROCESS_COMMAND_LIMIT){free(argument);error=zn_cold_errors_process_capture.process_command_exceeds_32766_utf_16_units;goto cleanup;}
        command[used++]=L' ';bool quoted=zn_process_quote(command,&used,argument);free(argument);
        if(!quoted){error=zn_cold_errors_process_capture.process_command_exceeds_32766_utf_16_units;goto cleanup;}
    }
    SECURITY_ATTRIBUTES security={sizeof(security),NULL,TRUE};
    if(!CreatePipe(&stdout_read,&stdout_write,&security,65536)
        || !CreatePipe(&stderr_read,&stderr_write,&security,65536)
        || !SetHandleInformation(stdout_read,HANDLE_FLAG_INHERIT,0)
        || !SetHandleInformation(stderr_read,HANDLE_FLAG_INHERIT,0)) {error=zn_cold_errors_process_capture.cannot_create_process_capture_pipes;system_error=GetLastError();goto cleanup;}
    input=CreateFileW(L"NUL",GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE,&security,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL,NULL);
    if(input==INVALID_HANDLE_VALUE){input=NULL;error=zn_cold_errors_process_capture.cannot_create_null_process_stdin;system_error=GetLastError();goto cleanup;}
    job=CreateJobObjectW(NULL,NULL);
    if(!job){error=zn_cold_errors_process_capture.cannot_create_process_job;system_error=GetLastError();goto cleanup;}
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits={0};limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if(!SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limits,sizeof(limits))) {error=zn_cold_errors_process_capture.cannot_configure_process_job;system_error=GetLastError();goto cleanup;}
    SIZE_T attribute_size=0;InitializeProcThreadAttributeList(NULL,1,0,&attribute_size);
    attributes=(LPPROC_THREAD_ATTRIBUTE_LIST)zn_alloc(attribute_size);
    if(!InitializeProcThreadAttributeList(attributes,1,0,&attribute_size)){error=zn_cold_errors_process_capture.cannot_initialize_process_handle_list;system_error=GetLastError();goto cleanup;}
    attributes_ready=true;HANDLE inherited[3]={input,stdout_write,stderr_write};
    if(!UpdateProcThreadAttribute(attributes,0,PROC_THREAD_ATTRIBUTE_HANDLE_LIST,inherited,sizeof(inherited),NULL,NULL)) {error=zn_cold_errors_process_capture.cannot_restrict_process_handle_inheritance;system_error=GetLastError();goto cleanup;}
    startup.StartupInfo.cb=sizeof(startup);startup.StartupInfo.dwFlags=STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput=input;startup.StartupInfo.hStdOutput=stdout_write;startup.StartupInfo.hStdError=stderr_write;startup.lpAttributeList=attributes;
    if(!CreateProcessW(executable,command,NULL,NULL,TRUE,CREATE_NO_WINDOW|CREATE_SUSPENDED|EXTENDED_STARTUPINFO_PRESENT,NULL,NULL,&startup.StartupInfo,&process)) {error=zn_cold_errors_process_capture.cannot_launch_process_executable;system_error=GetLastError();goto cleanup;}
    launched=true;
    if(!AssignProcessToJobObject(job,process.hProcess)){error=zn_cold_errors_process_capture.cannot_attach_process_to_cancellation_job;system_error=GetLastError();goto cleanup;}
    CloseHandle(stdout_write);stdout_write=NULL;CloseHandle(stderr_write);stderr_write=NULL;CloseHandle(input);input=NULL;
    failed=CreateEventW(NULL,TRUE,FALSE,NULL);
    if(!failed){error=zn_cold_errors_process_capture.cannot_create_capture_failure_event;system_error=GetLastError();goto cleanup;}
    capture[0].pipe=stdout_read;capture[1].pipe=stderr_read;
    for(size_t i=0;i<2;i++) {
        capture[i].failed=failed;readers[i]=CreateThread(NULL,0,zn_process_read,&capture[i],0,NULL);
        if(!readers[i]){error=zn_cold_errors_process_capture.cannot_start_process_capture_reader;system_error=GetLastError();goto cleanup;}
    }
    if(ResumeThread(process.hThread)==(DWORD)-1){error=zn_cold_errors_process_capture.cannot_resume_process;system_error=GetLastError();goto cleanup;}
    HANDLE waiters[2]={process.hProcess,failed};
    DWORD waited=WaitForMultipleObjects(2,waiters,FALSE,zn_process_remaining(started));
    if(waited!=WAIT_OBJECT_0){error=waited==WAIT_TIMEOUT?zn_cold_errors_process_capture.process_deadline_exceeded_30000_ms:zn_cold_errors_process_capture.process_capture_failed;if(waited==WAIT_FAILED)system_error=GetLastError();goto cleanup;}
    waited=WaitForMultipleObjects(2,readers,TRUE,zn_process_remaining(started));
    if(waited!=WAIT_OBJECT_0){error=waited==WAIT_TIMEOUT?zn_cold_errors_process_capture.process_pipe_deadline_exceeded_30000_ms:zn_cold_errors_process_capture.cannot_finish_process_capture;if(waited==WAIT_FAILED)system_error=GetLastError();goto cleanup;}
    if(capture[0].error || capture[1].error){error=zn_cold_errors_process_capture.process_capture_failed_or_exceeded_16_mib_stream_limit;goto cleanup;}
    if(!GetExitCodeProcess(process.hProcess,&status)){error=zn_cold_errors_process_capture.cannot_retrieve_process_status;system_error=GetLastError();goto cleanup;}
    if(!zn_process_text_valid(capture[0].text) || !zn_process_text_valid(capture[1].text)){error=zn_cold_errors_process_capture.process_output_must_be_utf_8_text_without_nul_bytes;goto cleanup;}
cleanup:
    if(error && launched) {
        /* Also terminate directly if assigning the still-suspended child failed. */
        if(job)TerminateJobObject(job,1);TerminateProcess(process.hProcess,1);
    }
    for(size_t i=0;i<2;i++)InterlockedExchange(&capture[i].stop,1);
    for(size_t i=0;i<2;i++)if(readers[i]){WaitForSingleObject(readers[i],INFINITE);CloseHandle(readers[i]);}
    if(job)CloseHandle(job); /* kills any descendants retaining capture writers */
    if(process.hThread)CloseHandle(process.hThread);
    if(process.hProcess)CloseHandle(process.hProcess);
    if(failed)CloseHandle(failed);
    if(stdout_read)CloseHandle(stdout_read);if(stdout_write)CloseHandle(stdout_write);
    if(stderr_read)CloseHandle(stderr_read);if(stderr_write)CloseHandle(stderr_write);
    if(input)CloseHandle(input);
    if(attributes_ready)DeleteProcThreadAttributeList(attributes);free(attributes);free(executable);free(command);
    if(error) {
        if(capture[0].error==ERROR_BUFFER_OVERFLOW || capture[1].error==ERROR_BUFFER_OVERFLOW)
            error=zn_cold_errors_process_capture.process_output_exceeded_16_mib_stream_limit;
        free(capture[0].text.data);free(capture[1].text.data);
        char message[240];if(system_error)snprintf(message,sizeof(message),zn_cold_errors_process_capture.s_windows_error_lu,error,(unsigned long)system_error);else snprintf(message,sizeof(message),zn_cold_errors_process_capture.s,error);
        result.tag=2;result.data.error=zn_text_make(message);
    } else {
        for(size_t i=0;i<2;i++) {
            if(!capture[i].text.data)capture[i].text.data=(char*)zn_alloc(1);
            capture[i].text.data[capture[i].text.len]=0;
        }
        result.tag=1;result.data.output.status=(int64_t)(uint64_t)status;
        result.data.output.stdout_text=capture[0].text;result.data.output.stderr_text=capture[1].text;
    }
    return result;
}
void zn_abi_process_run(void* out,const zn_text* program,const zn_text* args,uint64_t count) {
    zn_process_result result=zn_process_capture(*program,args,count);memcpy(out,&result,sizeof(result));
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
/* ---- v1.2 Frontier 3 (V1200 N2-N6) ----------------------------------------
   Only the 1.2 builtins reach these helpers (N7). Every function and table is
   its own COMDAT (/Gy /Gw), so /OPT:REF drops them, and every import they use,
   from programs that never call them. */
uint64_t zn_fnv1a_64(const void* data,uint64_t len) {
    if((!data && len) || len>(uint64_t)INT64_MAX) zn_fail("invalid hash input storage");
    const unsigned char* p=(const unsigned char*)data;uint64_t hash=UINT64_C(0xcbf29ce484222325);
    for(uint64_t i=0;i<len;i++){hash^=p[i];hash*=UINT64_C(0x100000001b3);}
    return hash;
}
static const uint32_t zn_sha256_k[64]={
    0x428a2f98u,0x71374491u,0xb5c0fbcfu,0xe9b5dba5u,0x3956c25bu,0x59f111f1u,0x923f82a4u,0xab1c5ed5u,
    0xd807aa98u,0x12835b01u,0x243185beu,0x550c7dc3u,0x72be5d74u,0x80deb1feu,0x9bdc06a7u,0xc19bf174u,
    0xe49b69c1u,0xefbe4786u,0x0fc19dc6u,0x240ca1ccu,0x2de92c6fu,0x4a7484aau,0x5cb0a9dcu,0x76f988dau,
    0x983e5152u,0xa831c66du,0xb00327c8u,0xbf597fc7u,0xc6e00bf3u,0xd5a79147u,0x06ca6351u,0x14292967u,
    0x27b70a85u,0x2e1b2138u,0x4d2c6dfcu,0x53380d13u,0x650a7354u,0x766a0abbu,0x81c2c92eu,0x92722c85u,
    0xa2bfe8a1u,0xa81a664bu,0xc24b8b70u,0xc76c51a3u,0xd192e819u,0xd6990624u,0xf40e3585u,0x106aa070u,
    0x19a4c116u,0x1e376c08u,0x2748774cu,0x34b0bcb5u,0x391c0cb3u,0x4ed8aa4au,0x5b9cca4fu,0x682e6ff3u,
    0x748f82eeu,0x78a5636fu,0x84c87814u,0x8cc70208u,0x90befffau,0xa4506cebu,0xbef9a3f7u,0xc67178f2u};
/* FIPS 180-4 section 6.2.2 over whole 64-byte blocks. */
static void zn_sha256_blocks(uint32_t* state,const unsigned char* p,size_t blocks) {
    for(;blocks;blocks--,p+=64) {
        uint32_t w[64];
        for(int i=0;i<16;i++){uint32_t v;memcpy(&v,p+4*i,4);w[i]=_byteswap_ulong(v);}
        for(int i=16;i<64;i++) {
            uint32_t x=w[i-15],y=w[i-2];
            w[i]=w[i-16]+(_rotr(x,7)^_rotr(x,18)^(x>>3))+w[i-7]+(_rotr(y,17)^_rotr(y,19)^(y>>10));
        }
        uint32_t a=state[0],b=state[1],c=state[2],d=state[3],e=state[4],f=state[5],g=state[6],h=state[7];
        for(int i=0;i<64;i++) {
            uint32_t t1=h+(_rotr(e,6)^_rotr(e,11)^_rotr(e,25))+((e&f)^(~e&g))+zn_sha256_k[i]+w[i];
            uint32_t t2=(_rotr(a,2)^_rotr(a,13)^_rotr(a,22))+((a&b)^(a&c)^(b&c));
            h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;
        }
        state[0]+=a;state[1]+=b;state[2]+=c;state[3]+=d;state[4]+=e;state[5]+=f;state[6]+=g;state[7]+=h;
    }
}
zn_text zn_sha256_bytes(const void* data,uint64_t len) {
    if((!data && len) || len>(uint64_t)INT64_MAX) zn_fail("invalid hash input storage");
    uint32_t state[8]={0x6a09e667u,0xbb67ae85u,0x3c6ef372u,0xa54ff53au,0x510e527fu,0x9b05688cu,0x1f83d9abu,0x5be0cd19u};
    const unsigned char* p=(const unsigned char*)data;
    size_t full=(size_t)(len/64),rest=(size_t)(len%64);
    if(full) zn_sha256_blocks(state,p,full);
    /* Padding: 0x80, zeros, then the 64-bit big-endian bit length (one or two blocks). */
    unsigned char tail[128];memset(tail,0,sizeof(tail));
    if(rest) memcpy(tail,p+full*64,rest);
    tail[rest]=0x80;
    size_t blocks=rest<56?1:2;
    uint64_t bits=_byteswap_uint64(len<<3);
    memcpy(tail+blocks*64-8,&bits,8);
    zn_sha256_blocks(state,tail,blocks);
    static const char hex[]="0123456789abcdef";
    zn_text out={(char*)zn_alloc(65),64};
    for(int i=0;i<32;i++){unsigned byte=(state[i/4]>>(24-8*(i%4)))&0xffu;out.data[2*i]=hex[byte>>4];out.data[2*i+1]=hex[byte&15u];}
    out.data[64]=0;return out;
}
void zn_abi_sha256_bytes(zn_text* out,const void* data,uint64_t len){*out=zn_sha256_bytes(data,len);}
/* N2: the zn_wire_write range predicate; faults only for hand-written C. */
bool zn_wire_write_be(void* data,uint64_t len,int64_t offset,uint64_t value,uint64_t width) {
    if(width!=2 && width!=4 && width!=8) zn_fail("invalid packet width");
    if(!data && len) zn_fail("packet storage overflow");
    if(offset<0 || (uint64_t)offset>len || len-(uint64_t)offset<width) return false;
    unsigned char* p=(unsigned char*)data+(size_t)offset;
    if(width==2){uint16_t v=_byteswap_ushort((uint16_t)value);memcpy(p,&v,2);}
    else if(width==4){uint32_t v=_byteswap_ulong((uint32_t)value);memcpy(p,&v,4);}
    else{uint64_t v=_byteswap_uint64(value);memcpy(p,&v,8);}
    return true;
}

#if defined(_MSC_VER)
#pragma optimize("s", on)
#endif
/* N6 net_request. loopback:// routes run in memory. http:// and tcp:// reach
   only the literal host 127.0.0.1. ws2_32.dll is resolved on the first socket
   request (LoadLibraryExW from System32 + GetProcAddress, WSAStartup once under
   InitOnceExecuteOnce), so no program gains a Winsock import. The minimal
   Winsock ABI below avoids winsock2.h, which conflicts with a host that included
   <windows.h> without WIN32_LEAN_AND_MEAN before this file. */
#define ZN_NET_LIMIT (16u*1024u*1024u)
#define ZN_NET_HEADER_LIMIT (64u*1024u)
#define ZN_NET_TIMEOUT_MS 5000u
#define ZN_NET_NON_LOOPBACK "net_request allows only loopback:// or 127.0.0.1 endpoints"
#define ZN_NET_TIMED_OUT "net_request timed out after 5000 ms"
typedef UINT_PTR zn_net_socket;
typedef struct { short family; unsigned short port; uint32_t address; char zero[8]; } zn_net_address;
typedef struct { unsigned int count; zn_net_socket sockets[1]; } zn_net_set;
typedef struct { long seconds,microseconds; } zn_net_timeval;
typedef struct {
    int (WINAPI* startup)(WORD,void*);
    zn_net_socket (WINAPI* open_socket)(int,int,int,void*,unsigned int,DWORD);
    int (WINAPI* connect_to)(zn_net_socket,const void*,int);
    int (WINAPI* send_bytes)(zn_net_socket,const char*,int,int);
    int (WINAPI* recv_bytes)(zn_net_socket,char*,int,int);
    int (WINAPI* shutdown_socket)(zn_net_socket,int);
    int (WINAPI* close_socket)(zn_net_socket);
    int (WINAPI* set_option)(zn_net_socket,int,int,const char*,int);
    int (WINAPI* get_option)(zn_net_socket,int,int,char*,int*);
    int (WINAPI* set_mode)(zn_net_socket,long,unsigned long*);
    int (WINAPI* wait_ready)(int,zn_net_set*,zn_net_set*,zn_net_set*,const zn_net_timeval*);
    int (WINAPI* last_error)(void);
} zn_net_winsock;
#define ZN_NET_INVALID_SOCKET (~(zn_net_socket)0)
#define ZN_NET_AF_INET 2
#define ZN_NET_SOCK_STREAM 1
#define ZN_NET_IPPROTO_TCP 6
#define ZN_NET_OVERLAPPED_NO_INHERIT 0x81u /* WSA_FLAG_OVERLAPPED|WSA_FLAG_NO_HANDLE_INHERIT */
#define ZN_NET_SOL_SOCKET 0xffff
#define ZN_NET_SO_SNDTIMEO 0x1005
#define ZN_NET_SO_RCVTIMEO 0x1006
#define ZN_NET_SO_ERROR 0x1007
#define ZN_NET_SD_SEND 1
#define ZN_NET_FIONBIO ((long)(int32_t)UINT32_C(0x8004667E))
#define ZN_NET_WOULDBLOCK 10035
#define ZN_NET_ETIMEDOUT 10060
static zn_net_winsock zn_net_ws;
static INIT_ONCE zn_net_ws_once=INIT_ONCE_STATIC_INIT;
static BOOL CALLBACK zn_net_ws_load(PINIT_ONCE once,PVOID parameter,PVOID* context) {
    (void)once;(void)context;DWORD* failure=(DWORD*)parameter;
    HMODULE module=LoadLibraryExW(L"ws2_32.dll",NULL,LOAD_LIBRARY_SEARCH_SYSTEM32);
    if(!module){*failure=GetLastError();return FALSE;}
    zn_net_winsock ws;
    ws.startup=(int (WINAPI*)(WORD,void*))GetProcAddress(module,"WSAStartup");
    ws.open_socket=(zn_net_socket (WINAPI*)(int,int,int,void*,unsigned int,DWORD))GetProcAddress(module,"WSASocketW");
    ws.connect_to=(int (WINAPI*)(zn_net_socket,const void*,int))GetProcAddress(module,"connect");
    ws.send_bytes=(int (WINAPI*)(zn_net_socket,const char*,int,int))GetProcAddress(module,"send");
    ws.recv_bytes=(int (WINAPI*)(zn_net_socket,char*,int,int))GetProcAddress(module,"recv");
    ws.shutdown_socket=(int (WINAPI*)(zn_net_socket,int))GetProcAddress(module,"shutdown");
    ws.close_socket=(int (WINAPI*)(zn_net_socket))GetProcAddress(module,"closesocket");
    ws.set_option=(int (WINAPI*)(zn_net_socket,int,int,const char*,int))GetProcAddress(module,"setsockopt");
    ws.get_option=(int (WINAPI*)(zn_net_socket,int,int,char*,int*))GetProcAddress(module,"getsockopt");
    ws.set_mode=(int (WINAPI*)(zn_net_socket,long,unsigned long*))GetProcAddress(module,"ioctlsocket");
    ws.wait_ready=(int (WINAPI*)(int,zn_net_set*,zn_net_set*,zn_net_set*,const zn_net_timeval*))GetProcAddress(module,"select");
    ws.last_error=(int (WINAPI*)(void))GetProcAddress(module,"WSAGetLastError");
    if(!ws.startup || !ws.open_socket || !ws.connect_to || !ws.send_bytes || !ws.recv_bytes || !ws.shutdown_socket
        || !ws.close_socket || !ws.set_option || !ws.get_option || !ws.set_mode || !ws.wait_ready || !ws.last_error) {
        *failure=ERROR_PROC_NOT_FOUND;FreeLibrary(module);return FALSE;
    }
    uint64_t data[128]; /* WSADATA is 408 bytes on x64 */
    int started=ws.startup(MAKEWORD(2,2),data);
    if(started){*failure=(DWORD)started;FreeLibrary(module);return FALSE;}
    zn_net_ws=ws; /* published by InitOnceExecuteOnce's barrier; never unloaded */
    return TRUE;
}
static zn_net_result zn_net_error(const char* message,const char* kind,unsigned long code) {
    zn_net_result r;memset(&r,0,sizeof(r));r.tag=2;
    if(kind){char text[160];snprintf(text,sizeof(text),"%s (%s error %lu)",message,kind,code);r.data.error=zn_text_make(text);}
    else r.data.error=zn_text_make(message);
    return r;
}
static zn_net_result zn_net_ok(int64_t status,zn_text body) {
    zn_net_result r;memset(&r,0,sizeof(r));r.tag=1;r.data.response.status=status;r.data.response.body=body;return r;
}
static zn_text zn_net_copy(zn_text t) {
    size_t n=zn_text_size(t);zn_text out={(char*)zn_alloc(n+1),n};
    if(n)memcpy(out.data,t.data,n);
    out.data[n]=0;return out;
}
/* Strict UTF-8 (Unicode Table 3-7: no overlongs, surrogates or > U+10FFFF), no NUL. */
static bool zn_net_utf8(const unsigned char* p,size_t n) {
    for(size_t i=0;i<n;) {
        unsigned c=p[i];
        if(c<0x80u){if(!c)return false;i++;continue;}
        size_t need;unsigned low=0x80u,high=0xBFu;
        if(c>=0xC2u && c<=0xDFu)need=1;
        else if(c==0xE0u){need=2;low=0xA0u;}
        else if((c>=0xE1u && c<=0xECu) || c==0xEEu || c==0xEFu)need=2;
        else if(c==0xEDu){need=2;high=0x9Fu;}
        else if(c==0xF0u){need=3;low=0x90u;}
        else if(c>=0xF1u && c<=0xF3u)need=3;
        else if(c==0xF4u){need=3;high=0x8Fu;}
        else return false;
        if(n-i-1<need || p[i+1]<low || p[i+1]>high)return false;
        for(size_t k=2;k<=need;k++)if(p[i+k]<0x80u || p[i+k]>0xBFu)return false;
        i+=need+1;
    }
    return true;
}
static bool zn_net_digit(char c){return c>='0' && c<='9';}
static bool zn_net_name(const char* p,size_t n,const char* lower) {
    if(strlen(lower)!=n)return false;
    for(size_t i=0;i<n;i++){char c=p[i];if(c>='A' && c<='Z')c=(char)(c-'A'+'a');if(c!=lower[i])return false;}
    return true;
}
/* An HTTP/1.x response head within the first 64 KiB: 1 parsed, 0 incomplete,
   -1 malformed. *content is SIZE_MAX without Content-Length, and is clamped to
   ZN_NET_LIMIT+1 when larger than the response limit. */
static int zn_net_http_head(const char* p,size_t n,int64_t* status,size_t* body_at,size_t* content) {
    size_t scan=n<ZN_NET_HEADER_LIMIT?n:ZN_NET_HEADER_LIMIT,end=SIZE_MAX;
    for(size_t i=0;i+3<scan;i++)if(p[i]=='\r' && p[i+1]=='\n' && p[i+2]=='\r' && p[i+3]=='\n'){end=i;break;}
    if(end==SIZE_MAX)return n>=ZN_NET_HEADER_LIMIT?-1:0;
    size_t line=0;while(!(p[line]=='\r' && p[line+1]=='\n'))line++;
    if(line<12 || memcmp(p,"HTTP/1.",7) || !zn_net_digit(p[7]) || p[8]!=' ' || !zn_net_digit(p[9])
        || !zn_net_digit(p[10]) || !zn_net_digit(p[11]) || (line>12 && p[12]!=' '))return -1;
    int code=(p[9]-'0')*100+(p[10]-'0')*10+(p[11]-'0');
    if(code<100 || code>599)return -1;
    size_t length=SIZE_MAX;
    for(size_t at=line+2;at<end+2;) {
        size_t stop=at;while(!(p[stop]=='\r' && p[stop+1]=='\n'))stop++;
        const char* colon=(const char*)memchr(p+at,':',stop-at);
        if(colon && zn_net_name(p+at,(size_t)(colon-(p+at)),"content-length")) {
            size_t v=(size_t)(colon-p)+1,digits=0,value=0;
            while(v<stop && (p[v]==' ' || p[v]=='\t'))v++;
            while(v<stop && zn_net_digit(p[v])){value=value*10+(size_t)(p[v]-'0');if(value>ZN_NET_LIMIT)value=(size_t)ZN_NET_LIMIT+1;v++;digits++;}
            while(v<stop && (p[v]==' ' || p[v]=='\t'))v++;
            if(!digits || v!=stop || length!=SIZE_MAX)return -1;
            length=value;
        }
        at=stop+2;
    }
    *status=code;*body_at=end+4;*content=length;return 1;
}
static DWORD zn_net_remaining(ULONGLONG deadline) {
    ULONGLONG now=GetTickCount64();return now>=deadline?0:(DWORD)(deadline-now);
}
/* One connection to 127.0.0.1:port. ZN_NET_TIMEOUT_MS bounds the whole exchange:
   connect waits in select, and each send/recv runs with SO_SNDTIMEO/SO_RCVTIMEO
   set to the remaining budget. TCP half-closes after the payload and reads until
   the peer closes. HTTP reads until close or until Content-Length bytes arrive.
   On success *body is a fresh NUL-terminated owner of exactly *size+1 bytes. */
static const char* zn_net_exchange(unsigned port,const char* request,size_t request_len,bool tcp,
    char** body,size_t* size,int64_t* status,unsigned long* code) {
    const zn_net_winsock* ws=&zn_net_ws;
    ULONGLONG deadline=GetTickCount64()+ZN_NET_TIMEOUT_MS;
    const char* error=NULL;char* data=NULL;size_t len=0,cap=0;
    size_t limit=tcp?(size_t)ZN_NET_LIMIT:(size_t)ZN_NET_LIMIT+ZN_NET_HEADER_LIMIT;
    size_t body_at=0,content=SIZE_MAX;bool head=false;DWORD wait=0;
    zn_net_socket s=ws->open_socket(ZN_NET_AF_INET,ZN_NET_SOCK_STREAM,ZN_NET_IPPROTO_TCP,NULL,0,ZN_NET_OVERLAPPED_NO_INHERIT);
    if(s==ZN_NET_INVALID_SOCKET){*code=(unsigned long)ws->last_error();return "net_request cannot create socket";}
    unsigned long mode=1;
    if(ws->set_mode(s,ZN_NET_FIONBIO,&mode)){*code=(unsigned long)ws->last_error();error="net_request cannot create socket";goto done;}
    zn_net_address address={ZN_NET_AF_INET,_byteswap_ushort((unsigned short)port),_byteswap_ulong(0x7F000001u),{0}};
    if(ws->connect_to(s,&address,(int)sizeof(address))) {
        int failure=ws->last_error();
        if(failure!=ZN_NET_WOULDBLOCK){*code=(unsigned long)failure;error="net_request connection failed";goto done;}
        wait=zn_net_remaining(deadline);
        zn_net_set writable={1,{s}},failed={1,{s}};
        zn_net_timeval limit_time={(long)(wait/1000u),(long)(wait%1000u)*1000L};
        int ready=ws->wait_ready(0,NULL,&writable,&failed,&limit_time);
        if(!ready){error=ZN_NET_TIMED_OUT;goto done;}
        if(ready<0){*code=(unsigned long)ws->last_error();error="net_request connection failed";goto done;}
        int result=0,result_size=(int)sizeof(result);
        if(ws->get_option(s,ZN_NET_SOL_SOCKET,ZN_NET_SO_ERROR,(char*)&result,&result_size)){*code=(unsigned long)ws->last_error();error="net_request connection failed";goto done;}
        if(result || failed.count || !writable.count){*code=(unsigned long)result;error="net_request connection failed";goto done;}
    }
    mode=0;
    if(ws->set_mode(s,ZN_NET_FIONBIO,&mode)){*code=(unsigned long)ws->last_error();error="net_request connection failed";goto done;}
    for(size_t at=0;at<request_len;) {
        wait=zn_net_remaining(deadline);
        if(!wait){error=ZN_NET_TIMED_OUT;goto done;}
        if(ws->set_option(s,ZN_NET_SOL_SOCKET,ZN_NET_SO_SNDTIMEO,(const char*)&wait,(int)sizeof(wait))){*code=(unsigned long)ws->last_error();error="net_request send failed";goto done;}
        int chunk=request_len-at>65536u?65536:(int)(request_len-at);
        int sent=ws->send_bytes(s,request+at,chunk,0);
        if(sent<=0) {
            int failure=ws->last_error();
            if(failure==ZN_NET_ETIMEDOUT)error=ZN_NET_TIMED_OUT;else{*code=(unsigned long)failure;error="net_request send failed";}
            goto done;
        }
        at+=(size_t)sent;
    }
    if(tcp && ws->shutdown_socket(s,ZN_NET_SD_SEND)){*code=(unsigned long)ws->last_error();error="net_request send failed";goto done;}
    for(;;) {
        if(len==cap) { /* len <= limit here, so the buffer always grows */
            size_t next=cap?cap*2:8192u;if(next>limit+1)next=limit+1;
            char* grown=(char*)realloc(data,next);if(!grown)zn_fail("allocation failed");
            data=grown;cap=next;
        }
        wait=zn_net_remaining(deadline);
        if(!wait){error=ZN_NET_TIMED_OUT;goto done;}
        if(ws->set_option(s,ZN_NET_SOL_SOCKET,ZN_NET_SO_RCVTIMEO,(const char*)&wait,(int)sizeof(wait))){*code=(unsigned long)ws->last_error();error="net_request receive failed";goto done;}
        int got=ws->recv_bytes(s,data+len,(int)(cap-len),0);
        if(!got)break;
        if(got<0) {
            int failure=ws->last_error();
            if(failure==ZN_NET_ETIMEDOUT)error=ZN_NET_TIMED_OUT;else{*code=(unsigned long)failure;error="net_request receive failed";}
            goto done;
        }
        len+=(size_t)got;
        if(len>limit){error="net_request response exceeded 16 MiB";goto done;}
        if(!tcp) {
            if(!head) {
                int parsed=zn_net_http_head(data,len,status,&body_at,&content);
                if(parsed<0){error="net_request received a malformed HTTP response";goto done;}
                head=parsed>0;
                if(head && content!=SIZE_MAX && content>ZN_NET_LIMIT){error="net_request response exceeded 16 MiB";goto done;}
            }
            if(head && content!=SIZE_MAX && len-body_at>=content)break;
        }
    }
    if(!tcp) {
        if(!head){error="net_request received a malformed HTTP response";goto done;}
        size_t bytes=len-body_at;
        if(content!=SIZE_MAX){if(bytes<content){error="net_request HTTP body is shorter than Content-Length";goto done;}bytes=content;}
        if(bytes>ZN_NET_LIMIT){error="net_request response exceeded 16 MiB";goto done;}
        memmove(data,data+body_at,bytes);len=bytes;
    } else *status=0;
    {
        char* exact=(char*)realloc(data,len+1);if(!exact)zn_fail("allocation failed");
        data=exact;data[len]=0;
    }
    if(!zn_net_utf8((const unsigned char*)data,len)){error="net_request response must be UTF-8 text without NUL bytes";goto done;}
done:
    ws->close_socket(s);
    if(error){free(data);return error;}
    *body=data;*size=len;return NULL;
}
zn_net_result zn_net_request(zn_text endpoint,zn_text payload) {
    size_t en=zn_text_size(endpoint),pn=zn_text_size(payload);const char* e=endpoint.data;
    if(en>ZN_NET_LIMIT || (en && (!e || memchr(e,0,en))))
        return zn_net_error("net_request endpoint must be at most 16 MiB without NUL bytes",NULL,0);
    if(pn>ZN_NET_LIMIT || (pn && (!payload.data || memchr(payload.data,0,pn))))
        return zn_net_error("net_request payload must be at most 16 MiB without NUL bytes",NULL,0);
    if(en>=11 && !memcmp(e,"loopback://",11)) {
        const char* route=e+11;size_t rn=en-11;
        if(rn==4 && !memcmp(route,"echo",4))return zn_net_ok(200,zn_net_copy(payload));
        if(rn==6 && !memcmp(route,"sha256",6))return zn_net_ok(200,zn_sha256_bytes(payload.data,pn));
        if(rn==10 && !memcmp(route,"head-fnv1a",10)) {
            uint64_t hash=zn_fnv1a_64(payload.data,pn);int64_t value;memcpy(&value,&hash,sizeof(value));
            return zn_net_ok(200,zn_int_text(value));
        }
        if(rn>=7 && !memcmp(route,"status/",7)) {
            if(rn!=10 || !zn_net_digit(route[7]) || !zn_net_digit(route[8]) || !zn_net_digit(route[9]))
                return zn_net_error("net_request loopback status must be three digits from 100 to 599",NULL,0);
            int code=(route[7]-'0')*100+(route[8]-'0')*10+(route[9]-'0');
            if(code<100 || code>599)return zn_net_error("net_request loopback status must be three digits from 100 to 599",NULL,0);
            return zn_net_ok(code,zn_net_copy(payload));
        }
        return zn_net_error("net_request unknown loopback route",NULL,0);
    }
    bool tcp;size_t at;
    if(en>=7 && !memcmp(e,"http://",7)){tcp=false;at=7;}
    else if(en>=6 && !memcmp(e,"tcp://",6)){tcp=true;at=6;}
    else return zn_net_error(ZN_NET_NON_LOOPBACK,NULL,0);
    size_t host=at;while(at<en && e[at]!=':' && e[at]!='/')at++;
    if(at-host!=9 || memcmp(e+host,"127.0.0.1",9))return zn_net_error(ZN_NET_NON_LOOPBACK,NULL,0);
    /* Parsing only: nothing below connects before the endpoint is fully valid. */
    unsigned port=0;size_t first=at+1,digits=0;
    if(at<en && e[at]==':')for(at++;at<en && zn_net_digit(e[at]) && digits<6;at++,digits++)port=port*10u+(unsigned)(e[at]-'0');
    const char* path="/";size_t path_len=1;bool shape=digits>=1 && digits<=5 && e[first]!='0' && port<=65535u;
    if(shape && tcp)shape=at==en;
    else if(shape && at<en){shape=e[at]=='/';path=e+at;path_len=en-at;}
    if(!shape)return zn_net_error("net_request endpoint must be http://127.0.0.1:<port>/<path> or tcp://127.0.0.1:<port> with port 1 to 65535",NULL,0);
    for(size_t i=0;i<path_len;i++)if((unsigned char)path[i]<=0x20u || (unsigned char)path[i]>=0x7Fu)
        return zn_net_error("net_request HTTP path must be printable ASCII without spaces",NULL,0);
    DWORD failure=0;
    if(!InitOnceExecuteOnce(&zn_net_ws_once,zn_net_ws_load,&failure,NULL))
        return zn_net_error("net_request cannot load Winsock",failure?"Windows":NULL,failure);
    const char* request=payload.data;size_t request_len=pn;char* owned=NULL;
    if(!tcp) {
        size_t room=path_len+160u+pn;owned=(char*)zn_alloc(room);
        int head=snprintf(owned,room,"POST %.*s HTTP/1.0\r\nHost: 127.0.0.1:%u\r\nContent-Length: %zu\r\nConnection: close\r\n\r\n",(int)path_len,path,port,pn);
        if(head<0 || (size_t)head>=room-pn)zn_fail("net_request header formatting failed");
        if(pn)memcpy(owned+head,payload.data,pn);
        request=owned;request_len=(size_t)head+pn;
    }
    char* body=NULL;size_t size=0;int64_t status=0;unsigned long code=0;
    const char* error=zn_net_exchange(port,request,request_len,tcp,&body,&size,&status,&code);
    free(owned);
    if(error)return zn_net_error(error,code?"Winsock":NULL,code);
    zn_text text={body,size};
    return zn_net_ok(status,text);
}
void zn_abi_net_request(void* out,const zn_text* endpoint,const zn_text* payload) {
    zn_net_result result=zn_net_request(*endpoint,*payload);memcpy(out,&result,sizeof(result));
}
/* v1.3 OS/runtime primitives. Only the 1.3 builtins call these helpers. Paths
   and names go through zn_path (UTF-8 to UTF-16; empty, interior NUL or invalid
   UTF-8 fails before any system call). Byte transfers are limited to 16 MiB. */
#define ZN_V130_LIMIT (16u*1024u*1024u)
static void zn_v130_storage(const void* data,uint64_t len){if(len && !data)zn_fail("byte buffer storage overflow");}
int64_t zn_try_read_bytes(zn_text path,void* dst,uint64_t cap) {
    zn_v130_storage(dst,cap);
    wchar_t* name=zn_path(path);
    if(!name)return -1;
    /* The share mode excludes writers and deleters, so the size checked below
       cannot change while the handle is open: dst is written only once the whole
       file is known to fit. Only a device error during ReadFile itself can leave
       dst partially written (the result is still -1). */
    HANDLE file=CreateFileW(name,GENERIC_READ,FILE_SHARE_READ,NULL,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL|FILE_FLAG_SEQUENTIAL_SCAN,NULL);
    free(name);
    if(file==INVALID_HANDLE_VALUE)return -1;
    int64_t result=-1;LARGE_INTEGER size;
    if(GetFileType(file)==FILE_TYPE_DISK && GetFileSizeEx(file,&size) && size.QuadPart>=0
        && (uint64_t)size.QuadPart<=cap && (uint64_t)size.QuadPart<=ZN_V130_LIMIT) {
        DWORD total=(DWORD)size.QuadPart,done=0;
        while(done<total) {
            DWORD got=0;
            if(!ReadFile(file,(unsigned char*)dst+done,total-done,&got,NULL) || !got)break;
            done+=got;
        }
        if(done==total)result=(int64_t)total;
    }
    CloseHandle(file);
    return result;
}
bool zn_try_write_bytes(zn_text path,const void* src,uint64_t len) {
    zn_v130_storage(src,len);
    if(len>ZN_V130_LIMIT)return false;
    wchar_t* name=zn_path(path);
    if(!name)return false;
    HANDLE file=CreateFileW(name,GENERIC_WRITE,0,NULL,CREATE_ALWAYS,FILE_ATTRIBUTE_NORMAL,NULL);
    free(name);
    if(file==INVALID_HANDLE_VALUE)return false;
    bool ok=true;uint64_t done=0;
    while(ok && done<len) {
        DWORD wrote=0;
        ok=WriteFile(file,(const unsigned char*)src+done,(DWORD)(len-done),&wrote,NULL) && wrote;
        done+=wrote;
    }
    if(!CloseHandle(file))ok=false;
    return ok;
}
/* Empty, '='-containing, NUL-containing, unset, invalid (lone surrogate) or
   over-16-MiB values all give "". */
zn_text zn_env_var(zn_text name) {
    zn_text out=zn_text_make("");
    size_t n=zn_text_size(name);
    if(!n || n>ZN_V130_LIMIT || !name.data || memchr(name.data,'=',n))return out;
    wchar_t* wide=zn_path(name);
    if(!wide)return out;
    for(int attempt=0;attempt<4;attempt++) {
        DWORD need=GetEnvironmentVariableW(wide,NULL,0);
        if(!need || need>ZN_V130_LIMIT+1u)break;
        wchar_t* value=(wchar_t*)zn_alloc((size_t)need*sizeof(wchar_t));
        DWORD got=GetEnvironmentVariableW(wide,value,need);
        if(got && got<need) {
            int bytes=WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,value,(int)got,NULL,0,NULL,NULL);
            if(bytes>0 && (size_t)bytes<=ZN_V130_LIMIT) {
                char* data=(char*)zn_alloc((size_t)bytes+1);
                if(WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,value,(int)got,data,bytes,NULL,NULL)==bytes) {
                    data[bytes]=0;free(out.data);out.data=data;out.len=(size_t)bytes;
                } else free(data);
            }
            free(value);
            break;
        }
        free(value);
        if(!got)break; /* removed between the two calls; a grown value retries */
    }
    free(wide);
    return out;
}
bool zn_create_dir(zn_text path) {
    wchar_t* name=zn_path(path);
    if(!name)return false;
    bool ok=CreateDirectoryW(name,NULL)!=0;
    if(!ok && GetLastError()==ERROR_ALREADY_EXISTS) {
        DWORD attributes=GetFileAttributesW(name);
        ok=attributes!=INVALID_FILE_ATTRIBUTES && (attributes&FILE_ATTRIBUTE_DIRECTORY);
    }
    free(name);
    return ok;
}
bool zn_remove_file(zn_text path) {
    wchar_t* name=zn_path(path);
    if(!name)return false;
    bool ok=DeleteFileW(name)!=0;
    free(name);
    return ok;
}
/* net_serve_once: one deterministic in-memory route, or one connection accepted
   on the literal 127.0.0.1. bind/listen/accept are resolved from the ws2_32.dll
   that zn_net_ws_load already loaded (and never unloads), under their own once. */
#define ZN_SERVE_NON_LOOPBACK "net_serve_once allows only loopback:// or 127.0.0.1 endpoints"
#define ZN_SERVE_TIMED_OUT "net_serve_once timed out after 5000 ms"
#define ZN_SERVE_HTTP_STATUS "net_serve_once status must be 100 to 599 for loopback:// and http:// endpoints"
#define ZN_SERVE_SO_REUSEADDR 4
typedef struct {
    int (WINAPI* bind_to)(zn_net_socket,const void*,int);
    int (WINAPI* listen_on)(zn_net_socket,int);
    zn_net_socket (WINAPI* accept_from)(zn_net_socket,void*,int*);
} zn_net_server_winsock;
static zn_net_server_winsock zn_net_server;
static INIT_ONCE zn_net_server_once=INIT_ONCE_STATIC_INIT;
static BOOL CALLBACK zn_net_server_load(PINIT_ONCE once,PVOID parameter,PVOID* context) {
    (void)once;(void)context;DWORD* failure=(DWORD*)parameter;
    HMODULE module=GetModuleHandleW(L"ws2_32.dll");
    if(!module){*failure=GetLastError();return FALSE;}
    zn_net_server_winsock ws;
    ws.bind_to=(int (WINAPI*)(zn_net_socket,const void*,int))GetProcAddress(module,"bind");
    ws.listen_on=(int (WINAPI*)(zn_net_socket,int))GetProcAddress(module,"listen");
    ws.accept_from=(zn_net_socket (WINAPI*)(zn_net_socket,void*,int*))GetProcAddress(module,"accept");
    if(!ws.bind_to || !ws.listen_on || !ws.accept_from){*failure=ERROR_PROC_NOT_FOUND;return FALSE;}
    zn_net_server=ws;
    return TRUE;
}
/* One recv into the growable buffer (*data, *len, *cap; never above limit+1):
   1 data, 0 peer closed, -1 error (*error set). */
static int zn_serve_recv(zn_net_socket s,char** data,size_t* len,size_t* cap,size_t limit,ULONGLONG deadline,const char** error,unsigned long* code) {
    const zn_net_winsock* ws=&zn_net_ws;
    if(*len==*cap) {
        size_t next=*cap?*cap*2:8192u;if(next>limit+1)next=limit+1;
        char* grown=(char*)realloc(*data,next);if(!grown)zn_fail("allocation failed");
        *data=grown;*cap=next;
    }
    DWORD wait=zn_net_remaining(deadline);
    if(!wait){*error=ZN_SERVE_TIMED_OUT;return -1;}
    if(ws->set_option(s,ZN_NET_SOL_SOCKET,ZN_NET_SO_RCVTIMEO,(const char*)&wait,(int)sizeof(wait))){*code=(unsigned long)ws->last_error();*error="net_serve_once receive failed";return -1;}
    int got=ws->recv_bytes(s,*data+*len,(int)(*cap-*len),0);
    if(!got)return 0;
    if(got<0) {
        int failure=ws->last_error();
        if(failure==ZN_NET_ETIMEDOUT)*error=ZN_SERVE_TIMED_OUT;else{*code=(unsigned long)failure;*error="net_serve_once receive failed";}
        return -1;
    }
    *len+=(size_t)got;
    if(*len>limit){*error="net_serve_once request exceeded 16 MiB";return -1;}
    return 1;
}
static const char* zn_serve_send(zn_net_socket s,const char* p,size_t n,ULONGLONG deadline,unsigned long* code) {
    const zn_net_winsock* ws=&zn_net_ws;
    for(size_t at=0;at<n;) {
        DWORD wait=zn_net_remaining(deadline);
        if(!wait)return ZN_SERVE_TIMED_OUT;
        if(ws->set_option(s,ZN_NET_SOL_SOCKET,ZN_NET_SO_SNDTIMEO,(const char*)&wait,(int)sizeof(wait))){*code=(unsigned long)ws->last_error();return "net_serve_once send failed";}
        int chunk=n-at>65536u?65536:(int)(n-at);
        int sent=ws->send_bytes(s,p+at,chunk,0);
        if(sent<=0) {
            int failure=ws->last_error();
            if(failure==ZN_NET_ETIMEDOUT)return ZN_SERVE_TIMED_OUT;
            *code=(unsigned long)failure;return "net_serve_once send failed";
        }
        at+=(size_t)sent;
    }
    return NULL;
}
/* Best effort: tell a rejected HTTP client why before closing. */
static void zn_serve_refuse(zn_net_socket s,const char* status_line,ULONGLONG deadline) {
    char head[128];unsigned long ignored=0;
    int n=snprintf(head,sizeof(head),"HTTP/1.0 %s\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",status_line);
    if(n>0 && (size_t)n<sizeof(head))(void)zn_serve_send(s,head,(size_t)n,deadline,&ignored);
}
/* An HTTP/1.x request head within the first 64 KiB: 1 parsed, 0 incomplete,
   -1 malformed. *content is 0 without Content-Length and is clamped to
   ZN_NET_LIMIT+1 when larger than the request limit. */
static int zn_serve_http_head(const char* p,size_t n,size_t* target,size_t* target_len,size_t* body_at,size_t* content) {
    size_t scan=n<ZN_NET_HEADER_LIMIT?n:ZN_NET_HEADER_LIMIT,end=SIZE_MAX;
    for(size_t i=0;i+3<scan;i++)if(p[i]=='\r' && p[i+1]=='\n' && p[i+2]=='\r' && p[i+3]=='\n'){end=i;break;}
    if(end==SIZE_MAX)return n>=ZN_NET_HEADER_LIMIT?-1:0;
    size_t line=0;while(!(p[line]=='\r' && p[line+1]=='\n'))line++;
    size_t method=0;while(method<line && p[method]>='A' && p[method]<='Z')method++;
    if(!method || method>=line || p[method]!=' ')return -1;
    size_t start=method+1,stop=start;while(stop<line && p[stop]!=' ')stop++;
    if(stop==start || line-stop!=9 || memcmp(p+stop," HTTP/1.",8) || !zn_net_digit(p[stop+8]))return -1;
    size_t length=SIZE_MAX;
    for(size_t at=line+2;at<end+2;) {
        size_t next=at;while(!(p[next]=='\r' && p[next+1]=='\n'))next++;
        const char* colon=(const char*)memchr(p+at,':',next-at);
        if(colon && zn_net_name(p+at,(size_t)(colon-(p+at)),"content-length")) {
            size_t v=(size_t)(colon-p)+1,digits=0,value=0;
            while(v<next && (p[v]==' ' || p[v]=='\t'))v++;
            while(v<next && zn_net_digit(p[v])){value=value*10+(size_t)(p[v]-'0');if(value>ZN_NET_LIMIT)value=(size_t)ZN_NET_LIMIT+1;v++;digits++;}
            while(v<next && (p[v]==' ' || p[v]=='\t'))v++;
            if(!digits || v!=next || length!=SIZE_MAX)return -1;
            length=value;
        }
        at=next+2;
    }
    *target=start;*target_len=stop-start;*body_at=end+4;*content=length==SIZE_MAX?0:length;
    return 1;
}
zn_net_result zn_net_serve_once(zn_text endpoint,int64_t status,zn_text reply_body) {
    size_t en=zn_text_size(endpoint),rn=zn_text_size(reply_body);const char* e=endpoint.data;
    if(en>ZN_NET_LIMIT || (en && (!e || memchr(e,0,en))))
        return zn_net_error("net_serve_once endpoint must be at most 16 MiB without NUL bytes",NULL,0);
    if(rn>ZN_NET_LIMIT || (rn && (!reply_body.data || memchr(reply_body.data,0,rn))))
        return zn_net_error("net_serve_once reply must be at most 16 MiB without NUL bytes",NULL,0);
    bool http_status=status>=100 && status<=599;
    if(en>=11 && !memcmp(e,"loopback://",11)) {
        const char* route=e+11;size_t rest=en-11;
        if(rest<6 || memcmp(route,"serve/",6))return zn_net_error("net_serve_once unknown loopback route",NULL,0);
        if(!http_status)return zn_net_error(ZN_SERVE_HTTP_STATUS,NULL,0);
        zn_text request={(char*)(route+6),rest-6};
        return zn_net_ok(status,zn_net_copy(request));
    }
    bool tcp;size_t at;
    if(en>=7 && !memcmp(e,"http://",7)){tcp=false;at=7;}
    else if(en>=6 && !memcmp(e,"tcp://",6)){tcp=true;at=6;}
    else return zn_net_error(ZN_SERVE_NON_LOOPBACK,NULL,0);
    size_t host=at;while(at<en && e[at]!=':' && e[at]!='/')at++;
    if(at-host!=9 || memcmp(e+host,"127.0.0.1",9))return zn_net_error(ZN_SERVE_NON_LOOPBACK,NULL,0);
    /* Parsing only: nothing below binds before the endpoint is fully valid. */
    unsigned port=0;size_t first=at+1,digits=0;
    if(at<en && e[at]==':')for(at++;at<en && zn_net_digit(e[at]) && digits<6;at++,digits++)port=port*10u+(unsigned)(e[at]-'0');
    const char* path="/";size_t path_len=1;bool shape=digits>=1 && digits<=5 && e[first]!='0' && port<=65535u;
    if(shape && tcp)shape=at==en;
    else if(shape && at<en){shape=e[at]=='/';path=e+at;path_len=en-at;}
    if(!shape)return zn_net_error("net_serve_once endpoint must be http://127.0.0.1:<port>/<path> or tcp://127.0.0.1:<port> with port 1 to 65535",NULL,0);
    for(size_t i=0;i<path_len;i++)if((unsigned char)path[i]<=0x20u || (unsigned char)path[i]>=0x7Fu)
        return zn_net_error("net_serve_once HTTP path must be printable ASCII without spaces",NULL,0);
    if(tcp && status!=0)return zn_net_error("net_serve_once status must be 0 for tcp:// endpoints",NULL,0);
    if(!tcp && !http_status)return zn_net_error(ZN_SERVE_HTTP_STATUS,NULL,0);
    DWORD failure=0;
    if(!InitOnceExecuteOnce(&zn_net_ws_once,zn_net_ws_load,&failure,NULL) || !InitOnceExecuteOnce(&zn_net_server_once,zn_net_server_load,&failure,NULL))
        return zn_net_error("net_serve_once cannot load Winsock",failure?"Windows":NULL,failure);
    const zn_net_winsock* ws=&zn_net_ws;const zn_net_server_winsock* server=&zn_net_server;
    ULONGLONG deadline=GetTickCount64()+ZN_NET_TIMEOUT_MS;
    const char* error=NULL;unsigned long code=0;char* data=NULL;char* reply=NULL;
    size_t len=0,cap=0,body_at=0,content=0,target=0,target_len=0;
    bool head=false;int reuse=1,ready=0,received=0;unsigned long mode=1;DWORD wait=0;
    zn_net_result result;memset(&result,0,sizeof(result));
    zn_net_socket client=ZN_NET_INVALID_SOCKET;
    zn_net_socket s=ws->open_socket(ZN_NET_AF_INET,ZN_NET_SOCK_STREAM,ZN_NET_IPPROTO_TCP,NULL,0,ZN_NET_OVERLAPPED_NO_INHERIT);
    if(s==ZN_NET_INVALID_SOCKET)return zn_net_error("net_serve_once cannot create socket","Winsock",(unsigned long)ws->last_error());
    zn_net_address address={ZN_NET_AF_INET,_byteswap_ushort((unsigned short)port),_byteswap_ulong(0x7F000001u),{0}};
    zn_net_set readable={1,{s}};zn_net_timeval limit_time;
    if(ws->set_option(s,ZN_NET_SOL_SOCKET,ZN_SERVE_SO_REUSEADDR,(const char*)&reuse,(int)sizeof(reuse))){code=(unsigned long)ws->last_error();error="net_serve_once cannot create socket";goto done;}
    if(server->bind_to(s,&address,(int)sizeof(address))){code=(unsigned long)ws->last_error();error="net_serve_once cannot bind the endpoint";goto done;}
    if(server->listen_on(s,1) || ws->set_mode(s,ZN_NET_FIONBIO,&mode)){code=(unsigned long)ws->last_error();error="net_serve_once cannot listen on the endpoint";goto done;}
    wait=zn_net_remaining(deadline);
    limit_time.seconds=(long)(wait/1000u);limit_time.microseconds=(long)(wait%1000u)*1000L;
    ready=ws->wait_ready(0,&readable,NULL,NULL,&limit_time);
    if(!ready){error=ZN_SERVE_TIMED_OUT;goto done;}
    if(ready<0){code=(unsigned long)ws->last_error();error="net_serve_once accept failed";goto done;}
    client=server->accept_from(s,NULL,NULL);
    if(client==ZN_NET_INVALID_SOCKET){code=(unsigned long)ws->last_error();error="net_serve_once accept failed";goto done;}
    mode=0;
    if(ws->set_mode(client,ZN_NET_FIONBIO,&mode)){code=(unsigned long)ws->last_error();error="net_serve_once accept failed";goto done;}
    for(;;) {
        received=zn_serve_recv(client,&data,&len,&cap,tcp?(size_t)ZN_NET_LIMIT:(size_t)ZN_NET_LIMIT+ZN_NET_HEADER_LIMIT,deadline,&error,&code);
        if(received<0)goto done;
        if(tcp){if(!received)break;continue;}
        if(!head) {
            int parsed=zn_serve_http_head(data,len,&target,&target_len,&body_at,&content);
            if(parsed<0){zn_serve_refuse(client,"400 Bad Request",deadline);error="net_serve_once received a malformed HTTP request";goto done;}
            head=parsed>0;
            if(head && content>ZN_NET_LIMIT){zn_serve_refuse(client,"413 Payload Too Large",deadline);error="net_serve_once request exceeded 16 MiB";goto done;}
            if(head && (target_len!=path_len || memcmp(data+target,path,path_len))){zn_serve_refuse(client,"404 Not Found",deadline);error="net_serve_once request path does not match the endpoint";goto done;}
        }
        if(head && len-body_at>=content)break;
        if(!received){error=head?"net_serve_once HTTP body is shorter than Content-Length":"net_serve_once received a malformed HTTP request";goto done;}
    }
    if(tcp){body_at=0;content=len;}
    if(!zn_net_utf8((const unsigned char*)data+body_at,content)) {
        if(!tcp)zn_serve_refuse(client,"400 Bad Request",deadline);
        error="net_serve_once request must be UTF-8 text without NUL bytes";goto done;
    }
    if(tcp)error=zn_serve_send(client,reply_body.data,rn,deadline,&code);
    else {
        size_t room=96u+rn;reply=(char*)zn_alloc(room);
        int written=snprintf(reply,room,"HTTP/1.0 %lld OK\r\nContent-Length: %zu\r\nConnection: close\r\n\r\n",(long long)status,rn);
        if(written<0 || (size_t)written>=room-rn)zn_fail("net_serve_once header formatting failed");
        if(rn)memcpy(reply+written,reply_body.data,rn);
        error=zn_serve_send(client,reply,(size_t)written+rn,deadline,&code);
    }
    if(error)goto done;
    ws->shutdown_socket(client,ZN_NET_SD_SEND);
    {
        zn_text body={(char*)zn_alloc(content+1),content};
        if(content)memcpy(body.data,data+body_at,content);
        body.data[content]=0;
        result=zn_net_ok(status,body);
    }
done:
    if(client!=ZN_NET_INVALID_SOCKET)ws->close_socket(client);
    ws->close_socket(s);
    free(data);free(reply);
    if(error)return zn_net_error(error,code?"Winsock":NULL,code);
    return result;
}
int64_t zn_abi_try_read_bytes(const zn_text* path,void* dst,uint64_t cap){return zn_try_read_bytes(*path,dst,cap);}
bool zn_abi_try_write_bytes(const zn_text* path,const void* src,uint64_t len){return zn_try_write_bytes(*path,src,len);}
void zn_abi_env_var(zn_text* out,const zn_text* name){*out=zn_env_var(*name);}
bool zn_abi_create_dir(const zn_text* path){return zn_create_dir(*path);}
bool zn_abi_remove_file(const zn_text* path){return zn_remove_file(*path);}
void zn_abi_net_serve_once(void* out,const zn_text* endpoint,int64_t status,const zn_text* reply_body) {
    zn_net_result result=zn_net_serve_once(*endpoint,status,*reply_body);memcpy(out,&result,sizeof(result));
}
/* keys(m): the keys of a text- or int-keyed map as a new owner, in slot order
   (unspecified, but stable while the map is unchanged). width is the entry
   size; the key is the entry's first field. Text keys are cloned. */
zn_texts zn_map_keys_text(const zn_map* m,uint64_t width) {
    zn_texts out={0};uint64_t cursor=0;void* slot;
    while((slot=zn_map_next_slot(m,&cursor,width))!=NULL)zn_texts_push(&out,zn_text_clone(*(const zn_text*)slot));
    return out;
}
zn_ints zn_map_keys_int(const zn_map* m,uint64_t width) {
    zn_ints out={0};uint64_t cursor=0;void* slot;
    while((slot=zn_map_next_slot(m,&cursor,width))!=NULL) {
        if(out.len==out.cap)zn_ints_grow(&out);
        memcpy(&out.data[out.len++],slot,sizeof(int64_t));
    }
    return out;
}
void zn_abi_map_keys_text(zn_texts* out,const zn_map* m,uint64_t width){*out=zn_map_keys_text(m,width);}
void zn_abi_map_keys_int(zn_ints* out,const zn_map* m,uint64_t width){*out=zn_map_keys_int(m,width);}

/* http_request(url, body): one GET (empty body) or POST to an http:// or
   https:// URL through WinHTTP, with the system proxy settings. loopback://
   URLs are passed to net_request unchanged. winhttp.dll is resolved on the
   first request (LoadLibraryExW from System32 + GetProcAddress, once under
   InitOnceExecuteOnce), so no program gains an import; the minimal ABI below
   avoids winhttp.h for the same reason the Winsock code avoids winsock2.h.
   The URL is parsed and checked before any I/O: ASCII host (IPv6 in brackets),
   optional port 1-65535, printable-ASCII path and query, no user info; a
   fragment is dropped. A non-empty body is sent as application/json when its
   first non-space byte is { or [, else as text/plain; charset=utf-8. Request
   and response are limited to 16 MiB; the response must be strict UTF-8
   without NUL. Timeouts: 10 s resolve, 10 s connect, 30 s send, 30 s receive. */
typedef void* zn_http_handle;
typedef struct {
    zn_http_handle (WINAPI* open)(const wchar_t*,DWORD,const wchar_t*,const wchar_t*,DWORD);
    zn_http_handle (WINAPI* connect)(zn_http_handle,const wchar_t*,WORD,DWORD);
    zn_http_handle (WINAPI* open_request)(zn_http_handle,const wchar_t*,const wchar_t*,const wchar_t*,const wchar_t*,const wchar_t**,DWORD);
    BOOL (WINAPI* set_timeouts)(zn_http_handle,int,int,int,int);
    BOOL (WINAPI* send_request)(zn_http_handle,const wchar_t*,DWORD,void*,DWORD,DWORD,DWORD_PTR);
    BOOL (WINAPI* receive_response)(zn_http_handle,void*);
    BOOL (WINAPI* query_headers)(zn_http_handle,DWORD,const wchar_t*,void*,DWORD*,DWORD*);
    BOOL (WINAPI* read_data)(zn_http_handle,void*,DWORD,DWORD*);
    BOOL (WINAPI* close_handle)(zn_http_handle);
} zn_http_api;
#define ZN_HTTP_ACCESS_TYPE_AUTOMATIC_PROXY 4u
#define ZN_HTTP_FLAG_SECURE 0x00800000u
#define ZN_HTTP_QUERY_STATUS_CODE_NUMBER (19u|0x20000000u) /* WINHTTP_QUERY_STATUS_CODE|WINHTTP_QUERY_FLAG_NUMBER */
static zn_http_api zn_http;
static INIT_ONCE zn_http_once=INIT_ONCE_STATIC_INIT;
static BOOL CALLBACK zn_http_load(PINIT_ONCE once,PVOID parameter,PVOID* context) {
    (void)once;(void)context;DWORD* failure=(DWORD*)parameter;
    HMODULE module=LoadLibraryExW(L"winhttp.dll",NULL,LOAD_LIBRARY_SEARCH_SYSTEM32);
    if(!module){*failure=GetLastError();return FALSE;}
    zn_http_api api;
    api.open=(zn_http_handle (WINAPI*)(const wchar_t*,DWORD,const wchar_t*,const wchar_t*,DWORD))GetProcAddress(module,"WinHttpOpen");
    api.connect=(zn_http_handle (WINAPI*)(zn_http_handle,const wchar_t*,WORD,DWORD))GetProcAddress(module,"WinHttpConnect");
    api.open_request=(zn_http_handle (WINAPI*)(zn_http_handle,const wchar_t*,const wchar_t*,const wchar_t*,const wchar_t*,const wchar_t**,DWORD))GetProcAddress(module,"WinHttpOpenRequest");
    api.set_timeouts=(BOOL (WINAPI*)(zn_http_handle,int,int,int,int))GetProcAddress(module,"WinHttpSetTimeouts");
    api.send_request=(BOOL (WINAPI*)(zn_http_handle,const wchar_t*,DWORD,void*,DWORD,DWORD,DWORD_PTR))GetProcAddress(module,"WinHttpSendRequest");
    api.receive_response=(BOOL (WINAPI*)(zn_http_handle,void*))GetProcAddress(module,"WinHttpReceiveResponse");
    api.query_headers=(BOOL (WINAPI*)(zn_http_handle,DWORD,const wchar_t*,void*,DWORD*,DWORD*))GetProcAddress(module,"WinHttpQueryHeaders");
    api.read_data=(BOOL (WINAPI*)(zn_http_handle,void*,DWORD,DWORD*))GetProcAddress(module,"WinHttpReadData");
    api.close_handle=(BOOL (WINAPI*)(zn_http_handle))GetProcAddress(module,"WinHttpCloseHandle");
    if(!api.open || !api.connect || !api.open_request || !api.set_timeouts || !api.send_request
        || !api.receive_response || !api.query_headers || !api.read_data || !api.close_handle) {
        *failure=ERROR_PROC_NOT_FOUND;FreeLibrary(module);return FALSE;
    }
    zn_http=api; /* published by InitOnceExecuteOnce's barrier; never unloaded */
    return TRUE;
}
/* ASCII bytes widened to a NUL-terminated UTF-16 string. */
static wchar_t* zn_http_wide(const char* s,size_t n) {
    wchar_t* out=(wchar_t*)zn_alloc((n+1)*sizeof(wchar_t));
    for(size_t i=0;i<n;i++)out[i]=(wchar_t)(unsigned char)s[i];
    out[n]=0;return out;
}
static bool zn_http_host_byte(unsigned char c) {
    return (c>='a' && c<='z') || (c>='A' && c<='Z') || (c>='0' && c<='9') || c=='-' || c=='.' || c=='_';
}
zn_net_result zn_http_request(zn_text url,zn_text body) {
    size_t un=zn_text_size(url),bn=zn_text_size(body);const char* u=url.data;
    if(un>ZN_NET_LIMIT || (un && (!u || memchr(u,0,un))))
        return zn_net_error("http_request url must be at most 16 MiB without NUL bytes",NULL,0);
    if(bn>ZN_NET_LIMIT || (bn && (!body.data || memchr(body.data,0,bn))))
        return zn_net_error("http_request body must be at most 16 MiB without NUL bytes",NULL,0);
    if(un>=11 && !memcmp(u,"loopback://",11))return zn_net_request(url,body);
    bool secure;size_t at;
    if(un>=8 && !_strnicmp(u,"https://",8)){secure=true;at=8;}
    else if(un>=7 && !_strnicmp(u,"http://",7)){secure=false;at=7;}
    else return zn_net_error("http_request url must start with http://, https:// or loopback://",NULL,0);
    /* Parsing only: nothing below connects before the URL is fully valid. */
    size_t authority=at;while(at<un && u[at]!='/' && u[at]!='?' && u[at]!='#')at++;
    size_t authority_end=at;
    if(memchr(u+authority,'@',authority_end-authority))
        return zn_net_error("http_request url must not contain user info",NULL,0);
    size_t host=authority,host_end=authority,port_at=authority_end;bool bracketed=false;
    if(host<authority_end && u[host]=='[') {
        const char* close=(const char*)memchr(u+host,']',authority_end-host);
        if(!close)return zn_net_error("http_request url has an unclosed IPv6 address",NULL,0);
        bracketed=true;host_end=(size_t)(close-u)+1;port_at=host_end;
        for(size_t i=host+1;i+1<host_end;i++){unsigned char c=(unsigned char)u[i];
            if(!((c>='0' && c<='9') || (c>='a' && c<='f') || (c>='A' && c<='F') || c==':' || c=='.'))
                return zn_net_error("http_request url has an invalid IPv6 address",NULL,0);}
    } else {
        while(host_end<authority_end && u[host_end]!=':')host_end++;
        port_at=host_end;
        for(size_t i=host;i<host_end;i++)if(!zn_http_host_byte((unsigned char)u[i]))
            return zn_net_error("http_request url host must be ASCII letters, digits, '-', '.' or '_' (use punycode for international names)",NULL,0);
    }
    if(host_end==host || (bracketed && host_end-host<=2))
        return zn_net_error("http_request url needs a host",NULL,0);
    unsigned port=secure?443u:80u;
    if(port_at<authority_end) {
        if(u[port_at]!=':')return zn_net_error("http_request url has an invalid host or port",NULL,0);
        size_t digits=authority_end-port_at-1;unsigned value=0;
        if(digits<1 || digits>5 || u[port_at+1]=='0')return zn_net_error("http_request url port must be 1 to 65535",NULL,0);
        for(size_t i=port_at+1;i<authority_end;i++){if(!zn_net_digit(u[i]))return zn_net_error("http_request url port must be 1 to 65535",NULL,0);value=value*10u+(unsigned)(u[i]-'0');}
        if(value>65535u)return zn_net_error("http_request url port must be 1 to 65535",NULL,0);
        port=value;
    }
    size_t path=authority_end,path_end=path;while(path_end<un && u[path_end]!='#')path_end++;
    for(size_t i=path;i<path_end;i++)if((unsigned char)u[i]<=0x20u || (unsigned char)u[i]>=0x7Fu)
        return zn_net_error("http_request url path must be printable ASCII without spaces (percent-encode other bytes)",NULL,0);
    DWORD failure=0;
    if(!InitOnceExecuteOnce(&zn_http_once,zn_http_load,&failure,NULL))
        return zn_net_error("http_request cannot load WinHTTP",failure?"Windows":NULL,failure);
    /* A path that starts with '?' (or is empty) is requested from "/". */
    char* target=(char*)zn_alloc(path_end-path+2);size_t target_len=0;
    if(path==path_end || u[path]!='/')target[target_len++]='/';
    memcpy(target+target_len,u+path,path_end-path);target_len+=path_end-path;
    size_t host_from=bracketed?host+1:host,host_to=bracketed?host_end-1:host_end;
    wchar_t* whost=zn_http_wide(u+host_from,host_to-host_from);
    wchar_t* wpath=zn_http_wide(target,target_len);free(target);
    const char* json_or_text="Content-Type: text/plain; charset=utf-8\r\n";
    for(size_t i=0;i<bn;i++){char c=body.data[i];if(c==' ' || c=='\t' || c=='\r' || c=='\n')continue;
        if(c=='{' || c=='[')json_or_text="Content-Type: application/json\r\n";break;}
    wchar_t* headers=bn?zn_http_wide(json_or_text,strlen(json_or_text)):NULL;
    const char* error=NULL;DWORD code=0;char* data=NULL;size_t size=0;DWORD status=0;
    zn_http_handle session=NULL,connection=NULL,request=NULL;
    session=zn_http.open(L"Anode/1.3",ZN_HTTP_ACCESS_TYPE_AUTOMATIC_PROXY,NULL,NULL,0);
    if(!session){error="http_request cannot open a WinHTTP session";code=GetLastError();goto done;}
    connection=zn_http.connect(session,whost,(WORD)port,0);
    if(!connection){error="http_request cannot use this host";code=GetLastError();goto done;}
    request=zn_http.open_request(connection,bn?L"POST":L"GET",wpath,NULL,NULL,NULL,secure?ZN_HTTP_FLAG_SECURE:0u);
    if(!request){error="http_request cannot create the request";code=GetLastError();goto done;}
    zn_http.set_timeouts(request,10000,10000,30000,30000);
    if(!zn_http.send_request(request,headers,headers?(DWORD)-1L:0,bn?(void*)body.data:NULL,(DWORD)bn,(DWORD)bn,0)
        || !zn_http.receive_response(request,NULL)) {
        error="http_request failed to connect, send or receive";code=GetLastError();goto done;
    }
    DWORD status_size=sizeof(status);
    if(!zn_http.query_headers(request,ZN_HTTP_QUERY_STATUS_CODE_NUMBER,NULL,&status,&status_size,NULL)) {
        error="http_request response has no status code";code=GetLastError();goto done;
    }
    size_t cap=4096;data=(char*)zn_alloc(cap+1);
    for(;;) {
        if(size==cap) {
            if(cap>=ZN_NET_LIMIT){error="http_request response exceeds 16 MiB";goto done;}
            cap=cap*2>ZN_NET_LIMIT?ZN_NET_LIMIT:cap*2;
            char* grown=(char*)realloc(data,cap+1);if(!grown)zn_fail("allocation failed");data=grown;
        }
        DWORD got=0;
        if(!zn_http.read_data(request,data+size,(DWORD)(cap-size),&got)){error="http_request failed while reading the response";code=GetLastError();goto done;}
        if(!got)break;
        size+=got;
    }
    if(!zn_net_utf8((const unsigned char*)data,size))error="http_request response must be UTF-8 text without NUL bytes";
done:
    if(request)zn_http.close_handle(request);
    if(connection)zn_http.close_handle(connection);
    if(session)zn_http.close_handle(session);
    free(whost);free(wpath);free(headers);
    if(error){free(data);return zn_net_error(error,code?"WinHTTP":NULL,code);}
    data[size]=0;
    zn_text text={data,size};
    return zn_net_ok((int64_t)status,text);
}
void zn_abi_http_request(void* out,const zn_text* url,const zn_text* body) {
    zn_net_result result=zn_http_request(*url,*body);memcpy(out,&result,sizeof(result));
}
#if defined(_MSC_VER)
#pragma optimize("t", on)
#endif
#undef ZN_COLD_ERRORS
#undef ZN_COLD_ERROR_TEXT
#undef ZN_COLD_ERROR_FIELD

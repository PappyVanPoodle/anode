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

struct zn_task { HANDLE handle; int64_t legacy_result; int64_t (*legacy_work)(void*); void (*work)(void*,void*); void* args; void* result; uint64_t width; };
int64_t zn_cold_add(int64_t a, int64_t b) { return zn_add(a,b); }
int64_t zn_cold_sub(int64_t a, int64_t b) { return zn_sub(a,b); }
int64_t zn_cold_mul(int64_t a, int64_t b) { return zn_mul(a,b); }
int64_t zn_cold_mul_literal(int64_t a, int64_t b) { return zn_mul_literal(a,b); }
int64_t zn_cold_div(int64_t a, int64_t b) { return zn_div(a,b); }
int64_t zn_cold_mod(int64_t a, int64_t b) { return zn_mod(a,b); }
int64_t zn_cold_neg(int64_t a) { return zn_neg(a); }
__declspec(noreturn) void zn_fail(const char* message) { fprintf(stderr, "Zenith runtime error: %s\n", message); fflush(stderr); ExitProcess(1); }
void* zn_alloc(size_t n) { void* p = malloc(n ? n : 1); if (!p) zn_fail("allocation failed"); return p; }
void zn_release(void* p) { free(p); }
void zn_assert(bool condition, int line, const char* message) { if (!condition) { fprintf(stderr, "line %d: ", line); zn_fail(message); } }
zn_text zn_text_make(const char* s) { size_t n = strlen(s); if (n == SIZE_MAX) zn_fail("text too long"); zn_text t = { (char*)zn_alloc(n+1), n }; memcpy(t.data,s,n+1); return t; }
/* Compiler-only constructor for independently bounded C literal segments. */
zn_text zn_text_parts(const char* const* parts,size_t count) {
    size_t size=0;
    for(size_t i=0;i<count;i++){size_t n=strlen(parts[i]);if(n>(size_t)INT64_MAX-size)zn_fail("text too long");size+=n;}
    zn_text out={(char*)zn_alloc(size+1),size};size_t at=0;
    for(size_t i=0;i<count;i++){size_t n=strlen(parts[i]);memcpy(out.data+at,parts[i],n);at+=n;}
    out.data[size]=0;return out;
}
void zn_text_drop(zn_text* t) { free(t->data); t->data = NULL; t->len = 0; }
zn_text zn_text_clone(zn_text t) {
    if(t.len==SIZE_MAX || (t.len && !t.data)) zn_fail("invalid text storage");
    zn_text out={(char*)zn_alloc(t.len+1),t.len};
    if(t.len) memcpy(out.data,t.data,t.len);
    out.data[t.len]=0;return out;
}
zn_text zn_concat(zn_text a, zn_text b) { if (b.len == SIZE_MAX || a.len > SIZE_MAX-b.len-1) zn_fail("text too long"); zn_text t = { (char*)zn_alloc(a.len+b.len+1), a.len+b.len }; memcpy(t.data,a.data,a.len); memcpy(t.data+a.len,b.data,b.len); t.data[t.len] = 0; return t; }
void zn_ints_drop(zn_ints* a) { free(a->data); a->data = NULL; a->len = a->cap = 0; }
void zn_ints_grow(zn_ints* a) {
    if (a->cap > SIZE_MAX / (2*sizeof(int64_t))) zn_fail("array too large");
    size_t cap = a->cap ? a->cap*2 : 4;
    void* p = realloc(a->data, cap*sizeof(int64_t)); if (!p) zn_fail("allocation failed");
    a->data = (int64_t*)p; a->cap = cap;
}
zn_ints zn_ints_clone(zn_ints a) { zn_ints b = {0}; for (size_t i=0;i<a.len;i++) zn_push(&b,a.data[i]); return b; }
/* push transfers ownership. text_at returns an explicit independent copy. */
void zn_texts_push(zn_texts* a, zn_text value) {
    if(a->len == a->cap) {
        if(a->cap > SIZE_MAX/(2*sizeof(zn_text))) zn_fail("array too large");
        size_t cap=a->cap ? a->cap*2 : 4;
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
    for(size_t i=0;i<a.len;i++) zn_texts_push(&out,zn_text_clone(a.data[i]));
    return out;
}
zn_text zn_text_at(zn_texts a,int64_t i) {
    if((uint64_t)i>=a.len) zn_fail("text collection index out of bounds");
    return zn_text_clone(a.data[(size_t)i]);
}
zn_texts zn_split(zn_text value,zn_text separator) {
    if(!separator.len) zn_fail("split separator must not be empty");
    zn_texts out={0}; size_t start=0;
    for(;;) {
        const char* next=strstr(value.data+start,separator.data);
        size_t end=next ? (size_t)(next-value.data) : value.len;
        zn_texts_push(&out,zn_slice(value,(int64_t)start,(int64_t)end));
        if(!next) return out;
        start=end+separator.len;
    }
}
unsigned __stdcall zn_worker_entry(void* data) { zn_task* t=(zn_task*)data; if(t->work)t->work(t->args,t->result);else t->legacy_result=t->legacy_work(t->args);return 0; }
static void zn_task_start(zn_task* t) { t->handle=(HANDLE)_beginthreadex(NULL,0,zn_worker_entry,t,0,NULL);if(!t->handle)zn_fail("thread creation failed"); }
static void zn_task_wait(zn_task* t) { if(!t || WaitForSingleObject(t->handle,INFINITE)!=WAIT_OBJECT_0)zn_fail("worker join failed");if(!CloseHandle(t->handle))zn_fail("worker close failed"); }
zn_task* zn_start(int64_t (*work)(void*),void* args) { zn_task* t=(zn_task*)zn_alloc(sizeof(*t));memset(t,0,sizeof(*t));t->legacy_work=work;t->args=args;zn_task_start(t);return t; }
int64_t zn_join(zn_task* t) { if(!t || t->work)zn_fail("worker result type mismatch");zn_task_wait(t);int64_t result=t->legacy_result;free(t);return result; }
zn_task* zn_start_owned(void (*work)(void*,void*),void* packet,uint64_t width) {
    if(!work || !packet || width>65536)zn_fail("invalid worker packet or result layout");
    zn_task* t=(zn_task*)zn_alloc(sizeof(*t));memset(t,0,sizeof(*t));
    t->work=work;t->args=packet;t->width=width;t->result=zn_alloc((size_t)width);memset(t->result,0,(size_t)width);zn_task_start(t);return t;
}
void zn_join_owned(void* out,zn_task* t,uint64_t width) {
    if(!t || !t->work || t->width!=width || (width && !out))zn_fail("worker result type mismatch");
    zn_task_wait(t);if(width)memcpy(out,t->result,(size_t)width);free(t->result);free(t);
}
void zn_task_drop(zn_task** owner) { zn_task* t=*owner;if(!t)return;*owner=NULL;zn_task_wait(t);if(t->work){t->work(NULL,t->result);free(t->result);}free(t); }
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
void zn_exit_code(int64_t code) { if(code<0 || code>255)zn_fail("exit code must be from 0 to 255");if(GetCurrentThreadId()!=zn_main_thread)zn_fail("exit_code is restricted to the main thread");zn_exit_status=(int)code; }
int zn_process_exit_code(void) { return zn_exit_status; }
int64_t zn_args_count(void) { return zn_argc > 0 ? zn_argc - 1 : 0; }
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
    if (!path.len || path.len > INT_MAX || memchr(path.data, 0, path.len)) return NULL;
    int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path.data, (int)path.len, NULL, 0);
    if (!count) return NULL;
    wchar_t* out = (wchar_t*)zn_alloc(((size_t)count + 1) * sizeof(wchar_t));
    if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path.data, (int)path.len, out, count)) { free(out); return NULL; }
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
    size_t width=a->len<b->len?a->len:b->len;int order=width?memcmp(a->data,b->data,width):0;
    return order?order:(a->len>b->len)-(a->len<b->len);
}
zn_texts zn_list_dir(zn_text path) {
    wchar_t* name=zn_path(path);if(!name)zn_fail("invalid UTF-8 directory path");
    DWORD attributes=GetFileAttributesW(name);
    if(attributes==INVALID_FILE_ATTRIBUTES || !(attributes&FILE_ATTRIBUTE_DIRECTORY)){free(name);zn_fail("cannot open directory");}
    size_t len=wcslen(name);if(len>(SIZE_MAX/sizeof(wchar_t))-3){free(name);zn_fail("directory path too long");}
    wchar_t* pattern=(wchar_t*)zn_alloc((len+3)*sizeof(wchar_t));memcpy(pattern,name,len*sizeof(wchar_t));free(name);
    if(len && pattern[len-1]!=L'\\' && pattern[len-1]!=L'/')pattern[len++]=L'\\';pattern[len++]=L'*';pattern[len]=0;
    WIN32_FIND_DATAW found;HANDLE handle=FindFirstFileW(pattern,&found);DWORD error=GetLastError();free(pattern);zn_texts result={0};
    if(handle==INVALID_HANDLE_VALUE){if(error==ERROR_FILE_NOT_FOUND)return result;zn_fail("cannot enumerate directory");}
    for(;;){
        if(wcscmp(found.cFileName,L".") && wcscmp(found.cFileName,L"..")){
            int bytes=WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,found.cFileName,-1,NULL,0,NULL,NULL);
            if(!bytes){FindClose(handle);zn_texts_drop(&result);zn_fail("directory entry is not valid Unicode");}
            zn_text entry={(char*)zn_alloc((size_t)bytes),(size_t)bytes-1};
            if(!WideCharToMultiByte(CP_UTF8,WC_ERR_INVALID_CHARS,found.cFileName,-1,entry.data,bytes,NULL,NULL)){FindClose(handle);zn_text_drop(&entry);zn_texts_drop(&result);zn_fail("directory Unicode conversion failed");}
            zn_texts_push(&result,entry);
        }
        if(!FindNextFileW(handle,&found)){error=GetLastError();break;}
    }
    if(!FindClose(handle) || error!=ERROR_NO_MORE_FILES){zn_texts_drop(&result);zn_fail("directory enumeration failed");}
    if(result.len>1)qsort(result.data,result.len,sizeof(zn_text),zn_directory_compare);return result;
}
static zn_io_result zn_io_error(const char* message) { zn_io_result r={2,zn_text_make(message)}; return r; }
zn_io_result zn_read_result(zn_text path) {
    wchar_t* name = zn_path(path);
    if(!name) return zn_io_error("invalid UTF-8 file path");
    FILE* file = NULL;
    errno_t error = _wfopen_s(&file, name, L"rb");
    free(name);
    if (error || !file) return zn_io_error("cannot open input file");
    if (_fseeki64(file, 0, SEEK_END)) { fclose(file); return zn_io_error("cannot seek input file"); }
    __int64 size = _ftelli64(file);
    if (size < 0 || size > 64 * 1024 * 1024) { fclose(file); return zn_io_error("input text limit is 64 MiB"); }
    if (_fseeki64(file, 0, SEEK_SET)) { fclose(file); return zn_io_error("cannot seek input file"); }
    zn_text t = { (char*)zn_alloc((size_t)size + 1), (size_t)size };
    size_t got = fread(t.data, 1, t.len, file);
    int failed = ferror(file);
    if (fclose(file)) failed = 1;
    if (failed || got != t.len) { free(t.data); return zn_io_error("input file changed or read failed"); }
    t.data[t.len] = 0;
    if (memchr(t.data, 0, t.len) || (t.len && !MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, t.data, (int)t.len, NULL, 0))) {
        free(t.data); return zn_io_error("input must be UTF-8 text without NUL bytes");
    }
    zn_io_result result={1,t}; return result;
}
zn_io_result zn_write_result(zn_text path, zn_text data) {
    wchar_t* name = zn_path(path);
    if(!name) return zn_io_error("invalid UTF-8 file path");
    FILE* file = NULL;
    errno_t error = _wfopen_s(&file, name, L"wb");
    free(name);
    if (error || !file) return zn_io_error("cannot open output file");
    size_t written = fwrite(data.data, 1, data.len, file);
    int failed = written != data.len;
    if (fclose(file)) failed = 1;
    if (failed) return zn_io_error("cannot write output file");
    zn_io_result result={1,{0}}; return result;
}
zn_text zn_read_text(zn_text path) { zn_io_result r=zn_read_result(path); if(r.tag==2) zn_fail(r.value.data); return r.value; }
void zn_write_text(zn_text path,zn_text text) { zn_io_result r=zn_write_result(path,text); if(r.tag==2) zn_fail(r.value.data); }
/* Use memcpy across the compiler's equivalent tagged-union layout: no C
   aliasing assumption about the anonymous generated data types is needed. */
void zn_abi_try_read_text(void* out,const zn_text* path) {zn_io_result r=zn_read_result(*path); memcpy(out,&r,sizeof(r));}
void zn_abi_try_write_text(void* out,const zn_text* path,const zn_text* text) {zn_io_result r=zn_write_result(*path,*text); memcpy(out,&r,sizeof(r));}
bool zn_text_eq(zn_text a, zn_text b) { return a.len == b.len && !memcmp(a.data, b.data, a.len); }
bool zn_contains(zn_text a, zn_text b) { return strstr(a.data, b.data) != NULL; }
zn_text zn_slice(zn_text t, int64_t start, int64_t end) {
    if (start < 0 || end < start || (uint64_t)end > t.len) zn_fail("text slice out of bounds");
    if (((size_t)start < t.len && ((unsigned char)t.data[start] & 0xc0) == 0x80)
        || ((size_t)end < t.len && ((unsigned char)t.data[end] & 0xc0) == 0x80)) zn_fail("text slice splits a UTF-8 character");
    size_t count = (size_t)(end-start);
    zn_text out = { (char*)zn_alloc(count + 1), count };
    memcpy(out.data, t.data + start, count); out.data[count] = 0;
    return out;
}
/* Text offsets are UTF-8 bytes. Character traversal operates on scalar values,
   not grapheme clusters, and never yields a pointer into owned storage. */
bool zn_is_char_boundary(zn_text t,int64_t at) {
    return at>=0 && (uint64_t)at<=t.len
        && ((uint64_t)at==t.len || ((unsigned char)t.data[(size_t)at]&0xc0)!=0x80);
}
static size_t zn_utf8_step(zn_text t,size_t at,uint32_t* out) {
    if(at>=t.len)zn_fail("character offset out of bounds");
    unsigned char first=(unsigned char)t.data[at];
    size_t width;uint32_t cp,min;
    if(first>0 && first<0x80){width=1;cp=first;min=1;}
    else if(first>=0xc2 && first<=0xdf){width=2;cp=first&0x1f;min=0x80;}
    else if(first>=0xe0 && first<=0xef){width=3;cp=first&0x0f;min=0x800;}
    else if(first>=0xf0 && first<=0xf4){width=4;cp=first&7;min=0x10000;}
    else {zn_fail("invalid UTF-8 character boundary");}
    if(width>t.len-at)zn_fail("incomplete UTF-8 character");
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
    while(i<t.len){i=zn_utf8_step(t,i,&cp);n++;}
    return zn_length(n);
}
int64_t zn_codepoint_at(zn_text t,int64_t at) {
    if(at<0)zn_fail("character offset out of bounds");
    uint32_t cp;zn_utf8_step(t,(size_t)at,&cp);return cp;
}
int64_t zn_next_char_boundary(zn_text t,int64_t at) {
    if(!zn_is_char_boundary(t,at))zn_fail("invalid UTF-8 character boundary");
    if((uint64_t)at==t.len)return at;
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
    if(!needle.len)return start;
    if(needle.len>t.len-start)return SIZE_MAX;
    size_t limit=t.len-needle.len;
    while(start<=limit){
        const char* p=(const char*)memchr(t.data+start,(unsigned char)needle.data[0],limit-start+1);
        if(!p)return SIZE_MAX;
        size_t at=(size_t)(p-t.data);
        if(!memcmp(p,needle.data,needle.len))return at;
        start=at+1;
    }
    return SIZE_MAX;
}
int64_t zn_text_find(zn_text t,zn_text needle,int64_t start) {
    if(!zn_is_char_boundary(t,start))zn_fail("search start must be a UTF-8 boundary in range");
    size_t at=zn_find_bytes(t,needle,(size_t)start);return at==SIZE_MAX?-1:zn_length(at);
}
bool zn_text_starts_with(zn_text t,zn_text prefix) {
    return prefix.len<=t.len && !memcmp(t.data,prefix.data,prefix.len);
}
bool zn_text_ends_with(zn_text t,zn_text suffix) {
    return suffix.len<=t.len && !memcmp(t.data+t.len-suffix.len,suffix.data,suffix.len);
}
/* Fixed White_Space set; no locale-dependent classification or case folding. */
static bool zn_unicode_space(uint32_t c) {
    return (c>=9 && c<=13) || c==32 || c==0x85 || c==0xa0 || c==0x1680
        || (c>=0x2000 && c<=0x200a) || c==0x2028 || c==0x2029
        || c==0x202f || c==0x205f || c==0x3000;
}
zn_text zn_text_trim(zn_text t) {
    size_t at=0,start=t.len,end=t.len;bool found=false;
    while(at<t.len){
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
    zn_length(t.len);size_t count=0,start=0,match;
    if(!needle.len){count=(size_t)zn_char_count(t)+1;}
    else while((match=zn_find_bytes(t,needle,start))!=SIZE_MAX){count++;start=match+needle.len;}
    size_t size=zn_replaced_size(t.len,count,needle.len,replacement.len);
    zn_text out={(char*)zn_alloc(size+1),size};size_t written=0;start=0;
    if(!needle.len){
        for(;;){
            memcpy(out.data+written,replacement.data,replacement.len);written+=replacement.len;
            if(start==t.len)break;
            uint32_t cp;size_t end=zn_utf8_step(t,start,&cp);
            memcpy(out.data+written,t.data+start,end-start);written+=end-start;start=end;
        }
    }else{
        while((match=zn_find_bytes(t,needle,start))!=SIZE_MAX){
            memcpy(out.data+written,t.data+start,match-start);written+=match-start;
            memcpy(out.data+written,replacement.data,replacement.len);written+=replacement.len;
            start=match+needle.len;
        }
        memcpy(out.data+written,t.data+start,t.len-start);written+=t.len-start;
    }
    if(written!=size)zn_fail("replacement length invariant failed");
    out.data[size]=0;return out;
}
zn_texts zn_text_lines(zn_text t) {
    zn_texts out={0};size_t start=0;
    for(size_t i=0;i<t.len;i++)if(t.data[i]=='\n'){
        size_t end=i;if(end>start && t.data[end-1]=='\r')end--;
        zn_texts_push(&out,zn_slice(t,zn_length(start),zn_length(end)));start=i+1;
    }
    if(start<t.len)zn_texts_push(&out,zn_slice(t,zn_length(start),zn_length(t.len)));
    return out;
}
zn_text zn_text_join(zn_texts parts,zn_text separator) {
    size_t size=0;
    for(size_t i=0;i<parts.len;i++){
        if(parts.data[i].len>(size_t)INT64_MAX-size)zn_fail("joined text too long");
        size+=parts.data[i].len;
        if(i){if(separator.len>(size_t)INT64_MAX-size)zn_fail("joined text too long");size+=separator.len;}
    }
    zn_text out={(char*)zn_alloc(size+1),size};size_t at=0;
    for(size_t i=0;i<parts.len;i++){
        if(i){memcpy(out.data+at,separator.data,separator.len);at+=separator.len;}
        memcpy(out.data+at,parts.data[i].data,parts.data[i].len);at+=parts.data[i].len;
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
    if (!t.len) return false;
    bool negative = t.data[0] == '-';
    size_t at = negative || t.data[0] == '+' ? 1 : 0;
    if (at == t.len) return false;
    uint64_t value = 0, limit = (uint64_t)INT64_MAX + (negative ? 1 : 0);
    for (; at < t.len; at++) {
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
zn_text zn_int_text(int64_t value) {
    char buffer[32];
    int count = snprintf(buffer, sizeof(buffer), "%" PRId64, value);
    if (count < 0 || count >= sizeof(buffer)) zn_fail("integer formatting failed");
    return zn_text_make(buffer);
}

/* Explicit pointer ABI for direct native backends. Ownership checks stay in
   the shared frontend; all dynamic bounds/UTF-8/arithmetic checks stay here. */
zn_text zn_text_view_make(zn_text source,int64_t start,int64_t end){
    if(start<0 || end<start || (uint64_t)end>source.len) zn_fail("text region out of bounds");
    if(!zn_is_char_boundary(source,start) || !zn_is_char_boundary(source,end)) zn_fail("text region splits UTF-8 code point");
    zn_text result={source.data ? source.data+(size_t)start : NULL,(size_t)(end-start)};
    return result;
}
void zn_text_view_drop(zn_text* t){(void)t;}
zn_text zn_to_text(zn_text t){return zn_slice(t,0,zn_length(t.len));}
bool zn_view_contains(zn_text a,zn_text b){return zn_text_find(a,b,0)>=0;}
void zn_print_text_view(zn_text t){if(t.len)fwrite(t.data,1,t.len,stdout);fputc('\n',stdout);}
void zn_abi_text_region(zn_text* out,const zn_text* source,int64_t start,int64_t end){*out=zn_text_view_make(*source,start,end);}
void zn_abi_to_text(zn_text* out,const zn_text* t){*out=zn_to_text(*t);}
bool zn_abi_view_contains(const zn_text* a,const zn_text* b){return zn_view_contains(*a,*b);}
void zn_abi_print_text_view(const zn_text* t){zn_print_text_view(*t);}
void zn_abi_text_make(zn_text* out,const char* s){*out=zn_text_make(s);}
void zn_abi_text_clone(zn_text* out,const zn_text* in){*out=zn_text_clone(*in);}
void zn_abi_ints_clone(zn_ints* out,const zn_ints* in){*out=zn_ints_clone(*in);}
void zn_abi_texts_clone(zn_texts* out,const zn_texts* in){*out=zn_texts_clone(*in);}
void zn_abi_texts_push(zn_texts* a,const zn_text* value){zn_texts_push(a,*value);}
void zn_abi_text_at(zn_text* out,const zn_texts* a,int64_t i){*out=zn_text_at(*a,i);}
void zn_abi_split(zn_texts* out,const zn_text* value,const zn_text* separator){*out=zn_split(*value,*separator);}
void zn_abi_concat(zn_text* out,const zn_text* a,const zn_text* b){*out=zn_concat(*a,*b);}
void zn_abi_arg(zn_text* out,int64_t i){*out=zn_arg(i);}
void zn_abi_read_text(zn_text* out,const zn_text* p){*out=zn_read_text(*p);}
bool zn_abi_file_exists(const zn_text* p){return zn_file_exists(*p);}
void zn_abi_list_dir(zn_texts* out,const zn_text* p){*out=zn_list_dir(*p);}
void zn_abi_write_text(const zn_text* p,const zn_text* t){zn_write_text(*p,*t);}
bool zn_abi_text_eq(const zn_text* a,const zn_text* b){return zn_text_eq(*a,*b);}
bool zn_abi_contains(const zn_text* a,const zn_text* b){return zn_contains(*a,*b);}
void zn_abi_slice(zn_text* out,const zn_text* t,int64_t a,int64_t b){*out=zn_slice(*t,a,b);}
int64_t zn_abi_byte_at(const zn_text* t,int64_t i){return zn_byte_at(*t,i);}
int64_t zn_abi_parse_int(const zn_text* t){return zn_parse_int(*t);}
bool zn_abi_is_int(const zn_text* t){return zn_is_int(*t);}
void zn_abi_int_text(zn_text* out,int64_t n){*out=zn_int_text(n);}
void zn_abi_print_text(const zn_text* t){zn_print_text(*t);}
int64_t zn_abi_length(const zn_text* t){return zn_length(t->len);}
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
    if(i<t.len && (t.data[i]=='+'||t.data[i]=='-'))i++;
    while(i<t.len && t.data[i]>='0'&&t.data[i]<='9'){i++;digits++;}
    if(i<t.len && t.data[i]=='.'){
        i++;while(i<t.len && t.data[i]>='0'&&t.data[i]<='9'){i++;digits++;}
    }
    if(!digits)return false;
    if(i<t.len && (t.data[i]=='e'||t.data[i]=='E')){
        i++;if(i<t.len && (t.data[i]=='+'||t.data[i]=='-'))i++;
        size_t start=i;while(i<t.len && t.data[i]>='0'&&t.data[i]<='9')i++;
        if(start==i)return false;
    }
    if(i!=t.len)return false;
    char* end;double v=strtod(t.data,&end);
    if(end!=t.data+t.len || !isfinite(v))return false;
    *out=v;return true;
}
bool zn_is_f64(zn_text t){double v;return zn_try_f64(t,&v);}
double zn_parse_f64(zn_text t){double v;if(!zn_try_f64(t,&v))zn_fail("invalid or overflowing float text");return v;}
uint64_t zn_abi_parse_f64(const zn_text* t){return zn_f64_bits(zn_parse_f64(*t));}
bool zn_abi_is_f64(const zn_text* t){return zn_is_f64(*t);}
void zn_abi_float_text(zn_text* out,uint64_t raw,uint64_t code){*out=zn_float_text(raw,code);}

/* Typed vectors: the compiler supplies checked element layouts and drop/clone glue. */
static size_t zn_vec_bytes(size_t count, uint64_t width) {
    if (!width || count > INT64_MAX || count > SIZE_MAX / width) zn_fail("vector capacity overflow");
    return count * (size_t)width;
}
void* zn_vec_push_slot(zn_vec* a, uint64_t width) {
    if (a->len == a->cap) {
        size_t cap = a->cap ? a->cap : 4;
        if (a->cap) { if (cap > INT64_MAX / 2) zn_fail("vector capacity overflow"); cap *= 2; }
        void* data = zn_alloc(zn_vec_bytes(cap, width));
        if (a->len) memcpy(data, a->data, zn_vec_bytes(a->len, width));
        zn_release(a->data); a->data=data; a->cap=cap;
    }
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

/* Nominal maps use deterministic linear probing. Storage moves are raw owner
   transfers; only generated typed code destroys or clones entry contents. */
static size_t zn_map_bytes(uint64_t count,uint64_t width) {
    if(!width || width>(uint64_t)PTRDIFF_MAX || count>(uint64_t)INT64_MAX ||
       count>(uint64_t)PTRDIFF_MAX/width || count>SIZE_MAX/width)
        zn_fail("map capacity overflow");
    return (size_t)count*(size_t)width;
}
static void zn_map_check(const zn_map* m,uint64_t width) {
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
static size_t zn_map_key_width(uint64_t code,uint64_t width) {
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
static zn_text zn_map_text_key(const void* key) {
    zn_text text;
    if(!key) zn_fail("invalid map key");
    memcpy(&text,key,sizeof(text));
    if(text.len>(size_t)PTRDIFF_MAX || (text.len && !text.data)) zn_fail("invalid map text key");
    return text;
}
static uint64_t zn_map_mix(uint64_t hash) {
    hash^=hash>>30;hash*=UINT64_C(0xbf58476d1ce4e5b9);
    hash^=hash>>27;hash*=UINT64_C(0x94d049bb133111eb);
    return hash^(hash>>31);
}
static uint64_t zn_map_hash(const void* key,uint64_t code,uint64_t width) {
    size_t size=zn_map_key_width(code,width);
    if(!key) zn_fail("invalid map key");
    if(code) {
        /* Narrow keys need neither alignment nor canonical register extension.
           In particular an unsigned 64-bit key retains every high bit. */
        uint64_t raw=0;memcpy(&raw,key,size);
        return zn_map_mix(raw^UINT64_C(0x9e3779b97f4a7c15));
    }
    zn_text text=zn_map_text_key(key);
    uint64_t hash=UINT64_C(14695981039346656037);
    for(size_t i=0;i<sizeof(uint64_t);i++) {
        hash^=((uint64_t)text.len>>(i*8))&255;hash*=UINT64_C(1099511628211);
    }
    for(size_t i=0;i<text.len;i++) {
        hash^=(unsigned char)text.data[i];hash*=UINT64_C(1099511628211);
    }
    return zn_map_mix(hash);
}
static bool zn_map_equal(const void* entry,const void* key,uint64_t code,uint64_t width) {
    size_t size=zn_map_key_width(code,width);
    if(code) return memcmp(entry,key,size)==0;
    zn_text a=zn_map_text_key(entry),b=zn_map_text_key(key);
    return a.len==b.len && (!a.len || memcmp(a.data,b.data,a.len)==0);
}
static unsigned char zn_map_state(const zn_map* m,uint64_t index) {
    unsigned char state=m->states[(size_t)index];
    if(state>2) zn_fail("invalid map slot state");
    return state;
}
static void* zn_map_entry(const zn_map* m,uint64_t index,uint64_t width) {
    return (unsigned char*)m->data+zn_map_bytes(index,width);
}
void* zn_map_find(const zn_map* m,const void* key,uint64_t code,uint64_t width) {
    zn_map_check(m,width);
    uint64_t hash=zn_map_hash(key,code,width);
    if(!m->cap) return NULL;
    uint64_t index=hash&(m->cap-1);
    for(uint64_t probes=0;probes<m->cap;probes++) {
        unsigned char state=zn_map_state(m,index);
        if(!state) return NULL;
        if(state==1) {
            void* entry=zn_map_entry(m,index,width);
            if(zn_map_equal(entry,key,code,width)) return entry;
        }
        index=(index+1)&(m->cap-1);
    }
    return NULL;
}
static void zn_map_rehash(zn_map* m,uint64_t cap,uint64_t code,uint64_t width) {
    zn_map out={0};size_t bytes=zn_map_bytes(cap,width);
    if(cap<8 || (cap&(cap-1))) zn_fail("invalid map capacity");
    out.data=zn_alloc(bytes);memset(out.data,0,bytes);
    out.states=(unsigned char*)zn_alloc((size_t)cap);memset(out.states,0,(size_t)cap);out.cap=cap;
    for(uint64_t old=0;old<m->cap;old++) if(zn_map_state(m,old)==1) {
        void* entry=zn_map_entry(m,old,width);
        uint64_t index=zn_map_hash(entry,code,width)&(cap-1),probes=0;
        while(out.states[(size_t)index]) {
            if(++probes==cap) zn_fail("map probing overflow");
            index=(index+1)&(cap-1);
        }
        memcpy(zn_map_entry(&out,index,width),entry,(size_t)width);
        out.states[(size_t)index]=1;out.len++;
    }
    if(out.len!=m->len) zn_fail("invalid map live count");
    zn_release(m->data);zn_release(m->states);*m=out;
}
void* zn_map_insert_slot(zn_map* m,const void* key,uint64_t code,uint64_t width,bool* replaced) {
    if(!replaced) zn_fail("invalid map replacement flag");
    void* existing=zn_map_find(m,key,code,width);
    *replaced=existing!=NULL;
    if(existing) return existing;
    if(!m->cap) zn_map_rehash(m,8,code,width);
    else if(m->len+m->tombstones>=m->cap-m->cap/4) {
        uint64_t cap=m->cap;
        if(m->len>=cap/2) {
            if(cap>(uint64_t)INT64_MAX/2) zn_fail("map capacity overflow");
            cap*=2;
        }
        zn_map_rehash(m,cap,code,width);
    }
    uint64_t index=zn_map_hash(key,code,width)&(m->cap-1),tomb=m->cap;
    for(uint64_t probes=0;probes<m->cap;probes++) {
        unsigned char state=zn_map_state(m,index);
        if(state==2 && tomb==m->cap) tomb=index;
        if(!state) {
            if(tomb!=m->cap) {index=tomb;m->tombstones--;}
            m->states[(size_t)index]=1;m->len++;
            return zn_map_entry(m,index,width);
        }
        index=(index+1)&(m->cap-1);
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
    memset(slot,0,(size_t)width);m->states[(size_t)index]=2;m->len--;m->tombstones++;
}
void zn_map_clone_storage(zn_map* out,const zn_map* source,uint64_t width) {
    zn_map_check(source,width);
    if(!out || out==source) zn_fail("invalid map clone destination");
    *out=(zn_map){0};
    if(!source->cap) return;
    size_t bytes=zn_map_bytes(source->cap,width);
    out->data=zn_alloc(bytes);out->states=(unsigned char*)zn_alloc((size_t)source->cap);
    memcpy(out->data,source->data,bytes);memcpy(out->states,source->states,(size_t)source->cap);
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

/* v1.2 bootstrap subprocess capture for `process_run` (Stage-3 --build-exe).
   Included only by generated C that calls process_run (src/cgen.an emits
   `#include "bprocess.h"` as a first-use helper), so programs without
   process_run keep their exact 1.1 C and include no Windows header here.
   Semantics follow LANGUAGE.md "Direct subprocess capture": CreateProcessW
   directly (no shell) with explicit UTF-16 argument quoting, an explicit
   inherited-handle list (NUL stdin, two capture pipes), stdout/stderr drained
   concurrently, each capped at 16 MiB, NUL/invalid UTF-8 rejected, nonzero
   exits returned as successful captures with the exact unsigned 32-bit status.
   Launch, completion and capture share one deadline; on timeout or capture
   failure the owned job object is terminated. The bootstrap deadline is
   600,000 ms (the main runtime uses 30,000 ms) because the Stage-3 build runs
   MSVC /O2 over the whole compiler; see README-v120.md. */
#ifndef ANODE_BOOTSTRAP_PROCESS_H
#define ANODE_BOOTSTRAP_PROCESS_H
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <limits.h>
#define ZN_V120_PROCESS_DEADLINE_MS 600000u
#define ZN_V120_CAPTURE_LIMIT 16777216u
typedef struct { HANDLE pipe; char* data; size_t len,cap; int overflow,failed; } zn_v120_capture;
static DWORD WINAPI zn_v120_drain(LPVOID raw) {
    zn_v120_capture* c=(zn_v120_capture*)raw; char buffer[65536];
    for(;;){
        DWORD got=0;
        if(!ReadFile(c->pipe,buffer,(DWORD)sizeof buffer,&got,NULL)){if(GetLastError()!=ERROR_BROKEN_PIPE)c->failed=1;break;}
        if(!got)break;
        if(c->overflow)continue;
        if(c->len+got>ZN_V120_CAPTURE_LIMIT){c->overflow=1;continue;}
        if(c->len+got>c->cap){size_t cap=c->cap?c->cap:65536;while(cap<c->len+got)cap*=2;char* grown=(char*)realloc(c->data,cap);if(!grown){c->failed=1;c->overflow=1;continue;}c->data=grown;c->cap=cap;}
        memcpy(c->data+c->len,buffer,got);c->len+=got;
    }
    return 0;
}
static int zn_v120_valid_text(const char* s,size_t n) {
    size_t i=0;
    while(i<n){
        unsigned char c=(unsigned char)s[i];
        if(c==0)return 0;
        if(c<0x80){++i;continue;}
        size_t extra;uint32_t cp;
        if(c>=0xC2 && c<=0xDF){extra=1;cp=c&0x1Fu;}
        else if(c>=0xE0 && c<=0xEF){extra=2;cp=c&0x0Fu;}
        else if(c>=0xF0 && c<=0xF4){extra=3;cp=c&0x07u;}
        else return 0;
        if(n-i<=extra)return 0;
        for(size_t k=1;k<=extra;++k){unsigned char d=(unsigned char)s[i+k];if((d&0xC0u)!=0x80u)return 0;cp=(cp<<6)|(d&0x3Fu);}
        if((extra==2 && (cp<0x800u || (cp>=0xD800u && cp<=0xDFFFu))) || (extra==3 && (cp<0x10000u || cp>0x10FFFFu)))return 0;
        i+=extra+1;
    }
    return 1;
}
static wchar_t* zn_v120_wide(zn_text t) {
    if(t.len>(size_t)INT_MAX-1 || memchr(t.data,0,t.len) || !zn_v120_valid_text(t.data,t.len))return NULL;
    int need=t.len?MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,t.data,(int)t.len,NULL,0):0;
    if(t.len && need<=0)return NULL;
    wchar_t* out=(wchar_t*)calloc((size_t)need+1,sizeof(wchar_t));
    if(!out)zn_fail("out of memory");
    if(t.len && MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,t.data,(int)t.len,out,need)!=need){free(out);return NULL;}
    return out;
}
/* MSVC/CommandLineToArgvW quoting: 2n backslashes + quote -> n + literal quote. */
static void zn_v120_quote(wchar_t** line,size_t* len,size_t* cap,const wchar_t* arg) {
    size_t n=wcslen(arg),need=*len+3*n+4;
    if(need>*cap){size_t cap2=*cap?*cap:256;while(cap2<need)cap2*=2;wchar_t* grown=(wchar_t*)realloc(*line,cap2*sizeof(wchar_t));if(!grown)zn_fail("out of memory");*line=grown;*cap=cap2;}
    wchar_t* out=*line;size_t at=*len;
    if(at)out[at++]=L' ';
    int plain=n>0;for(size_t i=0;i<n;++i)if(arg[i]==L' '||arg[i]==L'\t'||arg[i]==L'\n'||arg[i]==L'\v'||arg[i]==L'"'){plain=0;break;}
    if(plain){memcpy(out+at,arg,n*sizeof(wchar_t));at+=n;}
    else{
        out[at++]=L'"';size_t slashes=0;
        for(size_t i=0;i<n;++i){
            if(arg[i]==L'\\'){++slashes;continue;}
            if(arg[i]==L'"'){for(size_t k=0;k<2*slashes+1;++k)out[at++]=L'\\';out[at++]=L'"';}
            else{for(size_t k=0;k<slashes;++k)out[at++]=L'\\';out[at++]=arg[i];}
            slashes=0;
        }
        for(size_t k=0;k<2*slashes;++k)out[at++]=L'\\';
        out[at++]=L'"';
    }
    out[at]=0;*len=at;
}
static B zn_v120_process_error(const char* message) { B text=b_literal(message);return b_enum(1,&text); }
static B zn_v120_process_text(zn_v120_capture* c) {
    zn_text t;t.len=c->len;t.data=(char*)zn_alloc(c->len+1);if(c->len)memcpy(t.data,c->data,c->len);t.data[c->len]=0;return b_text(t);
}
static B zn_v120_process_run(B* program,B* args) {
    program=b_resolve(program);args=b_resolve(args);
    if(!program||program->kind!=BT||!args||args->kind!=BSEQ)zn_fail("process_run requires text program and borrowed texts");
    wchar_t* exe=zn_v120_wide(program->t);
    if(!exe||!program->t.len){free(exe);return zn_v120_process_error("process_run program is not valid nonempty UTF-8 text");}
    wchar_t* line=NULL;size_t len=0,cap=0;zn_v120_quote(&line,&len,&cap,exe);
    for(size_t i=0;i<args->o->len;++i){
        B* item=b_resolve(&args->o->data[i]);
        if(!item||item->kind!=BT){free(exe);free(line);zn_fail("process_run argv requires texts");}
        wchar_t* wide=zn_v120_wide(item->t);
        if(!wide){free(exe);free(line);return zn_v120_process_error("process_run argument is not valid UTF-8 text without NUL");}
        zn_v120_quote(&line,&len,&cap,wide);free(wide);
    }
    SECURITY_ATTRIBUTES inherit={sizeof(SECURITY_ATTRIBUTES),NULL,TRUE};
    HANDLE out_read=NULL,out_write=NULL,err_read=NULL,err_write=NULL,input=INVALID_HANDLE_VALUE,job=NULL;
    B result=b_unit();int launched=0;
    PROCESS_INFORMATION pi;memset(&pi,0,sizeof pi);
    LPPROC_THREAD_ATTRIBUTE_LIST attributes=NULL;
    zn_v120_capture out_capture,err_capture;memset(&out_capture,0,sizeof out_capture);memset(&err_capture,0,sizeof err_capture);
    HANDLE threads[2]={NULL,NULL};
    if(!CreatePipe(&out_read,&out_write,&inherit,0)||!CreatePipe(&err_read,&err_write,&inherit,0)){result=zn_v120_process_error("process_run could not create capture pipes");goto done;}
    SetHandleInformation(out_read,HANDLE_FLAG_INHERIT,0);SetHandleInformation(err_read,HANDLE_FLAG_INHERIT,0);
    input=CreateFileW(L"NUL",GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE,&inherit,OPEN_EXISTING,0,NULL);
    if(input==INVALID_HANDLE_VALUE){result=zn_v120_process_error("process_run could not open NUL input");goto done;}
    SIZE_T size=0;InitializeProcThreadAttributeList(NULL,1,0,&size);
    attributes=(LPPROC_THREAD_ATTRIBUTE_LIST)calloc(1,size);
    if(!attributes||!InitializeProcThreadAttributeList(attributes,1,0,&size)){free(attributes);attributes=NULL;result=zn_v120_process_error("process_run could not restrict inherited handles");goto done;}
    HANDLE inherited[3]={input,out_write,err_write};
    if(!UpdateProcThreadAttribute(attributes,0,PROC_THREAD_ATTRIBUTE_HANDLE_LIST,inherited,sizeof inherited,NULL,NULL)){result=zn_v120_process_error("process_run could not restrict inherited handles");goto done;}
    job=CreateJobObjectW(NULL,NULL);
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limit;memset(&limit,0,sizeof limit);limit.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if(!job||!SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limit,sizeof limit)){result=zn_v120_process_error("process_run could not create its job");goto done;}
    STARTUPINFOEXW startup;memset(&startup,0,sizeof startup);startup.StartupInfo.cb=sizeof startup;
    startup.StartupInfo.dwFlags=STARTF_USESTDHANDLES;startup.StartupInfo.hStdInput=input;startup.StartupInfo.hStdOutput=out_write;startup.StartupInfo.hStdError=err_write;startup.lpAttributeList=attributes;
    DWORD started=GetTickCount();
    if(!CreateProcessW(exe,line,NULL,NULL,TRUE,CREATE_SUSPENDED|CREATE_NO_WINDOW|EXTENDED_STARTUPINFO_PRESENT|CREATE_UNICODE_ENVIRONMENT,NULL,NULL,&startup.StartupInfo,&pi)){result=zn_v120_process_error("process_run could not launch the program");goto done;}
    launched=1;
    if(!AssignProcessToJobObject(job,pi.hProcess)){TerminateProcess(pi.hProcess,1);result=zn_v120_process_error("process_run could not assign its job");goto done;}
    CloseHandle(out_write);out_write=NULL;CloseHandle(err_write);err_write=NULL;CloseHandle(input);input=INVALID_HANDLE_VALUE;
    out_capture.pipe=out_read;err_capture.pipe=err_read;
    threads[0]=CreateThread(NULL,0,zn_v120_drain,&out_capture,0,NULL);threads[1]=CreateThread(NULL,0,zn_v120_drain,&err_capture,0,NULL);
    if(!threads[0]||!threads[1]){TerminateJobObject(job,1);result=zn_v120_process_error("process_run could not start capture threads");goto done;}
    ResumeThread(pi.hThread);
    if(WaitForSingleObject(pi.hProcess,ZN_V120_PROCESS_DEADLINE_MS)!=WAIT_OBJECT_0){TerminateJobObject(job,1);result=zn_v120_process_error("process_run timed out");goto done;}
    DWORD used=GetTickCount()-started,left=used<ZN_V120_PROCESS_DEADLINE_MS?ZN_V120_PROCESS_DEADLINE_MS-used:0;
    if(WaitForMultipleObjects(2,threads,TRUE,left)!=WAIT_OBJECT_0){TerminateJobObject(job,1);result=zn_v120_process_error("process_run capture did not finish before the deadline");goto done;}
    DWORD status=0;
    if(!GetExitCodeProcess(pi.hProcess,&status)){result=zn_v120_process_error("process_run could not read the exit status");goto done;}
    if(out_capture.failed||err_capture.failed||out_capture.overflow||err_capture.overflow){result=zn_v120_process_error("process_run capture failed or exceeded 16 MiB");goto done;}
    if(!zn_v120_valid_text(out_capture.data,out_capture.len)||!zn_v120_valid_text(err_capture.data,err_capture.len)){result=zn_v120_process_error("process_run captured output is not valid UTF-8 text without NUL");goto done;}
    {B record=b_seq(BREC,3);record.o->data[0]=b_int((int64_t)status);record.o->data[1]=zn_v120_process_text(&out_capture);record.o->data[2]=zn_v120_process_text(&err_capture);result=b_enum(0,&record);}
done:
    if(threads[0]||threads[1]){
        if(launched){if(out_write){CloseHandle(out_write);out_write=NULL;}if(err_write){CloseHandle(err_write);err_write=NULL;}}
        if(threads[0]){WaitForSingleObject(threads[0],INFINITE);CloseHandle(threads[0]);}
        if(threads[1]){WaitForSingleObject(threads[1],INFINITE);CloseHandle(threads[1]);}
    }
    if(pi.hThread)CloseHandle(pi.hThread);if(pi.hProcess)CloseHandle(pi.hProcess);
    if(job)CloseHandle(job);
    if(attributes){DeleteProcThreadAttributeList(attributes);free(attributes);}
    if(input!=INVALID_HANDLE_VALUE)CloseHandle(input);
    if(out_write)CloseHandle(out_write);if(err_write)CloseHandle(err_write);if(out_read)CloseHandle(out_read);if(err_read)CloseHandle(err_read);
    free(out_capture.data);free(err_capture.data);free(exe);free(line);
    return result;
}
#endif

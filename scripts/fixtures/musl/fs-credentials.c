#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static _Noreturn void fail(int step)
{
    dprintf(2,"fs credentials fail step=%d errno=%d\n",step,errno); _exit(step);
}
static int child_ok(pid_t child)
{
    int status=-1;
    if(child>0 && waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status)) return 1;
    dprintf(2,"fs credentials child=%ld status=%d\n",(long)child,status); return 0;
}
static long fsuid(unsigned id) { return syscall(SYS_setfsuid,id); }
static long fsgid(unsigned id) { return syscall(SYS_setfsgid,id); }
static int dumpable(void) { return prctl(PR_GET_DUMPABLE,0,0,0,0); }
static int pdeath(void)
{
    int value=-1;
    return prctl(PR_GET_PDEATHSIG,&value,0,0,0) ? -1 : value;
}
static int copy_file(const char *source,const char *destination,int mode)
{
    int in=open(source,O_RDONLY),out=open(destination,O_WRONLY|O_CREAT|O_TRUNC,mode);
    if(in<0 || out<0) return 0;
    char bytes[4096]; ssize_t count;
    while((count=read(in,bytes,sizeof bytes))>0) if(write(out,bytes,count)!=count) return 0;
    int okay=count==0 && !fchmod(out,mode); close(in); close(out); return okay;
}
static int proc_ids(const char *name,unsigned uid,unsigned gid)
{
    FILE *file=fopen(name,"r"); if(!file) return 0;
    char line[256]; unsigned real,effective,saved,filesystem; int seen=0;
    while(fgets(line,sizeof line,file)) {
        if(sscanf(line,"Uid:\t%u\t%u\t%u\t%u",&real,&effective,&saved,&filesystem)==4) {
            if(filesystem!=uid) { fclose(file); return 0; } seen|=1;
        }
        if(sscanf(line,"Gid:\t%u\t%u\t%u\t%u",&real,&effective,&saved,&filesystem)==4) {
            if(filesystem!=gid) { fclose(file); return 0; } seen|=2;
        }
    }
    fclose(file); return seen==3;
}
struct thread_state { atomic_int phase,tid; };
static void *identity_worker(void *argument)
{
    struct thread_state *state=argument;
    if(prctl(PR_SET_PDEATHSIG,SIGUSR1,0,0,0) || fsuid(1234)!=0 || fsgid(1235)!=0
        || geteuid()!=0 || getegid()!=0 || dumpable()!=0 || pdeath()!=0) fail(185);
    atomic_store(&state->tid,syscall(SYS_gettid));
    atomic_store(&state->phase,1);
    while(atomic_load(&state->phase)==1) sched_yield();
    if(dumpable()!=1 || fsuid(-1U)!=1234 || fsgid(-1U)!=1235) fail(186);
    return 0;
}
static void thread_identity(void)
{
    struct thread_state state={0}; pthread_t worker;
    if(prctl(PR_SET_DUMPABLE,1,0,0,0) || prctl(PR_SET_PDEATHSIG,SIGUSR2,0,0,0)
        || pthread_create(&worker,0,identity_worker,&state)) fail(187);
    while(!atomic_load(&state.phase)) sched_yield();
    char path[128]; snprintf(path,sizeof path,"/proc/self/task/%d/status",atomic_load(&state.tid));
    if(fsuid(-1U)!=0 || fsgid(-1U)!=0 || dumpable()!=0 || pdeath()!=SIGUSR2
        || !proc_ids(path,1234,1235) || !proc_ids("/proc/self/status",0,0)) fail(188);
    if(prctl(PR_SET_DUMPABLE,1,0,0,0)) fail(189);
    atomic_store(&state.phase,2);
    if(pthread_join(worker,0)) fail(190);
}
static void permission_and_denial(const char *path)
{
    int fd=open(path,O_CREAT|O_WRONLY|O_TRUNC,0400);
    if(fd<0 || write(fd,"F",1)!=1 || fchown(fd,1234,1235) || fchmod(fd,0400)) fail(191);
    close(fd);
    if(fsuid(1234)!=0 || fsuid(-1U)!=1234) fail(192);
    fd=open(path,O_RDONLY); char byte=0;
    if(fd<0 || read(fd,&byte,1)!=1 || byte!='F') fail(193);
    close(fd);
    if(fsuid(5678)!=1234) fail(194);
    errno=0; if(open(path,O_RDONLY)!=-1 || errno!=EACCES) fail(195);
    if(syscall(SYS_faccessat,AT_FDCWD,path,R_OK)) fail(248);
    /* Unchanged res-ID operands still restore filesystem IDs to effective IDs. */
    if(syscall(SYS_setresuid,-1U,-1U,-1U) || fsuid(-1U)!=0 || dumpable()!=0
        || fsgid(5678)!=0 || syscall(SYS_setresgid,-1U,-1U,-1U) || fsgid(-1U)!=0) fail(196);
    if(syscall(SYS_setresgid,1000,1000,1000) || syscall(SYS_setresuid,1000,1000,1000)
        || prctl(PR_SET_DUMPABLE,1,0,0,0) || prctl(PR_SET_PDEATHSIG,SIGUSR1,0,0,0)) fail(197);
    if(fsuid(2000)!=1000 || fsgid(2000)!=1000 || fsuid(-1U)!=1000 || fsgid(-1U)!=1000
        || dumpable()!=1 || pdeath()!=SIGUSR1) fail(198);
    errno=0; if(syscall(SYS_setuid,-1U)!=-1 || errno!=EINVAL) fail(199);
    errno=0; if(syscall(SYS_setuid,2000)!=-1 || errno!=EPERM
        || dumpable()!=1 || pdeath()!=SIGUSR1) fail(200);
}
static int set_shared_dumpability(void *argument)
{
    (void)argument; return prctl(PR_SET_DUMPABLE,1,0,0,0) ? 201 : 0;
}
static void mm_dumpability(void)
{
    if(prctl(PR_SET_DUMPABLE,0,0,0,0)) fail(202);
    pid_t child=fork();
    if(child==0) _exit(dumpable()==0 && !prctl(PR_SET_DUMPABLE,1,0,0,0) && dumpable()==1 ? 0 : 203);
    if(!child_ok(child) || dumpable()!=0) fail(204);
    void *stack=mmap(0,128*1024,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(stack==MAP_FAILED || !child_ok(clone(set_shared_dumpability,(char*)stack+128*1024,CLONE_VM|SIGCHLD,0))
        || dumpable()!=1) fail(205);
    errno=0; if(prctl(PR_SET_DUMPABLE,2,0,0,0)!=-1 || errno!=EINVAL || dumpable()!=1) fail(206);
    /* Linux ignores the remaining arguments for these two prctl operations. */
    if(prctl(PR_SET_DUMPABLE,0,1,2,3) || prctl(PR_GET_DUMPABLE,1,2,3,4)!=0) fail(207);
    if(munmap(stack,128*1024)) fail(208);
}
static int link_probe(const char *path,int allowed)
{
    char bytes[256]; errno=0; ssize_t count=readlink(path,bytes,sizeof bytes);
    return allowed ? count>0 : count==-1 && errno==EACCES;
}
static void proc_permission(void)
{
    int requests[2],answers[2]; if(pipe(requests) || pipe(answers)) fail(209);
    pid_t target=fork();
    if(target==0) {
        close(requests[1]); close(answers[0]);
        if(syscall(SYS_setresgid,1000,1000,1000) || syscall(SYS_setresuid,1000,1000,1000)
            || dumpable()!=0 || write(answers[1],"R",1)!=1) fail(210);
        char byte;
        if(read(requests[0],&byte,1)!=1 || prctl(PR_SET_DUMPABLE,1,0,0,0)
            || write(answers[1],"D",1)!=1 || read(requests[0],&byte,1)!=1) fail(211);
        _exit(0);
    }
    close(requests[0]); close(answers[1]); char byte,path[128];
    if(target<0 || read(answers[0],&byte,1)!=1) fail(212);
    snprintf(path,sizeof path,"/proc/%ld/root",(long)target);
    if(!link_probe(path,1)) fail(213); /* SYS_PTRACE bypasses nondumpable/ID checks. */
    pid_t caller=fork();
    if(caller==0) {
        if(syscall(SYS_setresgid,2000,2000,1000) || syscall(SYS_setresuid,2000,2000,1000)
            || fsuid(1000)!=2000 || fsgid(1000)!=2000 || geteuid()!=2000 || getegid()!=2000
            || !link_probe(path,0)) fail(214);
        if(write(requests[1],"D",1)!=1 || read(answers[0],&byte,1)!=1 || !link_probe(path,1)) fail(215);
        if(fsgid(2000)!=1000 || !link_probe(path,0)) fail(216);
        _exit(0);
    }
    if(!child_ok(caller) || write(requests[1],"E",1)!=1 || !child_ok(target)) fail(217);
    close(requests[1]); close(answers[0]);
}
int verify_fs_credentials_exec(int expected,int chain)
{
    if(dumpable()!=expected || fsuid(-1U)!=(long)geteuid() || fsgid(-1U)!=(long)getegid()
        || getauxval(AT_SECURE)!=0) return 218;
    errno=0; if(fcntl(99,F_GETFD)!=-1 || errno!=EBADF) return 219;
    if(chain) {
        if(pdeath()!=0) return 220;
        execl("/bin/init","init","fs-credentials-exec","1","0",(char*)0);
        return 221;
    }
    return 0;
}
static void ordinary_exec(int fs_only)
{
    int fd=open("/bin/init",O_RDONLY|O_CLOEXEC);
    if(fd<0 || dup3(fd,99,O_CLOEXEC)<0 || prctl(PR_SET_DUMPABLE,0,0,0,0)
        || prctl(PR_SET_PDEATHSIG,SIGUSR1,0,0,0)) fail(222);
    if(fs_only && fsuid(1234)!=0) fail(223);
    execl("/bin/init","init","fs-credentials-exec",fs_only ? "0" : "1",fs_only ? "1" : "0",(char*)0);
    fail(224);
}
static void execute_only(const char *root,int dynamic)
{
    char path[256];
    if(dynamic) {
        if(chroot(root) || chdir("/")) fail(225);
        strcpy(path,"/bin/dynamic");
    } else snprintf(path,sizeof path,"%s/execute-only",root);
    if(syscall(SYS_setresgid,1000,1000,1000) || syscall(SYS_setresuid,1000,1000,1000)
        || prctl(PR_SET_DUMPABLE,1,0,0,0)) fail(226);
    if(dynamic) execl(path,"dynamic","fs-credentials-dynamic","0",(char*)0);
    else execl(path,"init","fs-credentials-exec","0","0",(char*)0);
    fail(227);
}
static void pdeath_order(int death_first)
{
    int ready[2],gate[2],report[2]; if(pipe(ready) || pipe(gate) || pipe(report)) fail(228);
    pid_t parent=fork();
    if(parent==0) {
        pid_t child=fork();
        if(child==0) {
            close(ready[0]); close(gate[1]); close(report[0]);
            pid_t creator=getppid();
            sigset_t blocked; sigemptyset(&blocked); sigaddset(&blocked,SIGUSR1);
            if(sigprocmask(SIG_BLOCK,&blocked,0) || prctl(PR_SET_PDEATHSIG,SIGUSR1,0,0,0)) fail(229);
            if(!death_first && (fsuid(1234)!=0 || pdeath()!=0)) fail(230);
            if(write(ready[1],"R",1)!=1) fail(231);
            char byte;
            if(read(gate[0],&byte,1)!=1) fail(232);
            if(death_first && (fsuid(1234)!=0 || pdeath()!=0)) fail(233);
            struct timespec timeout={0}; siginfo_t info; errno=0;
            int signal=sigtimedwait(&blocked,&info,&timeout);
            int okay=death_first ? signal==SIGUSR1 && info.si_pid==creator : signal==-1 && errno==EAGAIN;
            if(write(report[1],okay ? "Y" : "N",1)!=1) fail(234);
            _exit(0);
        }
        close(ready[1]); char byte;
        if(child<0 || read(ready[0],&byte,1)!=1) fail(235);
        _exit(0);
    }
    close(gate[0]); close(report[1]);
    char byte;
    if(!child_ok(parent) || write(gate[1],"G",1)!=1 || read(report[0],&byte,1)!=1 || byte!='Y') fail(236);
    close(gate[1]); close(report[0]); close(ready[0]); close(ready[1]);
}
int verify_fs_credentials(void)
{
    if(mkdir("/tmp",0777) && errno!=EEXIST) return 237;
    if(mkdir("/proc",0755) && errno!=EEXIST) return 238;
    if(mount("proc","/proc","proc",0,0) && errno!=EBUSY) return 239;
    char root[128],path[256]; snprintf(root,sizeof root,"/tmp/fs-credentials-%ld",(long)getpid());
    if(mkdir(root,0755)) return 240;
    snprintf(path,sizeof path,"%s/execute-only",root); if(!copy_file("/bin/init",path,0111)) return 241;
    for(const char **dir=(const char*[]){"bin","lib","usr","usr/lib",0};*dir;dir++) {
        snprintf(path,sizeof path,"%s/%s",root,*dir); if(mkdir(path,0755)) return 242;
    }
    snprintf(path,sizeof path,"%s/bin/dynamic",root); if(!copy_file("/bin/fs-root-dynamic",path,0755)) return 243;
    snprintf(path,sizeof path,"%s/usr/lib/libc.so",root); if(!copy_file("/usr/lib/libc.so",path,0111)) return 244;
#if defined(__aarch64__)
    snprintf(path,sizeof path,"%s/lib/ld-musl-aarch64.so.1",root);
#else
    snprintf(path,sizeof path,"%s/lib/ld-musl-riscv64.so.1",root);
#endif
    if(symlink("/usr/lib/libc.so",path)) return 245;
    snprintf(path,sizeof path,"%s/owned",root);
    for(int scenario=0;scenario<10;scenario++) {
        pid_t child=fork();
        if(child==0) {
            switch(scenario) {
                case 0: thread_identity(); break;
                case 1: permission_and_denial(path); break;
                case 2: mm_dumpability(); break;
                case 3: proc_permission(); break;
                case 4: ordinary_exec(0); break;
                case 5: ordinary_exec(1); break;
                case 6: execute_only(root,0); break;
                case 7: execute_only(root,1); break;
                case 8: pdeath_order(0); break;
                case 9: pdeath_order(1); break;
            }
            _exit(0);
        }
        if(!child_ok(child)) return 246;
    }
    puts("LiteOS musl filesystem IDs dumpability exec proc and parent death ok"); return 0;
}

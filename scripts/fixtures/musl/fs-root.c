#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static _Noreturn void fail(int step) { dprintf(2,"fs root fail step=%d errno=%d\n",step,errno); _exit(step); }
static int child_ok(pid_t child)
{
    int status=-1;
    if (child>0 && waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status)) return 1;
    dprintf(2,"fs root child status=%d\n",status); return 0;
}
static int byte_at(int dirfd, const char *path, char expected)
{
    char byte=0; int fd=openat(dirfd,path,O_RDONLY);
    int okay=fd>=0 && read(fd,&byte,1)==1 && byte==expected;
    if(fd>=0) close(fd); return okay;
}
static int write_file(const char *name, const char *bytes, size_t length, int mode)
{
    int fd=open(name,O_WRONLY|O_CREAT|O_TRUNC,mode);
    if(fd<0) return 0;
    int okay=write(fd,bytes,length)==(ssize_t)length && fchmod(fd,mode)==0;
    close(fd); return okay;
}
static int copy_file(const char *source, const char *target)
{
    int in=open(source,O_RDONLY),out=open(target,O_WRONLY|O_CREAT|O_TRUNC,0755);
    if(in<0 || out<0) return 0;
    char bytes[4096]; ssize_t count;
    while((count=read(in,bytes,sizeof bytes))>0) if(write(out,bytes,count)!=count) return 0;
    int okay=count==0 && fchmod(out,0755)==0; close(in); close(out); return okay;
}
static int cwd_is(const char *expected)
{
    char cwd[256]; return getcwd(cwd,sizeof cwd) && !strcmp(cwd,expected);
}
static int link_is(const char *name,const char *expected)
{
    char bytes[256]; ssize_t count=readlink(name,bytes,sizeof bytes);
    return count==(ssize_t)strlen(expected) && !memcmp(bytes,expected,count);
}
struct fixture { char root[128],outside[128],outside_relative[128]; };
static int setup(struct fixture *f)
{
    if (mkdir("/tmp", 0777) && errno != EEXIST) return 0;
    snprintf(f->root,sizeof f->root,"/tmp/fs-root-%ld",(long)getpid());
    snprintf(f->outside,sizeof f->outside,"/tmp/fs-outside-%ld",(long)getpid());
    snprintf(f->outside_relative,sizeof f->outside_relative,"tmp/fs-outside-%ld",(long)getpid());
    if(chdir("/") || mkdir(f->root,0755) || !write_file(f->outside,"O",1,0644)) return 0;
    char path[256];
    for(const char **dir=(const char*[]){"sub","mnt","proc","bin","lib","usr","usr/lib",0};*dir;dir++) {
        snprintf(path,sizeof path,"%s/%s",f->root,*dir); if(mkdir(path,0755)) return 0;
    }
    snprintf(path,sizeof path,"%s/target",f->root); if(!write_file(path,"J",1,0644)) return 0;
    snprintf(path,sizeof path,"%s/absolute",f->root); if(symlink("/target",path)) return 0;
    snprintf(path,sizeof path,"%s/relative",f->root); if(symlink("sub/../target",path)) return 0;
    snprintf(path,sizeof path,"%s/escape",f->root); if(symlink(f->outside,path)) return 0;
    snprintf(path,sizeof path,"%s/proc",f->root); if(mount("proc",path,"proc",0,0)) return 0;
    snprintf(path,sizeof path,"%s/mnt",f->root); if(mount("tmpfs",path,"tmpfs",0,0)) return 0;
    snprintf(path,sizeof path,"%s/bin/init",f->root); if(!copy_file("/bin/init",path)) return 0;
    snprintf(path,sizeof path,"%s/bin/dynamic",f->root); if(!copy_file("/bin/fs-root-dynamic",path)) return 0;
    snprintf(path,sizeof path,"%s/usr/lib/libc.so",f->root); if(!copy_file("/usr/lib/libc.so",path)) return 0;
#if defined(__aarch64__)
    snprintf(path,sizeof path,"%s/lib/ld-musl-aarch64.so.1",f->root);
#else
    snprintf(path,sizeof path,"%s/lib/ld-musl-riscv64.so.1",f->root);
#endif
    if(symlink("/usr/lib/libc.so",path)) return 0;
    snprintf(path,sizeof path,"%s/script",f->root); if(!write_file(path,"#!/bin/init fs-root-exec\n",24,0755)) return 0;
    return 1;
}
int verify_fs_root_exec(void)
{
    return cwd_is("/") && byte_at(AT_FDCWD,"/target",'J') && link_is("/proc/self/root","/") ? 0 : 135;
}
static void lookup_and_proc(struct fixture *f)
{
    int outside_fd=open("/",O_RDONLY|O_DIRECTORY);
    if(chdir(f->root) || chdir("sub")) fail(166);
    if(outside_fd<0 || chroot(f->root)) fail(136);
    char cwd[256];
    if(syscall(SYS_getcwd,cwd,sizeof cwd)<=0 || strcmp(cwd,"/sub")) fail(137);
    if(fchdir(outside_fd)) fail(167);
    if(syscall(SYS_getcwd,cwd,sizeof cwd)<=0 || strncmp(cwd,"(unreachable)",13)) fail(168);
    errno=0; if(getcwd(cwd,sizeof cwd)!=0 || errno!=ENOENT) fail(138);
    if(chdir("/sub") || !cwd_is("/sub") || chdir("../../..") || !cwd_is("/")) fail(139);
    if(!byte_at(AT_FDCWD,"/absolute",'J') || !byte_at(-999,"/relative",'J')) fail(140);
    errno=0; if(open("/escape",O_RDONLY)!=-1 || errno!=ENOENT) fail(141);
    errno=0; if(open("/target/.",O_RDONLY)!=-1 || errno!=ENOTDIR) fail(142);
    int target_fd=open("/target",O_RDONLY); char fdpath[128];
    snprintf(fdpath,sizeof fdpath,"/proc/self/fd/%d",target_fd);
    if(target_fd<0 || !link_is(fdpath,"/target") || !link_is("/proc/self/root","/")
        || !link_is("/proc/self/cwd","/") || !link_is("/proc/self/exe","/bin/init")) fail(143);
    int nested=open("/proc/self/root/target",O_CREAT|O_RDONLY,0644);
    if(nested<0) fail(170); close(nested);
    nested=open("/proc/self/root/new-entry",O_CREAT|O_WRONLY,0644);
    if(nested<0 || write(nested,"N",1)!=1) fail(171); close(nested);
    struct stat proc_stat;
    if(stat("/proc/mounts",&proc_stat) || proc_stat.st_size!=0) fail(172);
    nested=open("/proc/mounts",O_RDONLY); char mount_text[256];
    if(nested<0 || read(nested,mount_text,sizeof mount_text)<=0) fail(173); close(nested);
    errno=0; if(open("/proc/self/root/target/",O_RDONLY)!=-1 || errno!=ENOTDIR) fail(164);
    int directory=open("/proc/self/root/sub/",O_RDONLY|O_DIRECTORY);
    struct stat fd_stat;
    snprintf(fdpath,sizeof fdpath,"/proc/self/fd/%d",target_fd);
    if(lstat(fdpath,&fd_stat) || fd_stat.st_size!=64) fail(183);
    if(lstat("/proc/self/root",&fd_stat) || fd_stat.st_size!=0) fail(184);
    if(directory<0) fail(165); close(directory);
    if(!byte_at(AT_FDCWD,"/proc/self/root/target",'J')
        || !byte_at(AT_FDCWD,"/proc/self/cwd/target",'J') || !byte_at(outside_fd,f->outside_relative,'O')) fail(144);
    snprintf(fdpath,sizeof fdpath,"/proc/self/fd/%d/%s",outside_fd,f->outside_relative);
    if(!byte_at(AT_FDCWD,fdpath,'O') || fchdir(outside_fd)
        || !byte_at(AT_FDCWD,f->outside_relative,'O') || chdir("/")) fail(145);
    close(target_fd); close(outside_fd);
    /* AF_UNIX namespace pathname shares the same origin; relative/absolute refer to one inode. */
    int server=socket(AF_UNIX,SOCK_DGRAM,0),client=socket(AF_UNIX,SOCK_DGRAM,0);
    struct sockaddr_un address={.sun_family=AF_UNIX}; strcpy(address.sun_path,"/endpoint");
    if(server<0 || client<0 || bind(server,(void*)&address,sizeof address)) fail(146);
    strcpy(address.sun_path,"endpoint");
    if(connect(client,(void*)&address,sizeof address) || send(client,"U",1,0)!=1) fail(147);
    char byte; if(recv(server,&byte,1,0)!=1 || byte!='U') fail(148);
    close(server); close(client);
    for(int mode=0;mode<2;mode++) {
        pid_t image=fork();
        if(image==0) { if(mode) execl("/bin/dynamic","dynamic",(char*)0); else execl("/script","script",(char*)0); fail(149); }
        if(!child_ok(image)) fail(150);
    }
}
static int shared_process(void *argument)
{
    return chroot(((struct fixture*)argument)->root) ? 151 : 0;
}
struct thread_input { struct fixture *fixture; int detach; atomic_int okay, tid, phase; };
static void *shared_thread(void *argument)
{
    struct thread_input *input=argument;
    int okay=(!input->detach || !unshare(CLONE_FS)) && !chroot(input->fixture->root)
        && byte_at(AT_FDCWD,"/target",'J');
    atomic_store(&input->okay,okay);
    atomic_store(&input->tid,syscall(SYS_gettid));
    while(!atomic_load(&input->phase)) sched_yield();
    return 0;
}
static int thread_roots(struct fixture *f,int detach)
{
    struct thread_input input={.fixture=f,.detach=detach}; pthread_t worker;
    if(pthread_create(&worker,0,shared_thread,&input)) return 0;
    while(!atomic_load(&input.tid)) sched_yield();
    char procbase[256], rootpath[512];
    if(detach) snprintf(procbase,sizeof procbase,"%s/proc",f->root);
    else strcpy(procbase,"/proc");
    snprintf(rootpath,sizeof rootpath,"%s/self/task/%d/root",procbase,atomic_load(&input.tid));
    int proc_ok=link_is(rootpath,detach ? f->root : "/");
    if(!proc_ok) dprintf(2,"fs root thread root view detach=%d path=%s errno=%d\n",detach,rootpath,errno);
    snprintf(rootpath,sizeof rootpath,"%s/self/task/%d/cwd/%s",procbase,atomic_load(&input.tid),f->outside_relative);
    int cwd_ok=byte_at(AT_FDCWD,rootpath,'O');
    if(!cwd_ok) dprintf(2,"fs root thread cwd lookup detach=%d path=%s errno=%d\n",detach,rootpath,errno);
    proc_ok=proc_ok && cwd_ok;
    atomic_store(&input.phase,1);
    int joined=pthread_join(worker,0);
    if(joined || !atomic_load(&input.okay) || !proc_ok) { dprintf(2,"fs root thread joined=%d worker_ok=%d proc_ok=%d detach=%d\n",joined,atomic_load(&input.okay),proc_ok,detach); return 0; }
    return detach ? byte_at(AT_FDCWD,f->outside,'O') : byte_at(AT_FDCWD,"/target",'J');
}
int verify_fs_root(void)
{
    struct fixture f;
    if(!setup(&f)) { dprintf(2,"fs root setup errno=%d\n",errno); return 152; }
    for(int scenario=0;scenario<9;scenario++) {
        pid_t child=fork();
        if(child==0) {
            if(scenario==0) lookup_and_proc(&f);
            if(scenario==1 || scenario==2) { if(!thread_roots(&f,scenario==2)) fail(153); }
            if(scenario==3) {
                void *stack=mmap(0,128*1024,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
                if(stack==MAP_FAILED || !child_ok(clone(shared_process,(char*)stack+128*1024,CLONE_FS|SIGCHLD,&f))
                    || !byte_at(AT_FDCWD,"/target",'J')) fail(154);
            }
            if(scenario==4) {
                char mount_root[256]; snprintf(mount_root,sizeof mount_root,"%s/mnt",f.root);
                if(chroot(mount_root) || chdir("/") || chdir("..") || !cwd_is("/")) fail(155);
                errno=0; if(open("/target",O_RDONLY)!=-1 || errno!=ENOENT) fail(156);
            }
            if(scenario==6) {
                char mount_root[256], old_cwd[256], expected[256], cwd[256];
                snprintf(mount_root,sizeof mount_root,"%s/mnt",f.root);
                snprintf(old_cwd,sizeof old_cwd,"%s/sub",f.root);
                snprintf(expected,sizeof expected,"(unreachable)%s",old_cwd);
                if(chdir(old_cwd) || chroot(mount_root) || syscall(SYS_getcwd,cwd,sizeof cwd)<=0 || strcmp(cwd,expected)) fail(169);
            }
            if(scenario==7) {
                char mount_root[256], name[257], cwd[8192];
                snprintf(mount_root,sizeof mount_root,"%s/mnt",f.root);
                if(chroot(mount_root) || chdir("/")) fail(175);
                memset(name,'x',256); name[256]=0;
                errno=0; if(open(name,O_RDONLY)!=-1 || errno!=ENAMETOOLONG) fail(176);
                errno=0; if(mkdir(name,0755)!=-1 || errno!=ENAMETOOLONG) fail(177);
                name[254]=0;
                for(int depth=0;depth<16;depth++) if(mkdir(name,0755) || chdir(name)) fail(178);
                if(syscall(SYS_getcwd,cwd,sizeof cwd)!=4081 || strlen(cwd)!=4080) fail(179);
                errno=0; if(syscall(SYS_getcwd,cwd,4080)!=-1 || errno!=ERANGE) fail(180);
                if(mkdir("yyyyyyyyyyyyyyy",0755) || chdir("yyyyyyyyyyyyyyy")) fail(181);
                errno=0; if(syscall(SYS_getcwd,cwd,sizeof cwd)!=-1 || errno!=ENAMETOOLONG) fail(182);
            }
            if(scenario==5) {
                if(setuid(1000)) fail(157);
                errno=0; if(chroot("/no-such-directory")!=-1 || errno!=ENOENT) fail(158);
                errno=0; if(chroot(f.outside)!=-1 || errno!=ENOTDIR) fail(159);
                errno=0; if(chroot(f.root)!=-1 || errno!=EPERM) fail(160);
            }
            if(scenario==8) {
                char root[256]; snprintf(root,sizeof root,"%s/no-search",f.root);
                if(mkdir(root,0700) || chown(root,1000,1000) || chmod(root,0400)) fail(183);
                int captured=open(root,O_PATH|O_DIRECTORY);
                if(captured<0 || chroot(root) || chdir("/") || setresgid(1000,1000,1000)
                    || setresuid(1000,1000,1000)) fail(184);
                int path=open("/",O_PATH|O_DIRECTORY), readable=open("/",O_RDONLY|O_DIRECTORY);
                struct stat metadata;
                if(path<0 || readable<0 || fstat(path,&metadata) || metadata.st_uid!=1000
                    || (metadata.st_mode&0777)!=0400) fail(185);
                errno=0; if(open(".",O_PATH|O_DIRECTORY)!=-1 || errno!=EACCES) fail(186);
                errno=0; if(fchdir(captured)!=-1 || errno!=EACCES) fail(187);
                errno=0; if(openat(captured,"",O_PATH)!=-1 || errno!=ENOENT) fail(188);
                close(readable); close(path); close(captured);
            }
            _exit(0);
        }
        if(!child_ok(child) || !byte_at(AT_FDCWD,f.outside,'O')) return 161;
    }
    char path[256]; snprintf(path,sizeof path,"%s/mnt",f.root); if(umount(path)) return 162;
    snprintf(path,sizeof path,"%s/proc",f.root); if(umount(path)) return 163;
    puts("LiteOS musl fs root chroot lookup exec and proc views ok"); return 0;
}

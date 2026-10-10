#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sys/resource.h>
#include <signal.h>
#include <sys/signalfd.h>
#include <sys/sendfile.h>
#include <sys/uio.h>
#include <time.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <unistd.h>

static _Noreturn void fail(int step)
{ dprintf(2,"anonymous files fail step=%d errno=%d\n",step,errno); _exit(step); }
static void fd_path(int fd,char *path,size_t size)
{ snprintf(path,size,"/proc/self/fd/%d",fd); }
static int reopen(int fd,int flags)
{ char path[64]; fd_path(fd,path,sizeof path); return open(path,flags); }
static void same_inode(int fd,int other,int step)
{
    struct stat a,b;
    if(fstat(fd,&a) || fstat(other,&b) || a.st_dev!=b.st_dev || a.st_ino!=b.st_ino) fail(step);
}
static void same_path_statfs(int fd,long magic,int step)
{
    char path[64]; struct statfs a,b;
    fd_path(fd,path,sizeof path);
    if(fstatfs(fd,&a) || statfs(path,&b) || a.f_type!=magic || b.f_type!=magic
        || a.f_bsize!=b.f_bsize || memcmp(&a.f_fsid,&b.f_fsid,sizeof a.f_fsid)) fail(step);
}
static void memfd_identity(void)
{
    int fd=memfd_create("collision",MFD_ALLOW_SEALING|MFD_CLOEXEC);
    if(fd<0 || write(fd,"actual",6)!=6) fail(20);
    char path[64],label[128]; fd_path(fd,path,sizeof path);
    ssize_t size=readlink(path,label,sizeof label-1);
    if(size<0) fail(21); label[size]=0;
    if(strcmp(label,"/memfd:collision (deleted)")) fail(22);
    int shadow=open(label,O_CREAT|O_RDWR|O_TRUNC,0600);
    if(shadow<0 || write(shadow,"shadow",6)!=6) fail(23);
    int other=reopen(fd,O_RDWR|O_CLOEXEC);
    char data[7]={0};
    if(other<0 || read(other,data,6)!=6 || strcmp(data,"actual")) fail(24);
    same_inode(fd,other,25);
    if(lseek(fd,0,SEEK_CUR)!=6 || lseek(other,0,SEEK_CUR)!=6) fail(26);
    if(lseek(other,0,SEEK_SET) || lseek(fd,0,SEEK_CUR)!=6
        || fcntl(other,F_SETFL,O_APPEND|O_NONBLOCK)
        || (fcntl(fd,F_GETFL)&(O_APPEND|O_NONBLOCK))
        || !(fcntl(other,F_GETFD)&FD_CLOEXEC)) fail(27);
    void *map=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,other,0);
    if(map==MAP_FAILED) fail(28);
    ((char *)map)[0]='A';
    if(pread(fd,data,6,0)!=6 || data[0]!='A' || munmap(map,4096)) fail(29);
    if(fcntl(other,F_ADD_SEALS,F_SEAL_GROW) || !(fcntl(fd,F_GET_SEALS)&F_SEAL_GROW)) fail(30);
    int ro=reopen(fd,O_RDONLY);
    if(ro<0 || !(fcntl(ro,F_GET_SEALS)&F_SEAL_GROW)) fail(31);
    errno=0; if(fcntl(ro,F_ADD_SEALS,0)!=-1 || errno!=EPERM) fail(32);
    errno=0; if(fcntl(ro,F_ADD_SEALS,~0U)!=-1 || errno!=EPERM) fail(33);
    errno=0; if(ftruncate(fd,7)!=-1 || errno!=EPERM) fail(34);
    same_path_statfs(fd,0x01021994,35);
    close(ro); close(other); close(shadow); close(fd); unlink(label);
}
static void sealed_metadata_failure(void)
{
    int fd=memfd_create("sealed-mode",MFD_ALLOW_SEALING);
    struct stat before,after;
    if(fd<0 || write(fd,"SS",2)!=2 || fchown(fd,1000,1001) || fchmod(fd,06777)
        || fcntl(fd,F_ADD_SEALS,F_SEAL_SHRINK|F_SEAL_GROW)
        || syscall(SYS_setfsuid,1000)!=0 || syscall(SYS_setfsgid,1001)!=0 || fstat(fd,&before)) fail(36);
    for(int size=0;size<=3;size+=3) {
        errno=0; if(ftruncate(fd,size)!=-1 || errno!=EPERM || fstat(fd,&after)
            || after.st_size!=before.st_size || after.st_mode!=before.st_mode
            || after.st_ctim.tv_sec!=before.st_ctim.tv_sec) fail(37);
    }
    if(ftruncate(fd,2) || fstat(fd,&after) || (after.st_mode&06000)) fail(38);
    close(fd);
}
static void creator_attributes(void)
{
    if(syscall(SYS_setfsuid,1234)!=0 || syscall(SYS_setfsgid,1235)!=0) fail(40);
    int fd=memfd_create("owner",0),pipes[2],sockets[2];
    if(fd<0 || pipe(pipes) || socketpair(AF_UNIX,SOCK_STREAM,0,sockets)) fail(41);
    struct stat st;
    for(int i=0;i<5;i++) {
        int current=i==0?fd:i<3?pipes[i-1]:sockets[i-3];
        if(fstat(current,&st) || st.st_uid!=1234 || st.st_gid!=1235) fail(42);
        if(i==0 && (!S_ISREG(st.st_mode) || (st.st_mode&0777)!=0777 || st.st_nlink)) fail(43);
        if(i>0 && i<3 && (!S_ISFIFO(st.st_mode) || (st.st_mode&0777)!=0600)) fail(44);
        if(i>=3 && (!S_ISSOCK(st.st_mode) || (st.st_mode&0777)!=0777)) fail(45);
    }
    same_inode(pipes[0],pipes[1],46);
    if(fchmod(fd,06777) || write(fd,"K",1)!=1 || fstat(fd,&st) || (st.st_mode&06000)) fail(47);
    if(fchmod(pipes[0],0640) || fstat(pipes[1],&st) || (st.st_mode&0777)!=0640) fail(48);
    if(fchmod(sockets[0],0600) || fstat(sockets[0],&st) || (st.st_mode&0777)!=0600) fail(49);
    if(syscall(SYS_setfsuid,0)!=1234 || syscall(SYS_setfsgid,0)!=1235) fail(50);
    if(fchown(fd,2000,2001) || fstat(fd,&st) || st.st_uid!=2000 || st.st_gid!=2001
        || fchown(pipes[0],2002,2003) || fstat(pipes[1],&st) || st.st_uid!=2002 || st.st_gid!=2003
        || fchown(sockets[0],2004,2005) || fstat(sockets[0],&st) || st.st_uid!=2004 || st.st_gid!=2005) fail(51);
    close(fd); close(pipes[0]); close(pipes[1]); close(sockets[0]); close(sockets[1]);
}
static void pipe_reopen(void)
{
    int pipes[2]; if(pipe2(pipes,O_CLOEXEC|O_NONBLOCK)) fail(60);
    close(pipes[1]);
    /* Anonymous reopen has neither named-FIFO ENXIO nor rendezvous. */
    int writer=reopen(pipes[0],O_WRONLY|O_NONBLOCK);
    if(writer<0 || write(writer,"P",1)!=1) fail(61);
    char byte=0; if(read(pipes[0],&byte,1)!=1 || byte!='P') fail(62);
    same_inode(pipes[0],writer,63);
    same_path_statfs(writer,0x50495045,64);
    close(writer);
    struct pollfd poller={.fd=pipes[0],.events=POLLIN};
    if(poll(&poller,1,0)!=1 || !(poller.revents&POLLHUP)) fail(65);
    int rw=reopen(pipes[0],O_RDWR);
    if(rw<0 || write(rw,"R",1)!=1 || read(rw,&byte,1)!=1 || byte!='R') fail(66);
    if(fcntl(rw,F_SETPIPE_SZ,8192)!=8192 || fcntl(pipes[0],F_GETPIPE_SZ)!=8192) fail(67);
    same_inode(pipes[0],rw,68); close(rw); close(pipes[0]);
    if(pipe(pipes)) fail(69);
    close(pipes[0]);
    int reader=reopen(pipes[1],O_RDONLY); /* Must complete with only original writer present. */
    if(reader<0 || write(pipes[1],"B",1)!=1 || read(reader,&byte,1)!=1 || byte!='B') fail(70);
    close(reader);
    int next_writer=reopen(pipes[1],O_WRONLY); /* Missing reader must not block this open. */
    if(next_writer<0) fail(71);
    reader=reopen(pipes[1],O_RDONLY|O_NONBLOCK);
    if(reader<0) fail(72);
    close(pipes[1]); close(next_writer);
    poller=(struct pollfd){.fd=reader,.events=POLLIN};
    if(poll(&poller,1,0)!=1 || !(poller.revents&POLLHUP) || read(reader,&byte,1)!=0) fail(73);
    close(reader);
    const char *fifo="/tmp/anonymous-named-fifo";
    if(mkfifo(fifo,0600)) fail(74);
    int hold=open(fifo,O_RDWR|O_NONBLOCK),named=open(fifo,O_RDONLY|O_NONBLOCK);
    if(hold<0 || named<0) fail(75);
    close(hold); poller=(struct pollfd){.fd=named,.events=POLLIN};
    if(poll(&poller,1,0)!=1 || !(poller.revents&POLLHUP)) fail(76);
    close(named); unlink(fifo);
}
static void zero_length_descriptor_reads(void)
{
    int pipes[2]; char byte;
    if(pipe2(pipes,O_CLOEXEC|O_NONBLOCK) || read(pipes[0],NULL,0)!=0
        || write(pipes[1],"z",1)!=1 || read(pipes[0],NULL,0)!=0
        || read(pipes[0],&byte,1)!=1 || byte!='z') fail(209);
    close(pipes[0]); close(pipes[1]);
    int watcher=inotify_init1(IN_NONBLOCK);
    if(watcher<0) fail(210);
    errno=0; if(read(watcher,NULL,0)!=-1 || errno!=EAGAIN) fail(211);
    const char *path="/tmp/zero-length-inotify";
    int fd=open(path,O_CREAT|O_RDWR|O_TRUNC,0600);
    if(fd<0 || inotify_add_watch(watcher,path,IN_MODIFY)<0 || write(fd,"x",1)!=1) fail(212);
    errno=0; if(read(watcher,NULL,0)!=-1 || errno!=EINVAL) fail(213);
    struct inotify_event event;
    if(read(watcher,&event,sizeof event)!=sizeof event || !(event.mask&IN_MODIFY)) fail(214);
    close(fd); close(watcher); unlink(path);
}
static void anonymous_singleton(void)
{
    int fds[]={epoll_create1(0),eventfd(0,0),timerfd_create(CLOCK_MONOTONIC,0),inotify_init1(0)};
    struct stat first,st;
    for(int i=0;i<4;i++) {
        if(fds[i]<0 || fstat(fds[i],&st) || (st.st_mode&S_IFMT) || (st.st_mode&0777)!=0600) fail(80+i);
        if(i==0) first=st;
        else if(st.st_dev!=first.st_dev || st.st_ino!=first.st_ino) fail(84);
        errno=0; if(reopen(fds[i],O_RDWR)!=-1 || errno!=ENXIO) fail(85);
        errno=0; if(fchmod(fds[i],0600)!=-1 || errno!=EOPNOTSUPP) fail(86);
        errno=0; if(fchown(fds[i],0,0)!=-1 || errno!=EOPNOTSUPP) fail(87);
        errno=0; if(mmap(0,4096,PROT_READ,MAP_PRIVATE,fds[i],0)!=MAP_FAILED || errno!=ENODEV) fail(88);
        if(i<3) { errno=0; if(read(fds[i],NULL,0)!=-1 || errno!=EINVAL) fail(206+i); }
        if(i==1) { errno=0; if(write(fds[i],NULL,0)!=-1 || errno!=EINVAL) fail(215); }
        struct iovec empty={.iov_base=(void *)1,.iov_len=0};
        if(i==0) { errno=0; if(readv(fds[i],&empty,1)!=-1 || errno!=EINVAL) fail(216); }
        else if(readv(fds[i],&empty,1)!=0) fail(216);
        if(i==1 && writev(fds[i],&empty,1)!=0) fail(217);
        same_path_statfs(fds[i],0x09041934,89); close(fds[i]);
    }
    int sockets[2]; if(socketpair(AF_UNIX,SOCK_STREAM,0,sockets)) fail(90);
    errno=0; if(reopen(sockets[0],O_RDWR)!=-1 || errno!=ENXIO) fail(91);
    same_path_statfs(sockets[0],0x534f434b,92);
    close(sockets[0]); char byte;
    if(read(sockets[1],&byte,1)!=0) fail(93); close(sockets[1]);
    int fd=open("/dev/null",O_RDWR),other=reopen(fd,O_RDWR);
    if(fd<0 || other<0 || read(other,&byte,1)!=0 || write(other,"D",1)!=1) fail(94);
    same_inode(fd,other,95); close(fd); close(other);
}
static void watch_real_inode(void)
{
    int pipes[2],watcher=inotify_init1(IN_NONBLOCK);
    if(watcher<0 || pipe2(pipes,O_NONBLOCK)) fail(113);
    char path[64]; fd_path(pipes[0],path,sizeof path);
    unsigned expected=IN_OPEN|IN_ACCESS|IN_MODIFY|IN_CLOSE_WRITE|IN_CLOSE_NOWRITE;
    int wd=inotify_add_watch(watcher,path,expected);
    if(wd<0) fail(114);
    int writer=reopen(pipes[0],O_WRONLY|O_NONBLOCK); char byte;
    if(writer<0 || write(writer,"W",1)!=1 || read(pipes[0],&byte,1)!=1) fail(115);
    close(writer); close(pipes[0]); close(pipes[1]);
    unsigned seen=0; char events[4096]; ssize_t count;
    while((count=read(watcher,events,sizeof events))>0) {
        for(size_t at=0;at<(size_t)count;) {
            struct inotify_event event; memcpy(&event,events+at,sizeof event);
            if(event.wd!=wd || at+sizeof event+event.len>(size_t)count) fail(116);
            seen|=event.mask; at+=sizeof event+event.len;
        }
    }
    if(count!=-1 || errno!=EAGAIN || (seen&expected)!=expected) fail(117);
    close(watcher);
    /* Linux S_PRIVATE anon singleton does not produce ordinary fsnotify events. */
    watcher=inotify_init1(IN_NONBLOCK); int event=eventfd(0,0);
    fd_path(event,path,sizeof path);
    if(watcher<0 || event<0 || inotify_add_watch(watcher,path,IN_ACCESS|IN_MODIFY|IN_CLOSE)<0) fail(118);
    eventfd_t value;
    if(eventfd_write(event,1) || eventfd_read(event,&value) || value!=1) fail(119);
    close(event); errno=0;
    if(read(watcher,events,sizeof events)!=-1 || errno!=EAGAIN) fail(120);
    close(watcher);
}
static void failed_truncate_open_lifecycle(void)
{
    int fd=memfd_create("truncate-open-events",MFD_ALLOW_SEALING);
    int watcher=inotify_init1(IN_NONBLOCK);
    char path[64]; fd_path(fd,path,sizeof path);
    if(fd<0 || watcher<0 || write(fd,"keep",4)!=4
        || fcntl(fd,F_ADD_SEALS,F_SEAL_SHRINK)
        || inotify_add_watch(watcher,path,IN_OPEN|IN_CLOSE|IN_MODIFY)<0) fail(160);
    for(int i=0;i<2;i++) {
        errno=0; if(open(path,(i?O_RDONLY:O_WRONLY)|O_TRUNC)!=-1 || errno!=EPERM) fail(161);
        char events[256]; ssize_t size=read(watcher,events,sizeof events);
        if(size!=(ssize_t)(2*sizeof(struct inotify_event))) fail(162);
        struct inotify_event first,last;
        memcpy(&first,events,sizeof first); memcpy(&last,events+sizeof first,sizeof last);
        if(first.mask!=IN_OPEN || last.mask!=(unsigned)(i?IN_CLOSE_NOWRITE:IN_CLOSE_WRITE)) fail(163);
    }
    close(watcher); close(fd);
    fd=memfd_create("readonly-truncate",0);
    if(fd<0 || write(fd,"data",4)!=4) fail(164);
    fd_path(fd,path,sizeof path);
    int ro=open(path,O_RDONLY|O_TRUNC); struct stat st;
    if(ro<0 || fstat(fd,&st) || st.st_size || (fcntl(ro,F_GETFL)&O_ACCMODE)!=O_RDONLY) fail(165);
    close(ro); close(fd);
}
static void explicit_file_times(void)
{
    if(mkdir("/tmp/anonymous-time-fs",0700) || mount("tmpfs","/tmp/anonymous-time-fs","tmpfs",0,0)) fail(99);
    int mem=memfd_create("times",0),tmp=open("/tmp/anonymous-time-fs/file",O_CREAT|O_RDWR|O_TRUNC,0600);
    if(mem<0 || tmp<0) fail(100);
    struct timespec times[2]={{.tv_sec=1},{.tv_sec=2}};
    int fds[]={mem,tmp}; struct stat st;
    for(int i=0;i<2;i++) {
        if(write(fds[i],"T",1)!=1 || futimens(fds[i],times) || fstat(fds[i],&st)
            || st.st_atim.tv_sec!=1 || st.st_mtim.tv_sec!=2) fail(101);
        times[1].tv_sec=4000000000LL;
        if(futimens(fds[i],times) || fstat(fds[i],&st) || st.st_mtim.tv_sec!=4000000000LL
            || st.st_ctim.tv_sec==4000000000LL) fail(103);
        times[0].tv_sec=3; times[1].tv_nsec=UTIME_OMIT;
        if(futimens(fds[i],times) || fstat(fds[i],&st) || st.st_atim.tv_sec!=3
            || st.st_mtim.tv_sec!=4000000000LL || st.st_ctim.tv_sec==4000000000LL) fail(104);
        times[0]=(struct timespec){.tv_sec=1}; times[1]=(struct timespec){.tv_sec=2};
        close(fds[i]);
    }
    times[0].tv_nsec=times[1].tv_nsec=UTIME_OMIT;
    if(syscall(SYS_utimensat,-999,(void *)1,times,~0U)) fail(102);
    unlink("/tmp/anonymous-time-fs/file");
    if(umount("/tmp/anonymous-time-fs") || rmdir("/tmp/anonymous-time-fs")) fail(105);
}
static void open_fd_admission(void)
{
    const char *existing="/tmp/open-fd-existing", *created="/tmp/open-fd-created", *fifo="/tmp/open-fd-no-peer";
    int fd=open(existing,O_CREAT|O_RDWR|O_TRUNC,0600);
    struct stat before,after;
    if(fd<0 || write(fd,"keep",4)!=4 || fchmod(fd,06777) || fstat(fd,&before)
        || mkfifo(fifo,0600)) fail(120);
    struct rlimit limit={32,32};
    if(setrlimit(RLIMIT_NOFILE,&limit)) fail(121);
    int copies[32],count=0;
    for(;;) {
        int copy=fcntl(fd,F_DUPFD,0);
        if(copy<0) { if(errno!=EMFILE) fail(122); break; }
        copies[count++]=copy;
    }
    errno=0; if(open(created,O_CREAT|O_WRONLY,0600)!=-1 || errno!=EMFILE) fail(123);
    errno=0; if(stat(created,&after)!=-1 || errno!=ENOENT) fail(124);
    errno=0; if(open(existing,O_TRUNC|O_WRONLY)!=-1 || errno!=EMFILE) fail(125);
    char data[4];
    if(pread(fd,data,4,0)!=4 || memcmp(data,"keep",4) || fstat(fd,&after)
        || before.st_size!=after.st_size || before.st_mode!=after.st_mode
        || before.st_ctim.tv_sec!=after.st_ctim.tv_sec) fail(126);
    errno=0; if(open("/tmp/absent/child",O_RDONLY)!=-1 || errno!=EMFILE) fail(127);
    errno=0; if(openat(-999,"relative",O_RDONLY)!=-1 || errno!=EMFILE) fail(128);
    // fd admission 必须先于 blocking FIFO rendezvous，失败不等待对端。
    alarm(5);
    errno=0; if(open(fifo,O_WRONLY)!=-1 || errno!=EMFILE) fail(129);
    alarm(0);
    for(int i=0;i<count;i++) close(copies[i]);
    errno=0; if(open("/tmp/absent/child",O_RDONLY)!=-1 || errno!=ENOENT) fail(130);
    int copy=fcntl(fd,F_DUPFD,0);
    if(copy<0 || copy!=copies[0]) fail(131);
    close(copy); close(fd); unlink(existing); unlink(fifo);
}
static void *blocked_fifo_open(void *unused)
{
    (void)unused;
    int fd=open("/tmp/open-fd-shared-fifo",O_WRONLY|O_CLOEXEC);
    return (void *)(intptr_t)fd;
}
static void shared_open_reservation(void)
{
    const char *fifo="/tmp/open-fd-shared-fifo";
    if(mkfifo(fifo,0600)) fail(140);
    int expected=fcntl(0,F_DUPFD,0);
    if(expected<0) fail(141);
    close(expected);
    pthread_t thread;
    if(pthread_create(&thread,0,blocked_fifo_open,0)) fail(142);
    // 保留 probe descriptors，避免另一个 CPU 在 probe 占位期间选择后一 fd。
    // 唯一跳过的 slot 就是 open reservation；不借助 sleep 状态猜测它是否已发布。
    int observed=0,probes[1000],count=0,next=expected;
    for(int i=0;i<1000;i++) {
        int probe=fcntl(0,F_DUPFD,0);
        if(probe<0) fail(143);
        probes[count++]=probe;
        if(probe==next+1) { expected=next; observed=1; break; }
        if(probe!=next) fail(144);
        next++;
        nanosleep(&(struct timespec){.tv_nsec=1000000},0);
    }
    for(int i=0;i<count;i++) close(probes[i]);
    if(!observed) fail(145);
    errno=0; if(fcntl(expected,F_GETFD)!=-1 || errno!=EBADF) fail(146);
    // musl dup2 会自动重试 EBUSY；此处验证一次标准 dup3 syscall 的不可见 slot 排他语义。
    errno=0; if(syscall(SYS_dup3,0,expected,0)!=-1 || errno!=EBUSY) fail(147);
    char path[64]; fd_path(expected,path,sizeof path);
    errno=0; if(open(path,O_RDONLY)!=-1 || errno!=ENOENT) fail(148);
    pid_t child=fork(); if(child<0) fail(149);
    if(!child) {
        errno=0; if(fcntl(expected,F_GETFD)!=-1 || errno!=EBADF) fail(150);
        int probe=fcntl(0,F_DUPFD,expected);
        if(probe!=expected) fail(151);
        close(probe); _exit(0);
    }
    int status; if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) fail(152);
    int reader=open(fifo,O_RDONLY|O_NONBLOCK);
    void *result;
    if(reader<0 || reader==expected || pthread_join(thread,&result)
        || (intptr_t)result!=expected || !(fcntl(expected,F_GETFD)&FD_CLOEXEC)) fail(153);
    close(reader); close(expected); unlink(fifo);
}
static void signal_descriptor(void)
{
    sigset_t mask,previous,other;
    sigemptyset(&mask); sigaddset(&mask,SIGUSR1); sigaddset(&mask,SIGUSR2);
    sigaddset(&mask,SIGRTMIN+3);
    if(pthread_sigmask(SIG_BLOCK,&mask,&previous)) fail(160);
    int fd=signalfd(-1,&mask,SFD_NONBLOCK|SFD_CLOEXEC);
    if(fd<0 || !(fcntl(fd,F_GETFL)&O_NONBLOCK) || !(fcntl(fd,F_GETFD)&FD_CLOEXEC)) fail(161);
    struct signalfd_siginfo records[2];
    if(lseek(fd,-1,SEEK_SET)!=0) fail(162);
    errno=0; if(write(fd,records,0)!=-1 || errno!=EINVAL) fail(162);
    errno=0; if(read(fd,records,0)!=-1 || errno!=EINVAL) fail(162);
    struct iovec empty={.iov_base=(void *)1,.iov_len=0};
    if(readv(fd,&empty,1)!=0 || readv(fd,NULL,0)!=0) fail(218);
    errno=0; if(read(fd,records,127)!=-1 || errno!=EINVAL) fail(163);
    errno=0; if(read(fd,records,128)!=-1 || errno!=EAGAIN) fail(164);
    int copy=dup(fd);
    sigemptyset(&other); sigaddset(&other,SIGUSR2);
    if(copy<0 || signalfd(copy,&other,SFD_CLOEXEC)!=copy || !(fcntl(fd,F_GETFL)&O_NONBLOCK)
        || (fcntl(copy,F_GETFD)&FD_CLOEXEC) || kill(getpid(),SIGUSR1)) fail(165);
    struct pollfd pollfd={.fd=fd,.events=POLLIN};
    if(poll(&pollfd,1,0)!=0 || kill(getpid(),SIGUSR2) || poll(&pollfd,1,0)!=1
        || read(copy,records,128)!=128 || records[0].ssi_signo!=SIGUSR2
        || records[0].ssi_pid!=(unsigned)getpid() || records[0].ssi_uid!=getuid()
        || records[0].ssi_code!=SI_USER) fail(166);
    if(signalfd(fd,&mask,0)!=fd || read(fd,records,128)!=128 || records[0].ssi_signo!=SIGUSR1) fail(167);
    union sigval first={.sival_ptr=(void *)(uintptr_t)0x11223344ffff0081ULL};
    union sigval second={.sival_ptr=(void *)(uintptr_t)0x5566778800000082ULL};
    if(sigqueue(getpid(),SIGRTMIN+3,first) || sigqueue(getpid(),SIGRTMIN+3,second)) fail(168);
    struct iovec vectors[2]={{records,137},{(char *)records+137,119}};
    if(readv(fd,vectors,2)!=256 || records[0].ssi_ptr!=(uintptr_t)first.sival_ptr
        || records[1].ssi_ptr!=(uintptr_t)second.sival_ptr || records[0].ssi_int!=(int)0xffff0081U
        || records[0].ssi_code!=SI_QUEUE || records[1].ssi_code!=SI_QUEUE) fail(169);
    for(size_t i=100;i<128;i++) if(((unsigned char *)&records[0])[i] || ((unsigned char *)&records[1])[i]) fail(170);
    if(kill(getpid(),SIGUSR1)) fail(171);
    errno=0; if(syscall(SYS_read,fd,1,128)!=-1 || errno!=EFAULT) fail(172);
    errno=0; if(read(fd,records,128)!=-1 || errno!=EAGAIN) fail(173);
    int epoll=epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event event={.events=EPOLLIN,.data.u64=0x1234};
    if(epoll<0 || epoll_ctl(epoll,EPOLL_CTL_ADD,fd,&event)) fail(174);
    pid_t child=fork(); if(child<0) fail(175);
    if(!child) {
        // inherited signalfd 读取 child 的真实队列；epoll ADD 的 wait queue 仍属于 parent sighand。
        if(kill(getpid(),SIGUSR1) || epoll_wait(epoll,&event,1,0)!=0
            || read(fd,records,128)!=128 || records[0].ssi_pid!=(unsigned)getpid()) fail(176);
        if(epoll_ctl(epoll,EPOLL_CTL_DEL,fd,0)) fail(177);
        event.events=EPOLLIN|EPOLLET; event.data.u64=0x5678;
        if(epoll_ctl(epoll,EPOLL_CTL_ADD,fd,&event) || kill(getpid(),SIGUSR1)
            || epoll_wait(epoll,&event,1,0)!=1 || event.data.u64!=0x5678
            || epoll_wait(epoll,&event,1,0)!=0 || read(fd,records,128)!=128
            || kill(getpid(),SIGUSR1) || epoll_wait(epoll,&event,1,0)!=1) fail(178);
        if(read(fd,records,128)!=128) fail(179);
        _exit(0);
    }
    int status;
    if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) fail(180);
    errno=0; if(read(fd,records,128)!=-1 || errno!=EAGAIN) fail(181);
    close(epoll); close(copy); close(fd);
    if(pthread_sigmask(SIG_SETMASK,&previous,0)) fail(182);
}
static void create_through_links_and_path_capabilities(void)
{
    const char *directory="/tmp/create-links", *relative="/tmp/create-links/relative", *chain="/tmp/create-links/chain";
    if(mkdir(directory,0700) || symlink("target",relative) || symlink("relative",chain)) fail(219);
    errno=0; if(open(relative,O_CREAT|O_EXCL|O_RDWR,0600)!=-1 || errno!=EEXIST) fail(220);
    errno=0; if(open(relative,O_CREAT|O_NOFOLLOW|O_RDWR,0600)!=-1 || errno!=ELOOP) fail(221);
    struct stat a,b;
    // 新文件 mode=0 仍允许本次创建者写；不能用新 inode mode 撤销 parent 的创建授权。
    int writer=open(chain,O_CREAT|O_TRUNC|O_WRONLY,0000);
    if(writer<0 || write(writer,"x",1)!=1 || fstat(writer,&a) || a.st_size!=1 || (a.st_mode&0777)!=0
        || stat("/tmp/create-links/target",&b) || a.st_dev!=b.st_dev || a.st_ino!=b.st_ino
        || lstat(relative,&b) || !S_ISLNK(b.st_mode)) fail(222);
    int path=open(relative,O_PATH|O_CLOEXEC|O_CREAT|O_TRUNC|O_WRONLY|O_NONBLOCK,0777);
    if(path<0 || fstat(path,&b) || a.st_ino!=b.st_ino || b.st_size!=1
        || fcntl(path,F_GETFL)!=O_PATH || fcntl(path,F_GETFD)!=FD_CLOEXEC) fail(223);
    errno=0; if(sendfile(writer,path,NULL,1)!=-1 || errno!=EBADF) fail(224);
    errno=0; if(sendfile(path,writer,NULL,1)!=-1 || errno!=EBADF) fail(224);
    int epoll=epoll_create1(0); struct epoll_event event={.events=EPOLLIN};
    errno=0; if(epoll<0 || epoll_ctl(epoll,EPOLL_CTL_ADD,path,&event)!=-1 || errno!=EBADF) fail(225);
    errno=0; if(open("/tmp/create-links/new/",O_CREAT|O_WRONLY,0600)!=-1 || errno!=EISDIR) fail(226);
    errno=0; if(open(directory,O_CREAT|O_RDONLY,0600)!=-1 || errno!=EISDIR) fail(226);
    errno=0; if(open("/tmp/create-links/new",O_CREAT|O_DIRECTORY|O_RDONLY,0600)!=-1 || errno!=EINVAL
        || !lstat("/tmp/create-links/new",&b) || errno!=ENOENT) fail(227);
    // 使用 libc 的 native flags，区分 AArch64 O_LARGEFILE 与 O_NOFOLLOW；不得屏蔽输入位。
    int followed=open(relative,O_RDONLY|O_LARGEFILE), dir=open(directory,O_RDONLY|O_DIRECTORY);
    if(followed<0 || dir<0 || fstat(followed,&b) || b.st_ino!=a.st_ino
        || !(fcntl(dir,F_GETFL)&O_DIRECTORY)) fail(235);
    errno=0; if(open(relative,O_RDONLY|O_NOFOLLOW)!=-1 || errno!=ELOOP) fail(236);
    errno=0; if(open("/tmp/create-links/target",O_RDONLY|O_DIRECTORY)!=-1 || errno!=ENOTDIR) fail(237);
    int link=open(relative,O_PATH|O_NOFOLLOW);
    if(link<0 || fstat(link,&b) || !S_ISLNK(b.st_mode)
        || fcntl(link,F_GETFL)!=(O_PATH|O_NOFOLLOW)) fail(238);
    close(link); close(dir); close(followed);
    close(epoll); close(path); close(writer);
    if(unlink(chain) || unlink(relative) || unlink("/tmp/create-links/target") || rmdir(directory)) fail(228);
}
static void packet_broken_pipe_has_no_stream_signal(void)
{
    sigset_t block,previous,pending; sigemptyset(&block); sigaddset(&block,SIGPIPE);
    if(sigprocmask(SIG_BLOCK,&block,&previous)) fail(229);
    for(int type=SOCK_DGRAM;type<=SOCK_SEQPACKET;type+=SOCK_SEQPACKET-SOCK_DGRAM) {
        int fd[2]; if(socketpair(AF_UNIX,type,0,fd) || shutdown(fd[0],SHUT_WR)) fail(230);
        errno=0; if(send(fd[0],"x",1,0)!=-1 || errno!=EPIPE) fail(231);
        errno=0; if(write(fd[0],"x",1)!=-1 || errno!=EPIPE) fail(232);
        if(sigpending(&pending) || sigismember(&pending,SIGPIPE)) fail(233);
        close(fd[0]); close(fd[1]);
    }
    if(sigprocmask(SIG_SETMASK,&previous,0)) fail(234);
}
int verify_anonymous_files(void)
{
    if((mkdir("/tmp",0777) && errno!=EEXIST) || (mkdir("/proc",0755) && errno!=EEXIST)
        || (mount("proc","/proc","proc",0,0) && errno!=EBUSY)) return 112;
    void (*cases[])(void)={memfd_identity,sealed_metadata_failure,creator_attributes,pipe_reopen,zero_length_descriptor_reads,anonymous_singleton,watch_real_inode,failed_truncate_open_lifecycle,explicit_file_times,open_fd_admission,shared_open_reservation,signal_descriptor,create_through_links_and_path_capabilities,packet_broken_pipe_has_no_stream_signal};
    for(size_t i=0;i<sizeof cases/sizeof *cases;i++) {
        pid_t child=fork(); if(child<0) return 110;
        if(!child) { cases[i](); _exit(0); }
        int status; if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) {
            dprintf(2,"anonymous files case=%zu status=%d\n",i,status); return 111;
        }
    }
    puts("LiteOS musl anonymous inode identity reopen metadata session and statfs ok"); return 0;
}

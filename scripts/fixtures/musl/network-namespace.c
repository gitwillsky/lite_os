#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#define NS_GET_USERNS 0xb701
#define NS_GET_NSTYPE 0xb703
#define PIDFD_GET_NET_NAMESPACE 0xff04

static int net_wait(pid_t child)
{
    int status;
    if(child<0 || waitpid(child,&status,0)!=child || !WIFEXITED(status)) return 119;
    return WEXITSTATUS(status);
}
static int net_read(int fd,char *bytes,size_t capacity)
{
    if(lseek(fd,0,SEEK_SET)<0) return -1;
    ssize_t count=read(fd,bytes,capacity-1);
    if(count<0) return -1;
    bytes[count]=0;
    return 0;
}
static int net_up(int socket)
{
    struct ifreq flags={0}; strcpy(flags.ifr_name,"lo");
    if(ioctl(socket,SIOCGIFFLAGS,&flags)) return -1;
    flags.ifr_flags|=IFF_UP;
    return ioctl(socket,SIOCSIFFLAGS,&flags);
}
static int empty_domain(void)
{
    int fd=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    if(fd<0) return 61;
    struct ifreq flags={0}; strcpy(flags.ifr_name,"lo");
    if(ioctl(fd,SIOCGIFFLAGS,&flags) || !(flags.ifr_flags&IFF_LOOPBACK)
        || (flags.ifr_flags&IFF_UP)) return 62;
    memset(&flags,0,sizeof flags); strcpy(flags.ifr_name,"lo");
    errno=0; if(ioctl(fd,SIOCGIFADDR,&flags)!=-1 || errno!=EADDRNOTAVAIL) return 63;
    memset(&flags,0,sizeof flags); strcpy(flags.ifr_name,"eth0");
    errno=0; if(ioctl(fd,SIOCGIFINDEX,&flags)!=-1 || errno!=ENODEV) return 64;
    struct ifconf list={0};
    if(ioctl(fd,SIOCGIFCONF,&list) || list.ifc_len) return 65;
    int table=open("/proc/self/net/dev",O_RDONLY); char bytes[2048];
    if(table<0 || net_read(table,bytes,sizeof bytes) || !strstr(bytes,"lo:")
        || strstr(bytes,"eth0:")) return 66;
    close(table); close(fd);
    return 0;
}
static int exchange(int fd,const struct sockaddr_in *address,const char *word)
{
    size_t count=strlen(word);
    if(sendto(fd,word,count,0,(const struct sockaddr *)address,sizeof *address)!=(ssize_t)count) return 67;
    struct pollfd ready={.fd=fd,.events=POLLIN};
    if(poll(&ready,1,1500)!=1 || !(ready.revents&POLLIN)) return 68;
    char bytes[256];
    if(recv(fd,bytes,sizeof bytes,0)!=(ssize_t)count || memcmp(bytes,word,count)) return 69;
    return 0;
}
static int abstract_bind(int fd)
{
    struct sockaddr_un address={.sun_family=AF_UNIX};
    memcpy(address.sun_path+1,"native-net-domain",17);
    return bind(fd,(const struct sockaddr *)&address,2+1+17);
}
/* OLD OFD保持创建NET，Task selector随unshare/setns变化；真实LO/UDP/abstract消息提供证据。 */
static int net_actor(void)
{
    int old_ns=open("/proc/self/ns/net",O_RDONLY|O_CLOEXEC);
    int old_table=open("/proc/self/net/dev",O_RDONLY|O_CLOEXEC);
    int old=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    int old_unix=socket(AF_UNIX,SOCK_DGRAM,0);
    if(old_ns<0 || old_table<0 || old<0 || old_unix<0) return 70;
    struct stat before,after;
    if(fstat(old_ns,&before) || ioctl(old_ns,NS_GET_NSTYPE)!=CLONE_NEWNET) return 71;
    struct sockaddr_in address={.sin_family=AF_INET,.sin_port=htons(43222),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    if(bind(old,(struct sockaddr *)&address,sizeof address) || abstract_bind(old_unix)) return 72;
    if(unshare(CLONE_NEWNET)) return 73;
    int result=empty_domain(); if(result) return result;
    int next_ns=open("/proc/self/ns/net",O_RDONLY|O_CLOEXEC);
    if(next_ns<0 || fstat(next_ns,&after) || before.st_ino==after.st_ino) return 74;
    int user=ioctl(next_ns,NS_GET_USERNS);
    if(user<0 || ioctl(user,NS_GET_NSTYPE)!=CLONE_NEWUSER) return 75;
    close(user);
    char bytes[2048];
    if(net_read(old_table,bytes,sizeof bytes) || !strstr(bytes,"eth0:")) return 76;
    struct ifreq value={0}; strcpy(value.ifr_name,"eth0");
    if(ioctl(old,SIOCGIFINDEX,&value)) return 77;
    int next=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    int next_unix=socket(AF_UNIX,SOCK_DGRAM,0);
    if(next<0 || next_unix<0 || net_up(next) || bind(next,(struct sockaddr *)&address,sizeof address)
        || abstract_bind(next_unix)) return 78;
    if((result=exchange(old,&address,"captured-old-domain"))) return result;
    errno=0; if(recv(next,bytes,sizeof bytes,0)!=-1 || errno!=EAGAIN) return 79;
    if((result=exchange(next,&address,"new-real-loopback"))) return result;
    int self=syscall(SYS_pidfd_open,getpid(),0);
    int pidfd_ns=self<0?-1:ioctl(self,PIDFD_GET_NET_NAMESPACE,0);
    struct stat actual;
    if(pidfd_ns<0 || fstat(pidfd_ns,&actual) || actual.st_ino!=after.st_ino) return 80;
    close(pidfd_ns); close(self);
    /* clone3必须准备全新实际域，而不是复制父LO的UP/地址或继承显示label。 */
    struct {uint64_t flags,pidfd,child_tid,parent_tid,exit_signal,stack,stack_size,tls,set_tid,set_tid_size,cgroup;} args={.flags=CLONE_NEWNET,.exit_signal=SIGCHLD};
    pid_t child=syscall(SYS_clone3,&args,sizeof args);
    if(!child) _exit(empty_domain());
    if((result=net_wait(child))) return result;
    if(setns(old_ns,CLONE_NEWNET)) return 81;
    int table=open("/proc/self/net/dev",O_RDONLY);
    if(table<0 || net_read(table,bytes,sizeof bytes) || !strstr(bytes,"eth0:")) return 82;
    close(table);
    if((result=exchange(next,&address,"new-ofd-after-setns"))) return result;
    if(setns(next_ns,0)) return 83;
    int current=open("/proc/self/ns/net",O_RDONLY);
    if(current<0 || fstat(current,&actual) || actual.st_ino!=after.st_ino) return 84;
    close(current);
    if(setgid(1000) || setuid(1000)) return 85;
    for(int protocol=IPPROTO_ICMP;;protocol=IPPROTO_RAW) {
        errno=0;
        if(socket(AF_INET,SOCK_RAW,protocol)!=-1 || errno!=EPERM) return 91;
        if(protocol==IPPROTO_RAW) break;
    }
    errno=0; if(setns(old_ns,CLONE_NEWNET)!=-1 || errno!=EPERM) return 86;
    errno=0; if(unshare(CLONE_NEWNET)!=-1 || errno!=EPERM) return 87;
    close(old_ns); close(next_ns); close(old_table); close(old); close(next); close(old_unix); close(next_unix);
    return 0;
}
static int net_new_user(void)
{
    int old=socket(AF_INET,SOCK_DGRAM,0);
    if(old<0 || setgid(1000) || setuid(1000) || unshare(CLONE_NEWUSER|CLONE_NEWNET)) return 88;
    int result=empty_domain(); if(result) return result;
    int own=socket(AF_INET,SOCK_DGRAM,0);
    if(own<0 || net_up(own)) return 89;
    struct ifreq flags={0}; strcpy(flags.ifr_name,"lo");
    flags.ifr_flags=IFF_UP;
    errno=0; if(ioctl(old,SIOCSIFFLAGS,&flags)!=-1 || errno!=EPERM) return 90;
    /* NET_RAW属于新NET owning UserNS，不能要求旧initial UserNS能力。 */
    for(int protocol=IPPROTO_ICMP;;protocol=IPPROTO_RAW) {
        int raw=socket(AF_INET,SOCK_RAW,protocol);
        if(raw<0) return 91;
        close(raw);
        if(protocol==IPPROTO_RAW) break;
    }
    close(own); close(old);
    return 0;
}
int verify_network_namespaces(void)
{
    pid_t child=fork();
    if(!child) _exit(net_actor());
    int result=net_wait(child);
    if(!result) {
        child=fork(); if(!child) _exit(net_new_user());
        result=net_wait(child);
    }
    if(result) dprintf(2,"network namespace native step=%d\n",result);
    else puts("LiteOS native NET namespace actual protocol loopback socket capture proc and PIDFD ownership ok");
    return result;
}

/* 真实MTU配置、LO收发和IPv4 teardown；每个actor独占NET避免改变其他guest的设备。 */
static int mtu_actor(void)
{
    if(unshare(CLONE_NEWNET)) return 101;
    int fd=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    if(fd<0 || net_up(fd)) return 102;
    struct ifreq request={0};strcpy(request.ifr_name,"lo");
    for(int test=0;test<2;test++) {
        request.ifr_mtu=test?-1:65537;
        errno=0;if(ioctl(fd,SIOCSIFMTU,&request)!=-1 || errno!=EINVAL) return 103;
    }
    request.ifr_mtu=512;
    if(ioctl(fd,SIOCSIFMTU,&request) || ioctl(fd,SIOCGIFMTU,&request) || request.ifr_mtu!=512) return 104;
    struct sockaddr_in local={.sin_family=AF_INET,.sin_addr.s_addr=htonl(INADDR_LOOPBACK),.sin_port=htons(43224)};
    if(bind(fd,(struct sockaddr *)&local,sizeof local)) return 105;
    char word[128];memset(word,'m',sizeof word-1);word[127]=0;
    int result=exchange(fd,&local,word);if(result) return result;
    request.ifr_mtu=0;
    if(ioctl(fd,SIOCSIFMTU,&request) || ioctl(fd,SIOCGIFMTU,&request) || request.ifr_mtu) return 106;
    errno=0;if(ioctl(fd,SIOCGIFADDR,&request)!=-1 || errno!=EADDRNOTAVAIL) return 107;
    struct sockaddr_in address={.sin_family=AF_INET,.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    memcpy(&request.ifr_addr,&address,sizeof address);
    errno=0;if(ioctl(fd,SIOCSIFADDR,&request)!=-1 || errno!=ENOBUFS) return 108;
    errno=0;if(sendto(fd,"gone",4,0,(struct sockaddr *)&local,sizeof local)!=-1 || errno!=ENETUNREACH) return 109;
    request.ifr_mtu=65536;if(ioctl(fd,SIOCSIFMTU,&request)) return 110;
    errno=0;if(ioctl(fd,SIOCGIFADDR,&request)!=-1 || errno!=EADDRNOTAVAIL) return 111;
    if(ioctl(fd,SIOCGIFFLAGS,&request)) return 112;
    request.ifr_flags&=~IFF_UP;if(ioctl(fd,SIOCSIFFLAGS,&request) || net_up(fd)) return 113;
    if(ioctl(fd,SIOCGIFADDR,&request)
        || ((struct sockaddr_in *)&request.ifr_addr)->sin_addr.s_addr!=htonl(INADDR_LOOPBACK)) return 114;
    result=exchange(fd,&local,"restored");close(fd);return result;
}
int verify_interface_mtu(void)
{
    pid_t child=fork();if(child==0) _exit(mtu_actor());
    int result=net_wait(child);
    if(result) dprintf(2,"native interface MTU failure step=%d\n",result);
    return result;
}

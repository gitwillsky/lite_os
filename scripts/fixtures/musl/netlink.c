#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
struct native_sockaddr_nl { uint16_t family,pad; uint32_t port,groups; };
#define NATIVE_NETLINK_KOBJECT_UEVENT 15
static int endpoint(int type)
{
    return socket(AF_NETLINK,type|SOCK_NONBLOCK|SOCK_CLOEXEC,NATIVE_NETLINK_KOBJECT_UEVENT);
}
static int name(int fd,struct native_sockaddr_nl *address)
{
    socklen_t count=sizeof *address;
    return getsockname(fd,(struct sockaddr *)address,&count) || count!=sizeof *address || address->family!=AF_NETLINK;
}
static int actor(void)
{
    int first=endpoint(SOCK_DGRAM),second=endpoint(SOCK_RAW),third=endpoint(SOCK_DGRAM);
    if(first<0 || second<0 || third<0) return 81;
    struct native_sockaddr_nl automatic={.family=AF_NETLINK,.pad=0xfeed,.groups=UINT32_MAX};
    if(bind(first,(struct sockaddr *)&automatic,sizeof automatic)
        || bind(second,(struct sockaddr *)&automatic,sizeof automatic)) return 82;
    struct native_sockaddr_nl a={0},b={0};
    if(name(first,&a) || name(second,&b) || a.port!=(uint32_t)getpid()
        || a.groups!=1 || b.groups!=1 || (int32_t)b.port > -4097 || a.port==b.port) return 83;
    a.groups=0;
    errno=0; if(bind(third,(struct sockaddr *)&a,sizeof a)!=-1 || errno!=EADDRINUSE) return 84;
    if(bind(first,(struct sockaddr *)&a,sizeof a)) return 85;
    struct native_sockaddr_nl unsubscribed={0};
    if(name(first,&unsubscribed) || unsubscribed.port!=a.port || unsubscribed.groups) return 86;
    errno=0; if(bind(first,(struct sockaddr *)&automatic,sizeof automatic)!=-1 || errno!=EINVAL) return 87;
    int small=0; if(setsockopt(first,SOL_SOCKET,SO_RCVBUF,&small,sizeof small)) return 88;
    socklen_t count=sizeof small;
    if(getsockopt(first,SOL_SOCKET,SO_RCVBUF,&small,&count) || count!=sizeof small || small<2048) return 89;
    int ns=open("/proc/self/ns/net",O_RDONLY|O_CLOEXEC);
    if(ns<0 || unshare(CLONE_NEWNET)) return 90;
    int isolated=endpoint(SOCK_DGRAM);
    if(isolated<0 || bind(isolated,(struct sockaddr *)&a,sizeof a)) return 91;
    // 旧OFD仍属于旧NET；切换Task NET不能消除它与原NET的端口碰撞。
    errno=0; if(bind(third,(struct sockaddr *)&a,sizeof a)!=-1 || errno!=EADDRINUSE) return 92;
    if(setns(ns,CLONE_NEWNET)) return 93;
    close(first);
    if(bind(third,(struct sockaddr *)&a,sizeof a)) return 94;
    close(third);close(second);close(isolated);close(ns);
    return 0;
}
int verify_netlink_native(void)
{
    pid_t child=fork(); if(child<0) return 95;
    if(child==0) _exit(actor());
    int status; if(waitpid(child,&status,0)!=child || !WIFEXITED(status)) return 96;
    int result=WEXITSTATUS(status);
    if(result) dprintf(2,"native netlink scope/bind failure step=%d\n",result);
    return result;
}

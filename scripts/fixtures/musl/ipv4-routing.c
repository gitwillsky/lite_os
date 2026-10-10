#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <net/route.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
_Static_assert(sizeof(struct rtentry)==120,"native LP64 rtentry");
static void routing_address(struct sockaddr *output,const char *text)
{
    struct sockaddr_in value={.sin_family=AF_INET};
    inet_pton(AF_INET,text,&value.sin_addr);
    memcpy(output,&value,sizeof value);
}
static void routing_request(struct rtentry *route,const char *destination,const char *mask,unsigned short flags)
{
    memset(route,0,sizeof *route); route->rt_flags=flags; route->rt_dev="lo";
    routing_address(&route->rt_dst,destination); routing_address(&route->rt_genmask,mask);
}
static int routing_table(int fd,char *output,size_t capacity)
{
    if(lseek(fd,0,SEEK_SET)<0) return -1;
    ssize_t length=read(fd,output,capacity-1);
    if(length<0) return -1;
    output[length]=0; return 0;
}
static int routing_actor(void)
{
    if(unshare(CLONE_NEWNET)) return 61;
    int socket_fd=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    struct ifreq device={0}; strcpy(device.ifr_name,"lo"); device.ifr_flags=IFF_UP;
    if(socket_fd<0 || ioctl(socket_fd,SIOCSIFFLAGS,&device)) return 62;
    int table=open("/proc/self/net/route",O_RDONLY); char text[4096];
    if(table<0 || routing_table(table,text,sizeof text) || strstr(text,"lo\t")) return 63;
    struct rtentry wide,narrow,host;
    routing_request(&wide,"172.16.0.0","255.255.0.0",RTF_UP); wide.rt_metric=8;
    routing_request(&narrow,"172.16.8.0","255.255.255.0",RTF_UP); narrow.rt_metric=101;
    narrow.rt_flags |= RTF_MTU|RTF_WINDOW|RTF_IRTT; narrow.rt_mtu=436; narrow.rt_window=1234; narrow.rt_irtt=57;
    routing_request(&host,"203.0.113.9","0.0.0.0",RTF_UP|RTF_HOST|RTF_REJECT);
    if(ioctl(socket_fd,SIOCADDRT,&wide) || ioctl(socket_fd,SIOCADDRT,&narrow) || ioctl(socket_fd,SIOCADDRT,&host)) return 64;
    errno=0; if(ioctl(socket_fd,SIOCADDRT,&wide)!=-1 || errno!=EEXIST) return 65;
    if(routing_table(table,text,sizeof text) || !strstr(text,"000010AC") || !strstr(text,"000810AC")
        || !strstr(text,"097100CB") || !strstr(text,"\t0\t0\t7\t") || !strstr(text,"\t0\t0\t100\t") || !strstr(text,"\t436\t1234\t57\n")) return 66;
    struct sockaddr_in blocked={.sin_family=AF_INET,.sin_port=htons(44333)};
    inet_pton(AF_INET,"203.0.113.9",&blocked.sin_addr);
    errno=0; if(sendto(socket_fd,"x",1,0,(struct sockaddr *)&blocked,sizeof blocked)!=-1 || errno!=EHOSTUNREACH) return 67;
    if(ioctl(socket_fd,SIOCDELRT,&host)) return 68;
    errno=0; if(ioctl(socket_fd,SIOCDELRT,&host)!=-1 || errno!=ESRCH) return 69;
    struct rtentry invalid;
    routing_request(&invalid,"172.16.8.1","255.255.255.0",RTF_UP);
    errno=0; if(ioctl(socket_fd,SIOCADDRT,&invalid)!=-1 || errno!=EINVAL) return 70;
    routing_request(&invalid,"172.16.8.0","255.0.255.0",RTF_UP);
    errno=0; if(ioctl(socket_fd,SIOCADDRT,&invalid)!=-1 || errno!=EINVAL) return 71;
    routing_request(&invalid,"172.16.8.0","255.255.255.0",RTF_UP); invalid.rt_dst.sa_family=AF_UNIX;
    errno=0; if(ioctl(socket_fd,SIOCADDRT,&invalid)!=-1 || errno!=EAFNOSUPPORT) return 72;
    routing_request(&invalid,"0.0.0.0","0.0.0.0",RTF_UP|RTF_GATEWAY);
    errno=0; if(ioctl(socket_fd,SIOCADDRT,&invalid)!=-1 || errno!=EINVAL) return 73;
    // 重置Task域不能重定向已捕获socket或proc route OFD。
    if(unshare(CLONE_NEWNET)) return 74;
    int other=open("/proc/self/net/route",O_RDONLY);
    if(other<0 || routing_table(other,text,sizeof text) || strstr(text,"lo\t")) return 75;
    if(routing_table(table,text,sizeof text) || !strstr(text,"000010AC")) return 76;
    if(ioctl(socket_fd,SIOCDELRT,&wide)) return 77;
    if(routing_table(table,text,sizeof text) || strstr(text,"000010AC") || !strstr(text,"000810AC")) return 78;
    memset(&device,0,sizeof device); strcpy(device.ifr_name,"lo");
    if(ioctl(socket_fd,SIOCSIFFLAGS,&device)) return 79;
    if(routing_table(table,text,sizeof text) || strstr(text,"000810AC")) return 80;
    device.ifr_flags=IFF_UP;
    if(ioctl(socket_fd,SIOCSIFFLAGS,&device) || routing_table(table,text,sizeof text) || strstr(text,"000810AC")) return 81;
    errno=0; if(ioctl(socket_fd,SIOCDELRT,&narrow)!=-1 || errno!=ESRCH) return 82;
    close(other); close(table); close(socket_fd); return 0;
}
int verify_ipv4_routing(void)
{
    pid_t child=fork();
    if(child==0) _exit(routing_actor());
    int status=0;
    if(child<0 || waitpid(child,&status,0)!=child || !WIFEXITED(status)) return 119;
    return WEXITSTATUS(status);
}

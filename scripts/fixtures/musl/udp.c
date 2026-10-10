#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

int verify_network_namespaces(void);
int verify_netlink_native(void);
int verify_interface_mtu(void);
int verify_ipv4_routing(void);

static struct sockaddr_in local_address(int fd)
{
    struct sockaddr_in result={0}; socklen_t size=sizeof result;
    if(getsockname(fd,(struct sockaddr *)&result,&size) || size!=sizeof result) result.sin_family=0;
    return result;
}
static int configure(int fd,unsigned long request,const struct sockaddr_in *address)
{
    struct ifreq value={0}; strcpy(value.ifr_name,"eth0");
    memcpy(&value.ifr_addr,address,sizeof *address);
    return ioctl(fd,request,&value);
}
static int bindings(const struct sockaddr_in *peer,uint32_t address)
{
    for(int mode=0;mode<4;mode++) {
        int fd=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0); if(fd<0) return 235;
        struct sockaddr_in local={.sin_family=AF_INET};
        if(mode!=0) {
            local.sin_addr.s_addr=mode>=2?address:INADDR_ANY;
            local.sin_port=mode==3?0:htons(43500+mode);
            if(bind(fd,(struct sockaddr *)&local,sizeof local)) return 236;
        }
        if(connect(fd,(const struct sockaddr *)peer,sizeof *peer)) return 237;
        struct sockaddr_in before=local_address(fd);
        if(before.sin_family!=AF_INET || before.sin_addr.s_addr!=address || !before.sin_port) return 238;
        struct sockaddr cancel={.sa_family=AF_UNSPEC};
        if(connect(fd,&cancel,sizeof cancel)) return 239;
        struct sockaddr_in after=local_address(fd);
        if(after.sin_family!=AF_INET || after.sin_addr.s_addr!=(mode>=2?address:INADDR_ANY)
            || after.sin_port!=((mode==1 || mode==2)?local.sin_port:0)) return 240;
        socklen_t size=sizeof local; errno=0;
        if(getpeername(fd,(struct sockaddr *)&local,&size)!=-1 || errno!=ENOTCONN) return 241;
        int other=socket(AF_INET,SOCK_DGRAM,0); if(other<0) return 242;
        local=before; errno=0; int result=bind(other,(struct sockaddr *)&local,sizeof local);
        if((mode==0 || mode==3)?result!=0:(result!=-1 || errno!=EADDRINUSE)) return 243;
        close(other); close(fd);
    }
    return 0;
}
static int shutdown_policy(const struct sockaddr_in *peer)
{
    sigset_t block,previous,pending; sigemptyset(&block); sigaddset(&block,SIGPIPE);
    if(sigprocmask(SIG_BLOCK,&block,&previous)) return 244;
    int fd=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0); if(fd<0) return 245;
    errno=0; if(shutdown(fd,SHUT_RD)!=-1 || errno!=ENOTCONN) return 246;
    struct pollfd ready={.fd=fd,.events=POLLIN|POLLOUT|POLLRDHUP};
    if(poll(&ready,1,0)!=1 || !(ready.revents&POLLIN) || !(ready.revents&POLLRDHUP)
        || (ready.revents&POLLHUP)) return 247;
    char byte; struct sockaddr_in source; memset(&source, 0xa5, sizeof source);
    struct iovec vector={&byte,1};
    struct msghdr message={.msg_name=&source,.msg_namelen=sizeof source,.msg_iov=&vector,.msg_iovlen=1};
    ssize_t received = recvmsg(fd, &message, MSG_DONTWAIT);
    if (received != 0 || message.msg_namelen != 0) {
        dprintf(2, "UDP shutdown recv=%zd namelen=%u errno=%d\n", received, message.msg_namelen, errno);
        return 248;
    }
    for (size_t i = 0; i < sizeof source; ++i)
        if (((unsigned char *)&source)[i] != 0xa5) return 248;
    if(connect(fd,(const struct sockaddr *)peer,sizeof *peer) || shutdown(fd,SHUT_WR)) return 249;
    ready.revents=0;
    if(poll(&ready,1,0)!=1 || !(ready.revents&POLLHUP)) return 250;
    errno=0; if(send(fd,"x",1,0)!=-1 || errno!=EPIPE) return 251;
    errno=0; if(write(fd,"x",1)!=-1 || errno!=EPIPE) return 251;
    if(sigpending(&pending) || sigismember(&pending,SIGPIPE)) return 252;
    struct sockaddr cancel={.sa_family=AF_UNSPEC};
    if(connect(fd,&cancel,sizeof cancel) || connect(fd,(const struct sockaddr *)peer,sizeof *peer)) return 253;
    errno=0; if(send(fd,"x",1,MSG_NOSIGNAL)!=-1 || errno!=EPIPE) return 254;
    close(fd); if(sigprocmask(SIG_SETMASK,&previous,0)) return 255;
    return 0;
}
/* 原生 RAW255 是真正的 send-only HDRINCL endpoint；接收由独立 ICMP socket 完成。 */
static int native_raw_header(void)
{
    int sender=socket(AF_INET,SOCK_RAW|SOCK_NONBLOCK,IPPROTO_RAW);
    int receiver=socket(AF_INET,SOCK_RAW|SOCK_NONBLOCK,IPPROTO_ICMP);
    if(sender<0 || receiver<0) return 187;
    struct sockaddr_in peer={.sin_family=AF_INET,.sin_port=htons(123)};
    peer.sin_addr.s_addr=htonl(INADDR_LOOPBACK);
    int value=0; socklen_t length=sizeof value;
    if(getsockopt(sender,IPPROTO_IP,IP_HDRINCL,&value,&length) || value!=1 || length!=4) return 188;
    length=sizeof value;
    if(getsockopt(receiver,IPPROTO_IP,IP_HDRINCL,&value,&length) || value!=0) return 188;
    for(int included=0;included<=1;included++) {
        if(setsockopt(sender,IPPROTO_IP,IP_HDRINCL,&included,sizeof included)) return 188;
        value=-1; length=sizeof value;
        if(getsockopt(sender,IPPROTO_IP,IP_HDRINCL,&value,&length) || value!=included) return 188;
    }
    /* 错误 total length/checksum 必须由发送 codec 填充；零 source/ID 才由内核选择。 */
    unsigned char packet[28]={0x45,0x10,0xff,0xff,0x12,0x34,0,0,37,IPPROTO_ICMP,0xa5,0xa5,
        0,0,0,0,127,0,0,1,8,0,0xf7,0xff,0,0,0,0};
    for(int generated=0;generated<2;generated++) {
        if(generated) packet[4]=packet[5]=0;
        unsigned char original[sizeof packet]; memcpy(original,packet,sizeof packet);
        if(sendto(sender,packet,sizeof packet,0,(struct sockaddr *)&peer,sizeof peer)!=sizeof packet
            || memcmp(packet,original,sizeof packet)) return 189;
        int seen=0;
        for(int attempt=0;attempt<20 && !seen;attempt++) {
            struct pollfd ready={.fd=receiver,.events=POLLIN};
            if(poll(&ready,1,100)<0) return 190;
            if(!(ready.revents&POLLIN)) continue;
            unsigned char received[128];
            ssize_t count=recv(receiver,received,sizeof received,MSG_DONTWAIT);
            if(count<0 && errno==EAGAIN) continue;
            if(count<28 || received[20]!=8) continue;
            unsigned sum=0; for(int i=0;i<20;i+=2) sum+=(received[i]<<8)|received[i+1];
            while(sum>>16) sum=(sum&0xffff)+(sum>>16);
            if(count!=28 || received[0]!=0x45 || received[1]!=0x10 || received[2]!=0 || received[3]!=28
                || (generated?!(received[4]|received[5]):(received[4]!=0x12 || received[5]!=0x34))
                || received[6]!=0 || received[7]!=0 || received[8]!=37 || received[9]!=IPPROTO_ICMP
                || sum!=0xffff || memcmp(received+12,"\177\0\0\1",4)
                || memcmp(received+16,&peer.sin_addr,4) || memcmp(received+20,packet+20,8)) return 191;
            seen=1;
        }
        if(!seen) return 190;
    }
    struct pollfd ready={.fd=sender,.events=POLLIN};
    if(poll(&ready,1,0)!=0) return 192;
    unsigned char output[64]; errno=0;
    if(recv(sender,output,sizeof output,MSG_DONTWAIT)!=-1 || errno!=EAGAIN) return 192;
    /* 未开放的 IP options/分片形态在入队之前报错，不接受后丢包。 */
    packet[0]=0x46; errno=0;
    if(sendto(sender,packet,sizeof packet,0,(struct sockaddr *)&peer,sizeof peer)!=-1 || errno!=EOPNOTSUPP) return 193;
    packet[0]=0x45; packet[6]=0x20; errno=0;
    if(sendto(sender,packet,sizeof packet,0,(struct sockaddr *)&peer,sizeof peer)!=-1 || errno!=EOPNOTSUPP) return 193;
    packet[6]=0; packet[0]=0x65; errno=0;
    if(sendto(sender,packet,sizeof packet,0,(struct sockaddr *)&peer,sizeof peer)!=-1 || errno!=EINVAL) return 193;
    close(receiver); close(sender);
    errno=0;
    if(socket(AF_INET,SOCK_RAW,2)!=-1 || errno!=EPROTONOSUPPORT) return 194;
    puts("LiteOS musl RAW255 header ownership and send only delivery ok");
    return 0;
}

/* 真实 protocol policy 的标准 option ABI；IP scalar 使用独立 native 短值/copyout 顺序。 */
static int native_options(void)
{
    int kinds[] = { SOCK_STREAM, SOCK_DGRAM, SOCK_RAW };
    for (size_t kind = 0; kind < sizeof kinds / sizeof kinds[0]; ++kind) {
        int fd = socket(AF_INET, kinds[kind], kinds[kind] == SOCK_RAW ? IPPROTO_ICMP : 0);
        if (fd < 0) return 35;
        int value = 0; socklen_t length = sizeof value;
        if (getsockopt(fd, IPPROTO_IP, IP_TTL, &value, &length) || value != 64 || length != 4) return 36;
        unsigned char short_value[3] = {37, 0xff, 0xff};
        if (setsockopt(fd, IPPROTO_IP, IP_TTL, short_value, sizeof short_value)) return 37;
        unsigned char result[4] = {0xa5,0xa5,0xa5,0xa5}; length = 3;
        if (getsockopt(fd, IPPROTO_IP, IP_TTL, result, &length) || length != 1
            || result[0] != 37 || result[1] != 0xa5 || result[2] != 0xa5) return 38;
        length = 3; errno = 0;
        if (getsockopt(fd, IPPROTO_IP, IP_TTL, (void *)1, &length) != -1 || errno != EFAULT || length != 1) return 39;
        value = -1;
        if (setsockopt(fd, IPPROTO_IP, IP_TTL, &value, sizeof value)) return 40;
        length = sizeof value;
        if (getsockopt(fd, IPPROTO_IP, IP_TTL, &value, &length) || value != 64) return 41;
        value = 0; errno = 0;
        if (setsockopt(fd, IPPROTO_IP, IP_TTL, &value, sizeof value) != -1 || errno != EINVAL) return 42;
        errno = 0;
        if (setsockopt(fd, IPPROTO_IP, IP_TTL, (void *)1, 0) != -1 || errno != EINVAL) return 43;
        int boolean_options[] = { SO_REUSEADDR, SO_BROADCAST, SO_KEEPALIVE };
        for (size_t i=0; i<sizeof boolean_options/sizeof boolean_options[0]; ++i) {
            value=1; if (setsockopt(fd,SOL_SOCKET,boolean_options[i],&value,sizeof value)) return 44;
            value=0; length=sizeof value;
            if (getsockopt(fd,SOL_SOCKET,boolean_options[i],&value,&length) || value!=1 || length!=4) return 45;
        }
        value=1;
        if (setsockopt(fd,IPPROTO_IP,IP_PKTINFO,&value,sizeof value)) return 46;
        value=0; length=sizeof value;
        if (getsockopt(fd,IPPROTO_IP,IP_PKTINFO,&value,&length) || value!=1) return 47;
        if (setsockopt(fd,IPPROTO_IP,IP_PKTINFO,(void *)1,0)) return 48;
        value=1; length=sizeof value;
        if (getsockopt(fd,IPPROTO_IP,IP_PKTINFO,&value,&length) || value) return 49;
        if (kinds[kind]==SOCK_STREAM) {
            int timers[][3]={{TCP_KEEPIDLE,7200,3},{TCP_KEEPINTVL,75,2},{TCP_KEEPCNT,9,4}};
            for(size_t timer=0;timer<sizeof timers/sizeof timers[0];timer++) {
                value=0; length=sizeof value;
                if(getsockopt(fd,IPPROTO_TCP,timers[timer][0],&value,&length) || value!=timers[timer][1]) return 154;
                value=timers[timer][2];
                if(setsockopt(fd,IPPROTO_TCP,timers[timer][0],&value,sizeof value)) return 155;
                value=0; length=sizeof value;
                if(getsockopt(fd,IPPROTO_TCP,timers[timer][0],&value,&length) || value!=timers[timer][2]) return 156;
                int bad[]={0,-1,timers[timer][0]==TCP_KEEPCNT?128:32768};
                for(size_t entry=0;entry<sizeof bad/sizeof bad[0];entry++) {
                    errno=0;
                    if(setsockopt(fd,IPPROTO_TCP,timers[timer][0],&bad[entry],sizeof(int))!=-1 || errno!=EINVAL) return 157;
                }
                errno=0;
                if(setsockopt(fd,IPPROTO_TCP,timers[timer][0],(void *)1,3)!=-1 || errno!=EINVAL) return 158;
                errno=0;
                if(setsockopt(fd,IPPROTO_TCP,timers[timer][0],(void *)1,4)!=-1 || errno!=EFAULT) return 159;
            }
            int retry_options[][3]={{TCP_SYNCNT,6,2},{TCP_USER_TIMEOUT,0,2500}};
            for(size_t retry=0;retry<sizeof retry_options/sizeof retry_options[0];retry++) {
                value=0; length=sizeof value;
                if(getsockopt(fd,IPPROTO_TCP,retry_options[retry][0],&value,&length) || value!=retry_options[retry][1]) return 160;
                value=retry_options[retry][2];
                if(setsockopt(fd,IPPROTO_TCP,retry_options[retry][0],&value,sizeof value)) return 161;
                value=0; length=sizeof value;
                if(getsockopt(fd,IPPROTO_TCP,retry_options[retry][0],&value,&length) || value!=retry_options[retry][2]) return 162;
                value=-1; errno=0;
                if(setsockopt(fd,IPPROTO_TCP,retry_options[retry][0],&value,sizeof value)!=-1 || errno!=EINVAL) return 163;
            }
            value=0; length=sizeof value;
            if(getsockopt(fd,IPPROTO_TCP,TCP_LINGER2,&value,&length) || value!=60) return 164;
            int linger_values[][2]={{-2,-1},{1,1},{121,120},{0,60}};
            for(size_t entry=0;entry<sizeof linger_values/sizeof linger_values[0];entry++) {
                value=linger_values[entry][0];
                if(setsockopt(fd,IPPROTO_TCP,TCP_LINGER2,&value,sizeof value)) return 165;
                value=0; length=sizeof value;
                if(getsockopt(fd,IPPROTO_TCP,TCP_LINGER2,&value,&length) || value!=linger_values[entry][1]) return 166;
            }
            value=1; if (setsockopt(fd,IPPROTO_TCP,TCP_NODELAY,&value,sizeof value)) return 50;
            value=0; length=sizeof value;
            if (getsockopt(fd,IPPROTO_TCP,TCP_NODELAY,&value,&length) || value!=1) return 51;
            length=8; errno=0;
            if (getsockopt(fd,IPPROTO_TCP,TCP_NODELAY,(void *)1,&length)!=-1 || errno!=EFAULT || length!=4) return 58;
            value=0; length=sizeof value;
            if (getsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&value,&length) || value!=536 || length!=4) return 59;
            value=512;
            if (setsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&value,sizeof value)) return 60;
            int invalid_mss[] = {-1, 1, 87, 32768};
            for (size_t i=0;i<sizeof invalid_mss/sizeof invalid_mss[0];i++) {
                errno=0;
                if (setsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&invalid_mss[i],sizeof(int))!=-1 || errno!=EINVAL) return 61;
            }
            value=0; length=sizeof value;
            if (getsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&value,&length) || value!=512) return 62;
            unsigned char truncated[4]={0xa5,0xa5,0xa5,0xa5}; length=3;
            if (getsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,truncated,&length) || length!=3
                || truncated[0]!=0 || truncated[1]!=2 || truncated[2]!=0 || truncated[3]!=0xa5) return 63;
            value=0;
            if (setsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&value,sizeof value)) return 64;
            length=sizeof value;
            if (getsockopt(fd,IPPROTO_TCP,TCP_MAXSEG,&value,&length) || value!=536) return 65;
        } else {
            value=1; errno=0;
            if (setsockopt(fd,IPPROTO_TCP,TCP_NODELAY,&value,sizeof value)!=-1 || errno!=ENOPROTOOPT) return 52;
        }
        if (setsockopt(fd,SOL_SOCKET,SO_BINDTODEVICE,"eth0",5)) return 53;
        char name[IFNAMSIZ]; length=IFNAMSIZ-1; errno=0;
        if (getsockopt(fd,SOL_SOCKET,SO_BINDTODEVICE,name,&length)!=-1 || errno!=EINVAL) return 54;
        length=sizeof name;
        if (getsockopt(fd,SOL_SOCKET,SO_BINDTODEVICE,name,&length) || length!=5 || strcmp(name,"eth0")) return 55;
        if (setsockopt(fd,SOL_SOCKET,SO_BINDTODEVICE,NULL,0)) return 56;
        length=0;
        if (getsockopt(fd,SOL_SOCKET,SO_BINDTODEVICE,(void *)1,&length) || length) return 57;
        close(fd);
    }
    return 0;
}

/* 标准lo请求只改变当前报文source/oif；接收控制信息来自实际ingress。 */
static int native_pktinfo(void)
{
    unsigned index=if_nametoindex("lo"); if(!index) return 70;
    int receiver=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    int sender=socket(AF_INET,SOCK_DGRAM|SOCK_NONBLOCK,0);
    if(receiver<0 || sender<0) return 71;
    int enabled=1;
    if(setsockopt(receiver,IPPROTO_IP,IP_PKTINFO,&enabled,sizeof enabled)) return 72;
    struct sockaddr_in target={.sin_family=AF_INET,.sin_port=htons(43510)};
    target.sin_addr.s_addr=htonl(INADDR_LOOPBACK);
    if(bind(receiver,(struct sockaddr *)&target,sizeof target)) return 73;
    union {struct cmsghdr alignment; unsigned char bytes[CMSG_SPACE(sizeof(struct in_pktinfo))];} control={0};
    char byte='p'; struct iovec vector={.iov_base=&byte,.iov_len=1};
    struct msghdr message={.msg_name=&target,.msg_namelen=sizeof target,
        .msg_iov=&vector,.msg_iovlen=1,.msg_control=control.bytes,.msg_controllen=sizeof control.bytes};
    struct cmsghdr *header=CMSG_FIRSTHDR(&message);
    header->cmsg_level=IPPROTO_IP; header->cmsg_type=IP_PKTINFO; header->cmsg_len=CMSG_LEN(sizeof(struct in_pktinfo));
    struct in_pktinfo request={.ipi_ifindex=(int)index};
    inet_pton(AF_INET,"127.0.0.2",&request.ipi_spec_dst);
    memcpy(CMSG_DATA(header),&request,sizeof request);
    if(sendmsg(sender,&message,0)!=1) return 74;
    struct pollfd wait={.fd=receiver,.events=POLLIN};
    if(poll(&wait,1,1000)!=1 || !(wait.revents&POLLIN)) return 75;
    struct sockaddr_in source={0};
    memset(control.bytes,0,sizeof control.bytes);
    message.msg_name=&source; message.msg_namelen=sizeof source; message.msg_controllen=sizeof control.bytes;
    if(recvmsg(receiver,&message,0)!=1 || byte!='p' || source.sin_addr.s_addr!=request.ipi_spec_dst.s_addr) return 76;
    header=CMSG_FIRSTHDR(&message);
    if(!header || header->cmsg_level!=IPPROTO_IP || header->cmsg_type!=IP_PKTINFO
        || header->cmsg_len!=CMSG_LEN(sizeof(struct in_pktinfo))) return 77;
    struct in_pktinfo received; memcpy(&received,CMSG_DATA(header),sizeof received);
    if(received.ipi_ifindex!=(int)index || received.ipi_spec_dst.s_addr!=target.sin_addr.s_addr
        || received.ipi_addr.s_addr!=target.sin_addr.s_addr) return 78;
    /* 未附cmsg的下一帧恢复默认source；不能持久改变socket binding。 */
    if(sendto(sender,"d",1,0,(struct sockaddr *)&target,sizeof target)!=1) return 79;
    if(poll(&wait,1,1000)!=1) return 80;
    socklen_t size=sizeof source;
    if(recvfrom(receiver,&byte,1,0,(struct sockaddr *)&source,&size)!=1
        || byte!='d' || source.sin_addr.s_addr!=target.sin_addr.s_addr) return 81;
    memset(control.bytes,0,sizeof control.bytes);
    message.msg_name=&target; message.msg_namelen=sizeof target; message.msg_controllen=sizeof control.bytes;
    header=CMSG_FIRSTHDR(&message); header->cmsg_level=IPPROTO_IP; header->cmsg_type=IP_PKTINFO;
    header->cmsg_len=CMSG_LEN(sizeof(struct in_pktinfo));
    request.ipi_ifindex=INT32_MAX; request.ipi_spec_dst.s_addr=0;
    memcpy(CMSG_DATA(header),&request,sizeof request); errno=0;
    if(sendmsg(sender,&message,0)!=-1 || errno!=ENODEV) return 82;
    request.ipi_ifindex=(int)index;
    inet_pton(AF_INET,"192.0.2.1",&request.ipi_spec_dst);
    memcpy(CMSG_DATA(header),&request,sizeof request); errno=0;
    if(sendmsg(sender,&message,0)!=-1 || errno!=ENETUNREACH) return 83;
    close(sender); close(receiver); return 0;
}

/* devinet_ioctl 使用真实 ifa 记录：首次classful，显式broadcast在mask变化后保留。 */
static int native_interface_address(int control)
{
    struct sockaddr_in local={.sin_family=AF_INET},mask={.sin_family=AF_INET},broadcast={.sin_family=AF_INET};
    inet_pton(AF_INET,"172.19.3.15",&local.sin_addr);
    if(configure(control,SIOCSIFADDR,&local)) return 175;
    struct ifreq value={0}; strcpy(value.ifr_name,"eth0");
    if(ioctl(control,SIOCGIFNETMASK,&value) || ((struct sockaddr_in *)&value.ifr_netmask)->sin_addr.s_addr!=htonl(0xffff0000)) return 176;
    inet_pton(AF_INET,"172.19.3.99",&broadcast.sin_addr);
    inet_pton(AF_INET,"255.255.255.0",&mask.sin_addr);
    if(configure(control,SIOCSIFBRDADDR,&broadcast) || configure(control,SIOCSIFNETMASK,&mask)
        || configure(control,SIOCSIFADDR,&local)) return 177;
    memset(&value,0,sizeof value); strcpy(value.ifr_name,"eth0");
    if(ioctl(control,SIOCGIFBRDADDR,&value) || ((struct sockaddr_in *)&value.ifr_broadaddr)->sin_addr.s_addr!=broadcast.sin_addr.s_addr) return 178;
    memset(&value,0,sizeof value); strcpy(value.ifr_name,"eth0");
    if(ioctl(control,SIOCGIFNETMASK,&value) || ((struct sockaddr_in *)&value.ifr_netmask)->sin_addr.s_addr!=mask.sin_addr.s_addr) return 179;
    memset(&value,0,sizeof value); strcpy(value.ifr_name,"lo");
    if(ioctl(control,SIOCGIFBRDADDR,&value) || ((struct sockaddr_in *)&value.ifr_broadaddr)->sin_addr.s_addr) return 180;
    struct ifreq invalid={0}; strcpy(invalid.ifr_name,"missing"); invalid.ifr_addr.sa_family=AF_UNIX;
    errno=0; if(ioctl(control,SIOCSIFADDR,&invalid)!=-1 || errno!=EINVAL) return 181;
    local.sin_addr.s_addr=0;
    if(configure(control,SIOCSIFADDR,&local)) return 182;
    for(unsigned long request=SIOCGIFADDR;;request=SIOCGIFNETMASK) {
        memset(&value,0,sizeof value); strcpy(value.ifr_name,"eth0");
        errno=0; if(ioctl(control,request,&value)!=-1 || errno!=EADDRNOTAVAIL) return 183;
        if(request==SIOCGIFNETMASK) break;
    }
    return 0;
}
/* native proc_net: root 是 self/net symlink，task selector 与实际 NET 文件共享 inode。 */
static int native_proc_network(void)
{
    char link[16]={0};
    ssize_t count=readlink("/proc/net",link,sizeof link);
    if(count!=8 || memcmp(link,"self/net",8)) return 187;
    struct stat root,alias,task;
    if(lstat("/proc/net",&root) || !S_ISLNK(root.st_mode)) return 188;
    if(stat("/proc/net/dev",&root) || stat("/proc/self/net/dev",&alias)
        || root.st_dev!=alias.st_dev || root.st_ino!=alias.st_ino) return 189;
    char name[96]; snprintf(name,sizeof name,"/proc/self/task/%ld/net/dev",syscall(SYS_gettid));
    if(stat(name,&task) || task.st_dev!=alias.st_dev || task.st_ino!=alias.st_ino) return 190;
    int fd=open(name,O_RDONLY); if(fd<0) return 191;
    char bytes[4096]; ssize_t length=read(fd,bytes,sizeof bytes-1); close(fd);
    if(length<0) return 192;
    bytes[length]=0;
    if(!strstr(bytes,"lo:") || !strstr(bytes,"eth0:")) return 193;
    errno=0; fd=open("/proc/self/net/dev",O_RDWR);
    if(fd>=0 || errno!=EACCES) {if(fd>=0) close(fd);return 194;}
    return 0;
}
int verify_udp_native(void)
{
    int control=socket(AF_INET,SOCK_DGRAM,0); if(control<0) return 235;
    struct ifreq saved_address={0},saved_mask={0},saved_flags={0},saved_broadcast={0};
    int had_address=1;
    strcpy(saved_address.ifr_name,"eth0"); strcpy(saved_mask.ifr_name,"eth0"); strcpy(saved_flags.ifr_name,"eth0");
    if(ioctl(control,SIOCGIFADDR,&saved_address)) {
        if(errno!=EADDRNOTAVAIL) return 236;
        had_address=0;
        ((struct sockaddr_in *)&saved_address.ifr_addr)->sin_family=AF_INET;
    }
    if(ioctl(control,SIOCGIFNETMASK,&saved_mask)) {
        if(errno!=EADDRNOTAVAIL) return 236;
        ((struct sockaddr_in *)&saved_mask.ifr_netmask)->sin_family=AF_INET;
    }
    if(ioctl(control,SIOCGIFFLAGS,&saved_flags)) return 236;
    strcpy(saved_broadcast.ifr_name,"eth0");
    if(had_address && ioctl(control,SIOCGIFBRDADDR,&saved_broadcast)) return 236;
    int result=native_interface_address(control);
    if(result) return result;
    struct sockaddr_in address={.sin_family=AF_INET},mask={.sin_family=AF_INET},peer={.sin_family=AF_INET,.sin_port=htons(44000)};
    inet_pton(AF_INET,"10.0.2.15",&address.sin_addr); inet_pton(AF_INET,"255.255.255.0",&mask.sin_addr);
    inet_pton(AF_INET,"10.0.2.16",&peer.sin_addr);
    struct ifreq up=saved_flags; up.ifr_flags|=IFF_UP;
    if(configure(control,SIOCSIFADDR,&address) || configure(control,SIOCSIFNETMASK,&mask) || ioctl(control,SIOCSIFFLAGS,&up)) return 237;
    result=native_options();
    if(!result) result=native_raw_header();
    if(!result) result=native_pktinfo();
    if(!result) result=native_proc_network();
    if(!result) result=verify_network_namespaces();
    if(!result) result=verify_netlink_native();
    if(!result) result=verify_interface_mtu();
    if(!result) result=verify_ipv4_routing();
    if(!result) result=bindings(&peer,address.sin_addr.s_addr);
    if(!result) result=shutdown_policy(&peer);
    if(ioctl(control,SIOCSIFADDR,&saved_address)
        || (had_address && (ioctl(control,SIOCSIFNETMASK,&saved_mask) || ioctl(control,SIOCSIFBRDADDR,&saved_broadcast)))
        || ioctl(control,SIOCSIFFLAGS,&saved_flags)) return 238;
    close(control);
    if(result) {dprintf(2,"UDP native policy failure step=%d errno=%d\n",result,errno);return result;}
    puts("LiteOS musl UDP bind locks disconnect shutdown EOF and packet EPIPE policy ok");
    return 0;
}

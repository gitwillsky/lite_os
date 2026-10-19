#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <sched.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <linux/capability.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <poll.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>

/* NETLINK_ROUTE：getifaddrs、link/addr/route dump 与 ioctl 一致，单设备查询、错误与端口语义。 */
struct request {
	struct nlmsghdr header;
	struct rtgenmsg generic;
};

static int send_dump(int fd, uint16_t type, uint32_t sequence)
{
	struct request request;
	memset(&request, 0, sizeof request);
	request.header.nlmsg_len = sizeof request;
	request.header.nlmsg_type = type;
	request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
	request.header.nlmsg_seq = sequence;
	request.generic.rtgen_family = AF_UNSPEC;
	return send(fd, &request, sizeof request, 0) == (ssize_t)sizeof request ? 0 : -1;
}

/* 读到 NLMSG_DONE；每条消息交给 visit。返回消息数，失败返回负数。 */
static int drain(int fd, uint32_t sequence, int (*visit)(const struct nlmsghdr *, void *), void *context)
{
	char buffer[8192];
	int count = 0;
	for (;;) {
		ssize_t bytes = recv(fd, buffer, sizeof buffer, 0);
		if (bytes <= 0) return -1;
		for (struct nlmsghdr *header = (struct nlmsghdr *)buffer; NLMSG_OK(header, (size_t)bytes);
		     header = NLMSG_NEXT(header, bytes)) {
			if (header->nlmsg_seq != sequence) return -2;
			if (header->nlmsg_type == NLMSG_DONE) return count;
			if (header->nlmsg_type == NLMSG_ERROR) return -3;
			if (!(header->nlmsg_flags & NLM_F_MULTI)) return -4;
			if (visit(header, context) != 0) return -5;
			count++;
		}
	}
}

struct links {
	int count;
	int loopback_index;
	unsigned loopback_flags;
	int mtu_matches;
	int indexes[8];
};

static int visit_link(const struct nlmsghdr *header, void *context)
{
	struct links *links = context;
	if (header->nlmsg_type != RTM_NEWLINK) return -1;
	const struct ifinfomsg *info = NLMSG_DATA(header);
	const char *name = NULL;
	uint32_t mtu = 0;
	int stats = 0;
	int length = (int)IFLA_PAYLOAD(header);
	for (const struct rtattr *attr = IFLA_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length)) {
		if (attr->rta_type == IFLA_IFNAME) name = RTA_DATA(attr);
		if (attr->rta_type == IFLA_MTU) memcpy(&mtu, RTA_DATA(attr), 4);
		if (attr->rta_type == IFLA_STATS64) stats = 1;
	}
	if (!name || !stats || info->ifi_index <= 0) return -1;
	/* 与 ioctl 比较 flags 与 MTU。 */
	int fd = socket(AF_INET, SOCK_DGRAM, 0);
	struct ifreq request;
	memset(&request, 0, sizeof request);
	strncpy(request.ifr_name, name, IFNAMSIZ - 1);
	if (ioctl(fd, SIOCGIFFLAGS, &request) != 0) return -1;
	unsigned short flags = (unsigned short)request.ifr_flags;
	if ((info->ifi_flags & 0xffff) != flags) return -1;
	if (ioctl(fd, SIOCGIFMTU, &request) != 0) return -1;
	links->mtu_matches += (uint32_t)request.ifr_mtu == mtu;
	if (ioctl(fd, SIOCGIFINDEX, &request) != 0) return -1;
	if (request.ifr_ifindex != info->ifi_index) return -1;
	close(fd);
	if (!strcmp(name, "lo")) {
		links->loopback_index = info->ifi_index;
		links->loopback_flags = info->ifi_flags;
		if (info->ifi_type != ARPHRD_LOOPBACK) return -1;
	}
	if (links->count < 8) links->indexes[links->count] = info->ifi_index;
	links->count++;
	return 0;
}

struct addresses {
	int count;
	int loopback_seen;
	int outside_links;
	struct links *links;
};

static int visit_address(const struct nlmsghdr *header, void *context)
{
	struct addresses *addresses = context;
	if (header->nlmsg_type != RTM_NEWADDR) return -1;
	const struct ifaddrmsg *info = NLMSG_DATA(header);
	if (info->ifa_family != AF_INET) return -1;
	uint32_t local = 0;
	int length = (int)IFA_PAYLOAD(header);
	for (const struct rtattr *attr = IFA_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length))
		if (attr->rta_type == IFA_LOCAL) memcpy(&local, RTA_DATA(attr), 4);
	if (local == 0) return -1;
	if (local == htonl(INADDR_LOOPBACK)) {
		addresses->loopback_seen = 1;
		if (info->ifa_prefixlen != 8 || info->ifa_scope != RT_SCOPE_HOST) return -1;
	}
	int known = 0;
	for (int i = 0; i < addresses->links->count && i < 8; i++)
		known |= addresses->links->indexes[i] == (int)info->ifa_index;
	addresses->outside_links += !known;
	addresses->count++;
	return 0;
}

struct routes {
	int count;
	int outside_links;
	struct links *links;
};

static int visit_route(const struct nlmsghdr *header, void *context)
{
	struct routes *routes = context;
	if (header->nlmsg_type != RTM_NEWROUTE) return -1;
	const struct rtmsg *info = NLMSG_DATA(header);
	if (info->rtm_family != AF_INET) return -1;
	int length = (int)RTM_PAYLOAD(header);
	int has_table = 0;
	for (const struct rtattr *attr = RTM_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length)) {
		if (attr->rta_type == RTA_TABLE) has_table = 1;
		if (attr->rta_type == RTA_OIF) {
			int oif;
			memcpy(&oif, RTA_DATA(attr), 4);
			int known = 0;
			for (int i = 0; i < routes->links->count && i < 8; i++)
				known |= routes->links->indexes[i] == oif;
			routes->outside_links += !known;
		}
	}
	if (!has_table) return -1;
	routes->count++;
	return 0;
}

static int expect_error(int fd, uint16_t type, uint16_t flags, const void *body, size_t body_length, int expected)
{
	struct { struct nlmsghdr header; char body[64]; } request;
	memset(&request, 0, sizeof request);
	request.header.nlmsg_len = NLMSG_LENGTH(body_length);
	request.header.nlmsg_type = type;
	request.header.nlmsg_flags = flags;
	request.header.nlmsg_seq = 77;
	memcpy(request.body, body, body_length);
	if (send(fd, &request, request.header.nlmsg_len, 0) < 0) return -1;
	char buffer[512];
	ssize_t bytes = recv(fd, buffer, sizeof buffer, 0);
	if (bytes < (ssize_t)(sizeof(struct nlmsghdr) + sizeof(struct nlmsgerr))) return -2;
	struct nlmsghdr *reply = (struct nlmsghdr *)buffer;
	if (reply->nlmsg_type != NLMSG_ERROR || reply->nlmsg_seq != 77) return -3;
	return ((struct nlmsgerr *)NLMSG_DATA(reply))->error == expected ? 0 : -4;
}

/* 单设备 RTM_GETLINK：index > 0 按 index，否则按名字；返回回复类型（RTM_NEWLINK 或 NLMSG_ERROR），
 * *error 为 NLMSG_ERROR 的 errno，*found_index 为 NEWLINK 的 ifi_index。 */
static int request_link(int fd, int index, const char *name, uint32_t sequence, int *error, int *found_index)
{
	struct { struct nlmsghdr header; struct ifinfomsg info; char attributes[32]; } request;
	memset(&request, 0, sizeof request);
	request.header.nlmsg_type = RTM_GETLINK;
	request.header.nlmsg_flags = NLM_F_REQUEST;
	request.header.nlmsg_seq = sequence;
	request.info.ifi_index = index;
	size_t length = NLMSG_LENGTH(sizeof request.info);
	if (name) {
		struct rtattr *attr = (struct rtattr *)request.attributes;
		attr->rta_type = IFLA_IFNAME;
		attr->rta_len = RTA_LENGTH(strlen(name) + 1);
		memcpy(RTA_DATA(attr), name, strlen(name) + 1);
		length += RTA_ALIGN(attr->rta_len);
	}
	request.header.nlmsg_len = length;
	if (send(fd, &request, length, 0) != (ssize_t)length) return -1;
	char buffer[1024];
	ssize_t bytes = recv(fd, buffer, sizeof buffer, 0);
	struct nlmsghdr *reply = (struct nlmsghdr *)buffer;
	if (bytes < (ssize_t)sizeof *reply || reply->nlmsg_seq != sequence) return -1;
	if (reply->nlmsg_type == NLMSG_ERROR) *error = ((struct nlmsgerr *)NLMSG_DATA(reply))->error;
	if (reply->nlmsg_type == RTM_NEWLINK) *found_index = ((struct ifinfomsg *)NLMSG_DATA(reply))->ifi_index;
	return reply->nlmsg_type;
}


/* ---- 修改请求：在全新的 NET 里配置 lo，不触碰 smoke 自身的网络 ---- */
struct builder {
	struct nlmsghdr header;
	char body[256];
};

static void begin(struct builder *b, uint16_t type, uint16_t flags, uint32_t sequence, const void *fixed, size_t fixed_length)
{
	memset(b, 0, sizeof *b);
	b->header.nlmsg_type = type;
	b->header.nlmsg_flags = NLM_F_REQUEST | flags;
	b->header.nlmsg_seq = sequence;
	memcpy(b->body, fixed, fixed_length);
	b->header.nlmsg_len = NLMSG_LENGTH(fixed_length);
}

static void attribute(struct builder *b, uint16_t type, const void *data, size_t size)
{
	struct rtattr *attr = (struct rtattr *)((char *)b + NLMSG_ALIGN(b->header.nlmsg_len));
	attr->rta_type = type;
	attr->rta_len = RTA_LENGTH(size);
	memcpy(RTA_DATA(attr), data, size);
	b->header.nlmsg_len = NLMSG_ALIGN(b->header.nlmsg_len) + RTA_ALIGN(attr->rta_len);
}

/* 发送一个修改请求并读取回复；返回 NLMSG_ERROR 里的 error（0 为 ACK），传输失败返回 1。 */
static int change(int fd, struct builder *b)
{
	if (send(fd, b, b->header.nlmsg_len, 0) != (ssize_t)b->header.nlmsg_len) return 1;
	char buffer[512];
	ssize_t bytes = recv(fd, buffer, sizeof buffer, 0);
	struct nlmsghdr *reply = (struct nlmsghdr *)buffer;
	if (bytes < (ssize_t)(sizeof *reply + sizeof(struct nlmsgerr)) || reply->nlmsg_type != NLMSG_ERROR) return 1;
	return ((struct nlmsgerr *)NLMSG_DATA(reply))->error;
}

struct snapshot {
	unsigned link_flags;
	uint32_t mtu;
	int address_count;
	uint32_t local;
	unsigned char prefix;
	int route_seen;
};

static int visit_snapshot_link(const struct nlmsghdr *header, void *context)
{
	struct snapshot *snapshot = context;
	const struct ifinfomsg *info = NLMSG_DATA(header);
	snapshot->link_flags = info->ifi_flags;
	int length = (int)IFLA_PAYLOAD(header);
	for (const struct rtattr *attr = IFLA_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length))
		if (attr->rta_type == IFLA_MTU) memcpy(&snapshot->mtu, RTA_DATA(attr), 4);
	return 0;
}

static int visit_snapshot_address(const struct nlmsghdr *header, void *context)
{
	struct snapshot *snapshot = context;
	const struct ifaddrmsg *info = NLMSG_DATA(header);
	snapshot->address_count++;
	snapshot->prefix = info->ifa_prefixlen;
	int length = (int)IFA_PAYLOAD(header);
	for (const struct rtattr *attr = IFA_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length))
		if (attr->rta_type == IFA_LOCAL) memcpy(&snapshot->local, RTA_DATA(attr), 4);
	return 0;
}

static int visit_snapshot_route(const struct nlmsghdr *header, void *context)
{
	struct snapshot *snapshot = context;
	const struct rtmsg *info = NLMSG_DATA(header);
	int length = (int)RTM_PAYLOAD(header);
	uint32_t destination = 0;
	int oif = 0;
	for (const struct rtattr *attr = RTM_RTA(info); RTA_OK(attr, length); attr = RTA_NEXT(attr, length)) {
		if (attr->rta_type == RTA_DST) memcpy(&destination, RTA_DATA(attr), 4);
		if (attr->rta_type == RTA_OIF) memcpy(&oif, RTA_DATA(attr), 4);
	}
	if (destination == inet_addr("10.77.0.0") && info->rtm_dst_len == 16 && oif == 1) snapshot->route_seen = 1;
	return 0;
}

static void take_snapshot(int fd, struct snapshot *snapshot, uint32_t sequence)
{
	memset(snapshot, 0, sizeof *snapshot);
	send_dump(fd, RTM_GETLINK, sequence);
	drain(fd, sequence, visit_snapshot_link, snapshot);
	send_dump(fd, RTM_GETADDR, sequence + 1);
	drain(fd, sequence + 1, visit_snapshot_address, snapshot);
	send_dump(fd, RTM_GETROUTE, sequence + 2);
	drain(fd, sequence + 2, visit_snapshot_route, snapshot);
}

static int link_change(int fd, uint32_t sequence, unsigned flags, unsigned mask, int mtu, const char *rename, uint16_t type, uint16_t nl_flags)
{
	struct ifinfomsg info;
	memset(&info, 0, sizeof info);
	info.ifi_index = 1;
	info.ifi_flags = flags;
	info.ifi_change = mask;
	struct builder b;
	begin(&b, type, nl_flags, sequence, &info, sizeof info);
	if (mtu) { uint32_t value = (uint32_t)mtu; attribute(&b, IFLA_MTU, &value, 4); }
	if (rename) attribute(&b, IFLA_IFNAME, rename, strlen(rename) + 1);
	return change(fd, &b);
}

static int address_change(int fd, uint32_t sequence, uint16_t type, uint16_t nl_flags, const char *address, int prefix)
{
	struct ifaddrmsg info;
	memset(&info, 0, sizeof info);
	info.ifa_family = AF_INET;
	info.ifa_prefixlen = (unsigned char)prefix;
	info.ifa_index = 1;
	struct builder b;
	begin(&b, type, nl_flags, sequence, &info, sizeof info);
	uint32_t local = inet_addr(address);
	attribute(&b, IFA_LOCAL, &local, 4);
	attribute(&b, IFA_ADDRESS, &local, 4);
	return change(fd, &b);
}

static int route_change(int fd, uint32_t sequence, uint16_t type, uint16_t nl_flags, unsigned char route_type)
{
	struct rtmsg info;
	memset(&info, 0, sizeof info);
	info.rtm_family = AF_INET;
	info.rtm_dst_len = 16;
	info.rtm_table = RT_TABLE_MAIN;
	info.rtm_protocol = RTPROT_BOOT;
	info.rtm_scope = RT_SCOPE_LINK;
	info.rtm_type = route_type;
	struct builder b;
	begin(&b, type, nl_flags, sequence, &info, sizeof info);
	uint32_t destination = inet_addr("10.77.0.0");
	uint32_t oif = 1;
	attribute(&b, RTA_DST, &destination, 4);
	attribute(&b, RTA_OIF, &oif, 4);
	return change(fd, &b);
}

static int modify_in_new_namespace(void)
{
	/* 先降为普通用户，再同时创建 user 与 net namespace：新 userns 里拥有对这个 NET 的全部 capability，
	 * 与起始进程在 smoke 里剩下什么权限无关。 */
	if (setgid(1000) != 0 || setuid(1000) != 0) return 90;
	/* 失败时把 errno 编进退出码（40 + errno）：smoke 里这一步取决于进程当时的状态。 */
	if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 40 + errno;
	int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
	if (fd < 0) return 2;
	struct snapshot snapshot;
	take_snapshot(fd, &snapshot, 100);
	if (snapshot.link_flags & IFF_UP || snapshot.address_count != 0) return 3;

	/* 链路：UP 之后内核发表 127.0.0.1/8（原生 NETDEV_UP）。 */
	if (link_change(fd, 110, IFF_UP, IFF_UP, 0, NULL, RTM_NEWLINK, NLM_F_ACK) != 0) return 4;
	take_snapshot(fd, &snapshot, 120);
	if (!(snapshot.link_flags & IFF_UP) || snapshot.address_count != 1 || snapshot.local != inet_addr("127.0.0.1") || snapshot.prefix != 8) return 5;

	/* 地址：删除、新增、重复新增、不同地址（多地址不支持）、不匹配删除。 */
	if (address_change(fd, 130, RTM_DELADDR, NLM_F_ACK, "127.0.0.1", 8) != 0) return 6;
	take_snapshot(fd, &snapshot, 140);
	if (snapshot.address_count != 0) return 7;
	if (address_change(fd, 150, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK, "10.20.30.1", 24) != 0) return 8;
	take_snapshot(fd, &snapshot, 160);
	if (snapshot.address_count != 1 || snapshot.local != inet_addr("10.20.30.1") || snapshot.prefix != 24) return 9;
	if (address_change(fd, 170, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK, "10.20.30.1", 24) != -EEXIST) return 10;
	if (address_change(fd, 171, RTM_NEWADDR, NLM_F_CREATE | NLM_F_ACK, "10.20.30.2", 24) != -EOPNOTSUPP) return 11;
	if (address_change(fd, 172, RTM_DELADDR, NLM_F_ACK, "10.20.30.9", 24) != -EADDRNOTAVAIL) return 12;
	if (address_change(fd, 173, RTM_DELADDR, NLM_F_ACK, "10.20.30.1", 16) != -EADDRNOTAVAIL) return 13;
	/* ioctl 看到同一个地址。 */
	int inet = socket(AF_INET, SOCK_DGRAM, 0);
	struct ifreq request;
	memset(&request, 0, sizeof request);
	strcpy(request.ifr_name, "lo");
	if (ioctl(inet, SIOCGIFADDR, &request) != 0 ||
	    ((struct sockaddr_in *)&request.ifr_addr)->sin_addr.s_addr != inet_addr("10.20.30.1")) return 14;
	close(inet);

	/* 路由：新增、重复新增、dump 可见、RTN_UNSPEC 删除、重复删除。 */
	if (route_change(fd, 180, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK, RTN_UNICAST) != 0) return 15;
	take_snapshot(fd, &snapshot, 190);
	if (!snapshot.route_seen) return 16;
	if (route_change(fd, 200, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK, RTN_UNICAST) != -EEXIST) return 17;
	if (route_change(fd, 201, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_ACK, RTN_BLACKHOLE) != -EOPNOTSUPP) return 18;
	if (route_change(fd, 210, RTM_DELROUTE, NLM_F_ACK, RTN_UNSPEC) != 0) return 19;
	take_snapshot(fd, &snapshot, 220);
	if (snapshot.route_seen) return 20;
	if (route_change(fd, 230, RTM_DELROUTE, NLM_F_ACK, RTN_UNSPEC) != -ESRCH) return 21;

	/* MTU 与不支持的链路修改。 */
	if (link_change(fd, 240, 0, 0, 4000, NULL, RTM_NEWLINK, NLM_F_ACK) != 0) return 22;
	take_snapshot(fd, &snapshot, 250);
	if (snapshot.mtu != 4000) return 23;
	if (link_change(fd, 260, 0, 0, 0, "renamed0", RTM_NEWLINK, NLM_F_ACK) != -EOPNOTSUPP) return 24;
	if (link_change(fd, 261, IFF_PROMISC, IFF_PROMISC, 0, NULL, RTM_NEWLINK, NLM_F_ACK) != -EOPNOTSUPP) return 25;
	if (link_change(fd, 262, 0, 0, 0, NULL, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK) != -EEXIST) return 26;
	if (link_change(fd, 263, 0, 0, 0, NULL, RTM_DELLINK, NLM_F_ACK) != -EOPNOTSUPP) return 27;
	if (link_change(fd, 270, 0, IFF_UP, 0, NULL, RTM_SETLINK, NLM_F_ACK) != 0) return 28; /* flags=0 change=UP：DOWN */
	take_snapshot(fd, &snapshot, 280);
	if (snapshot.link_flags & IFF_UP) return 29;

	/* 去掉有效集里的 CAP_NET_ADMIN：发送者得到 EPERM，查询仍然可用。 */
	struct __user_cap_header_struct capability_header = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
	struct __user_cap_data_struct capability_data[2];
	if (syscall(SYS_capget, &capability_header, capability_data) != 0) return 30;
	capability_data[0].effective &= ~(1u << CAP_NET_ADMIN);
	if (syscall(SYS_capset, &capability_header, capability_data) != 0) return 30;
	if (link_change(fd, 290, IFF_UP, IFF_UP, 0, NULL, RTM_NEWLINK, NLM_F_ACK) != -EPERM) return 31;
	if (address_change(fd, 291, RTM_NEWADDR, NLM_F_CREATE | NLM_F_ACK, "10.1.1.1", 24) != -EPERM) return 32;
	if (route_change(fd, 292, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_ACK, RTN_UNICAST) != -EPERM) return 33;
	take_snapshot(fd, &snapshot, 300);
	if (snapshot.address_count != 1) return 34;
	close(fd);
	return 0;
}

static int verify_netlink_modify(void)
{
	pid_t child = fork();
	if (child < 0) return 80;
	if (child == 0) _exit(modify_in_new_namespace());
	int status;
	if (waitpid(child, &status, 0) != child) return 81;
	if (!WIFEXITED(status)) return 82;
	return WEXITSTATUS(status);
}

int verify_netlink_route(void)
{
	/* getifaddrs 走的就是 RTM_GETLINK + RTM_GETADDR 两次 dump。 */
	struct ifaddrs *list;
	if (getifaddrs(&list) != 0) return 1;
	int loopback_ipv4 = 0, packet_entries = 0;
	for (struct ifaddrs *entry = list; entry; entry = entry->ifa_next) {
		if (!entry->ifa_name) return 2;
		if (entry->ifa_addr && entry->ifa_addr->sa_family == AF_PACKET) packet_entries++;
		if (!strcmp(entry->ifa_name, "lo") && entry->ifa_addr && entry->ifa_addr->sa_family == AF_INET) {
			loopback_ipv4 = 1;
			if (!(entry->ifa_flags & IFF_LOOPBACK) || !(entry->ifa_flags & IFF_UP)) return 3;
		}
	}
	freeifaddrs(list);
	if (!loopback_ipv4 || packet_entries == 0) return 4;

	int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
	if (fd < 0) return 5;
	struct links links;
	memset(&links, 0, sizeof links);
	if (send_dump(fd, RTM_GETLINK, 11) != 0) return 6;
	struct pollfd poller = {.fd = fd, .events = POLLIN};
	if (poll(&poller, 1, 1000) != 1 || !(poller.revents & POLLIN)) return 7;
	int link_count = drain(fd, 11, visit_link, &links);
	if (link_count < 1 || link_count != links.count) return 8;
	if (links.loopback_index != 1 || !(links.loopback_flags & IFF_LOOPBACK) || links.mtu_matches != link_count) return 9;

	struct addresses addresses = {.links = &links};
	if (send_dump(fd, RTM_GETADDR, 12) != 0) return 10;
	if (drain(fd, 12, visit_address, &addresses) < 1) return 11;
	if (!addresses.loopback_seen || addresses.outside_links != 0) return 12;

	struct routes routes = {.links = &links};
	if (send_dump(fd, RTM_GETROUTE, 13) != 0) return 13;
	int route_count = drain(fd, 13, visit_route, &routes);
	if (route_count < 1 || routes.outside_links != 0) return 14;

	/* 没有 IPv6 地址：其他地址族的 dump 是空的，直接 DONE。 */
	struct request v6;
	memset(&v6, 0, sizeof v6);
	v6.header.nlmsg_len = sizeof v6;
	v6.header.nlmsg_type = RTM_GETADDR;
	v6.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
	v6.header.nlmsg_seq = 14;
	v6.generic.rtgen_family = AF_INET6;
	if (send(fd, &v6, sizeof v6, 0) != (ssize_t)sizeof v6) return 15;
	if (drain(fd, 14, visit_address, &addresses) != 0) return 16;

	/* 无事件时 non-blocking recv 不阻塞。 */
	errno = 0;
	if (recv(fd, &v6, sizeof v6, MSG_DONTWAIT) != -1 || errno != EAGAIN) return 17;

	/* 按名字与 index 查询单个设备；不存在得到 ENODEV，缺少选择条件得到 EINVAL。 */
	int error = 0, found = 0;
	if (request_link(fd, 0, "lo", 15, &error, &found) != RTM_NEWLINK || found != 1) return 18;
	found = 0;
	if (request_link(fd, 1, NULL, 16, &error, &found) != RTM_NEWLINK || found != 1) return 19;
	if (request_link(fd, 0, "nosuch0", 17, &error, &found) != NLMSG_ERROR || error != -ENODEV) return 20;
	if (request_link(fd, 9999, NULL, 18, &error, &found) != NLMSG_ERROR || error != -ENODEV) return 21;
	if (request_link(fd, 0, NULL, 19, &error, &found) != NLMSG_ERROR || error != -EINVAL) return 22;

	/* ACK 标志对未知控制消息返回 error = 0。 */
	struct ifaddrmsg add;
	memset(&add, 0, sizeof add);
	add.ifa_family = AF_INET;
	/* 修改请求已支持：缺少 IFA_LOCAL 是 EINVAL；邻居表等其他对象仍不支持。 */
	if (expect_error(fd, RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, &add, sizeof add, -EINVAL) != 0) return 24;
	if (expect_error(fd, RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK, &add, sizeof add, -EOPNOTSUPP) != 0) return 42;
	if (expect_error(fd, RTM_GETADDR, NLM_F_REQUEST, &add, sizeof add, -EOPNOTSUPP) != 0) return 25;
	if (expect_error(fd, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, &add, sizeof add, 0) != 0) return 26;

	/* 小接收缓冲：数据报被截断并标记 MSG_TRUNC，被截断数据报的余下部分丢失（用独立 socket，避免影响后续步骤）。 */
	int truncated = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
	if (truncated < 0 || send_dump(truncated, RTM_GETLINK, 31) != 0) return 27;
	char tiny[40];
	struct iovec vector = {tiny, sizeof tiny};
	struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1};
	ssize_t got = recvmsg(truncated, &message, 0);
	if (got != (ssize_t)sizeof tiny || !(message.msg_flags & MSG_TRUNC)) return 28;
	close(truncated);

	/* iproute2 在 bind 前设置收发缓冲；回读值是请求的两倍。 */
	int requested = 32768, stored = 0;
	socklen_t stored_length = sizeof stored;
	if (setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &requested, sizeof requested) != 0) return 38;
	if (getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &stored, &stored_length) != 0 || stored != 2 * requested) return 39;
	requested = 1 << 20;
	if (setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &requested, sizeof requested) != 0) return 40;
	stored_length = sizeof stored;
	if (getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &stored, &stored_length) != 0 || stored < requested) return 41;

	/* bind：port 0 自动分配，第二个 socket 得到不同 port；group 不支持；关闭后 port 可重用。 */
	struct sockaddr_nl address = {.nl_family = AF_NETLINK};
	int second = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
	if (second < 0 || bind(second, (struct sockaddr *)&address, sizeof address) != 0) return 30;
	socklen_t length = sizeof address;
	struct sockaddr_nl first_name, second_name;
	if (getsockname(second, (struct sockaddr *)&second_name, &length) != 0 || second_name.nl_pid == 0) return 31;
	if (send_dump(fd, RTM_GETLINK, 32) != 0) return 32; /* 首次发送自动绑定 */
	length = sizeof first_name;
	if (getsockname(fd, (struct sockaddr *)&first_name, &length) != 0) return 33;
	if (first_name.nl_pid == 0 || first_name.nl_pid == second_name.nl_pid) return 34;
	drain(fd, 32, visit_link, &links);
	struct sockaddr_nl clash = {.nl_family = AF_NETLINK, .nl_pid = second_name.nl_pid};
	int third = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
	errno = 0;
	if (bind(third, (struct sockaddr *)&clash, sizeof clash) != -1 || errno != EADDRINUSE) return 35;
	close(second);
	if (bind(third, (struct sockaddr *)&clash, sizeof clash) != 0) return 36;
	struct sockaddr_nl groups = {.nl_family = AF_NETLINK, .nl_groups = RTMGRP_LINK};
	int fourth = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
	errno = 0;
	if (bind(fourth, (struct sockaddr *)&groups, sizeof groups) != -1 || errno != EOPNOTSUPP) return 37;
	close(third);
	close(fourth);
	close(fd);

	/* 修改请求在子进程的新 NET 里验证。 */
	int modify_result = verify_netlink_modify();
	/* 子进程的检查点编号加 60，与上面的编号区分。 */
	return modify_result == 0 ? 0 : 60 + modify_result;
}

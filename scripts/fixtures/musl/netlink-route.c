#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
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

	/* 修改请求明确不支持；ACK 标志对未知控制消息返回 error = 0。 */
	struct ifaddrmsg add;
	memset(&add, 0, sizeof add);
	add.ifa_family = AF_INET;
	if (expect_error(fd, RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, &add, sizeof add, -EOPNOTSUPP) != 0) return 24;
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
	return 0;
}

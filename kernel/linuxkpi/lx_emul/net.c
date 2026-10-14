// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul networking: net_device, sk_buff and NAPI for Linux Ethernet drivers, and the
 * bridge to Nova's own TCP/IP stack (smoltcp, kernel/src/net.rs).
 *
 * Linux's protocol stack is not used: a received frame goes from the driver's
 * netif_receive_skb() / napi_gro_receive() straight to Nova (nova_netdev_rx), and Nova
 * transmits by handing a frame to the driver's ndo_start_xmit. Linux's own Ethernet helpers
 * (eth_type_trans, ether_setup, alloc_etherdev, ...) are the real net/ethernet/eth.c.
 *
 * Socket buffers are always linear (no page fragments): every skb Nova or a driver
 * allocates here has all its data in the head buffer.
 */
#include <linux/netdevice.h>
#include <linux/etherdevice.h>
#include <linux/if_vlan.h>
#include <linux/skbuff.h>
#include <linux/ethtool.h>
#include <linux/property.h>
#include <linux/rtnetlink.h>
#include <net/gro.h>
#include <net/flow_dissector.h>
#include <net/net_namespace.h>
#include <net/checksum.h>
#include "nova.h"

/* ---------------------------------------------------------------- devices */

static struct net_device *lx_netdevs[8];

struct net_device *alloc_netdev_mqs(int sizeof_priv, const char *name, unsigned char name_assign_type,
				    void (*setup)(struct net_device *), unsigned int txqs, unsigned int rxqs)
{
	struct net_device *dev;
	unsigned int i;

	if (!txqs)
		txqs = 1;
	dev = kzalloc(struct_size(dev, priv, sizeof_priv), GFP_KERNEL);
	if (!dev)
		return NULL;
	dev->priv_len = sizeof_priv;
	dev->dev_addr = kzalloc(MAX_ADDR_LEN, GFP_KERNEL);
	dev->_tx = kcalloc(txqs, sizeof(*dev->_tx), GFP_KERNEL);
	if (!dev->dev_addr || !dev->_tx) {
		kfree(dev->dev_addr);
		kfree(dev->_tx);
		kfree(dev);
		return NULL;
	}
	for (i = 0; i < txqs; i++)
		dev->_tx[i].dev = dev;
	dev->num_tx_queues = txqs;
	dev->real_num_tx_queues = txqs;
	dev->num_rx_queues = rxqs;
	dev->real_num_rx_queues = rxqs;
	INIT_LIST_HEAD(&dev->napi_list);
	INIT_LIST_HEAD(&dev->mc.list);
	INIT_LIST_HEAD(&dev->uc.list);
	dev->name_assign_type = name_assign_type;
	strscpy(dev->name, name, sizeof(dev->name));
	set_bit(__LINK_STATE_PRESENT, &dev->state);
	setup(dev);
	return dev;
}

void free_netdev(struct net_device *dev)
{
	if (!dev)
		return;
	kfree(dev->dev_addr);
	kfree(dev->_tx);
	kfree(dev);
}

void dev_addr_mod(struct net_device *dev, unsigned int offset, const void *addr, size_t len)
{
	memcpy((u8 *)dev->dev_addr + offset, addr, len);
	memcpy(dev->dev_addr_shadow + offset, addr, len);
}

int register_netdev(struct net_device *dev)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_netdevs) && lx_netdevs[i]; i++)
		;
	if (i == ARRAY_SIZE(lx_netdevs))
		return -ENOSPC;
	if (strchr(dev->name, '%')) {
		char name[IFNAMSIZ];

		snprintf(name, sizeof(name), dev->name, i);
		strscpy(dev->name, name, sizeof(dev->name));
	}
	if (dev->netdev_ops && dev->netdev_ops->ndo_init) {
		int err = dev->netdev_ops->ndo_init(dev);

		if (err)
			return err;
	}
	dev->ifindex = i + 1;
	dev->reg_state = NETREG_REGISTERED;
	lx_netdevs[i] = dev;
	return 0;
}

void unregister_netdev(struct net_device *dev)
{
	unsigned int i;

	if (test_and_clear_bit(__LINK_STATE_START, &dev->state) && dev->netdev_ops->ndo_stop)
		dev->netdev_ops->ndo_stop(dev);
	for (i = 0; i < ARRAY_SIZE(lx_netdevs); i++)
		if (lx_netdevs[i] == dev)
			lx_netdevs[i] = NULL;
	dev->reg_state = NETREG_UNREGISTERED;
}

void netif_carrier_on(struct net_device *dev)
{
	if (test_and_clear_bit(__LINK_STATE_NOCARRIER, &dev->state))
		netdev_info(dev, "link up\n");
}

void netif_carrier_off(struct net_device *dev)
{
	if (!test_and_set_bit(__LINK_STATE_NOCARRIER, &dev->state))
		netdev_info(dev, "link down\n");
}

void netif_carrier_event(struct net_device *dev)
{
}

void netif_device_detach(struct net_device *dev)
{
	if (test_and_clear_bit(__LINK_STATE_PRESENT, &dev->state) && netif_running(dev))
		netif_tx_stop_all_queues(dev);
}

void netif_device_attach(struct net_device *dev)
{
	if (!test_and_set_bit(__LINK_STATE_PRESENT, &dev->state) && netif_running(dev))
		netif_tx_wake_all_queues(dev);
}

void netif_tx_stop_all_queues(struct net_device *dev)
{
	unsigned int i;

	for (i = 0; i < dev->num_tx_queues; i++)
		netif_tx_stop_queue(netdev_get_tx_queue(dev, i));
}

/* Nova's stack polls for frames to send, so a woken queue needs no qdisc kick. */
void netif_tx_wake_queue(struct netdev_queue *dev_queue)
{
	clear_bit(__QUEUE_STATE_DRV_XOFF, &dev_queue->state);
}

void netdev_stats_to_stats64(struct rtnl_link_stats64 *stats64, const struct net_device_stats *netdev_stats)
{
	size_t i, n = sizeof(*netdev_stats) / sizeof(unsigned long);
	const unsigned long *src = (const unsigned long *)netdev_stats;
	u64 *dst = (u64 *)stats64;

	BUILD_BUG_ON(n > sizeof(*stats64) / sizeof(u64));
	for (i = 0; i < n; i++)
		dst[i] = src[i];
	memset(&dst[n], 0, sizeof(*stats64) - n * sizeof(u64));
}

u32 ethtool_op_get_link(struct net_device *dev)
{
	return netif_carrier_ok(dev) ? 1 : 0;
}

/* ethtool's legacy link-mode masks are the low 32 bits of the modern bitmaps. */
void ethtool_convert_legacy_u32_to_link_mode(unsigned long *dst, u32 legacy_u32)
{
	bitmap_zero(dst, __ETHTOOL_LINK_MODE_MASK_NBITS);
	dst[0] = legacy_u32;
}

bool ethtool_convert_link_mode_to_legacy_u32(u32 *legacy_u32, const unsigned long *src)
{
	*legacy_u32 = src[0];
	return find_next_bit(src, __ETHTOOL_LINK_MODE_MASK_NBITS, 32) == __ETHTOOL_LINK_MODE_MASK_NBITS;
}

/* ---------------------------------------------------------------- printing */

void netdev_printk(const char *level, const struct net_device *dev, const char *format, ...)
{
	struct va_format vaf;
	va_list args;

	va_start(args, format);
	vaf.fmt = format;
	vaf.va = &args;
	_printk("%s%s: %pV", level, dev ? dev->name : "(NULL net_device)", &vaf);
	va_end(args);
}

#define LX_NETDEV_PRINTK(fn, level)						\
	void fn(const struct net_device *dev, const char *format, ...)		\
	{									\
		struct va_format vaf;						\
		va_list args;							\
		va_start(args, format);						\
		vaf.fmt = format;						\
		vaf.va = &args;							\
		netdev_printk(level, dev, "%pV", &vaf);				\
		va_end(args);							\
	}

LX_NETDEV_PRINTK(netdev_emerg, KERN_EMERG)
LX_NETDEV_PRINTK(netdev_alert, KERN_ALERT)
LX_NETDEV_PRINTK(netdev_crit, KERN_CRIT)
LX_NETDEV_PRINTK(netdev_err, KERN_ERR)
LX_NETDEV_PRINTK(netdev_warn, KERN_WARNING)
LX_NETDEV_PRINTK(netdev_notice, KERN_NOTICE)
LX_NETDEV_PRINTK(netdev_info, KERN_INFO)

/* ---------------------------------------------------------------- sk_buff */

struct sk_buff *__alloc_skb(unsigned int size, gfp_t gfp_mask, int flags, int node)
{
	struct skb_shared_info *shinfo;
	struct sk_buff *skb;
	unsigned int data_size = SKB_DATA_ALIGN(size);
	u8 *data;

	skb = kzalloc(sizeof(*skb), gfp_mask & ~__GFP_DMA);
	if (!skb)
		return NULL;
	data = kmalloc(data_size + SKB_DATA_ALIGN(sizeof(struct skb_shared_info)), gfp_mask);
	if (!data) {
		kfree(skb);
		return NULL;
	}
	skb->head = data;
	skb->data = data;
	skb_reset_tail_pointer(skb);
	skb_set_end_offset(skb, data_size);
	skb->truesize = SKB_TRUESIZE(data_size);
	skb->mac_header = (typeof(skb->mac_header))~0U;
	skb->transport_header = (typeof(skb->transport_header))~0U;
	refcount_set(&skb->users, 1);
	shinfo = skb_shinfo(skb);
	memset(shinfo, 0, offsetof(struct skb_shared_info, dataref));
	atomic_set(&shinfo->dataref, 1);
	return skb;
}

static struct sk_buff *lx_alloc_rx_skb(struct net_device *dev, unsigned int length, gfp_t gfp)
{
	struct sk_buff *skb = __alloc_skb(length + NET_SKB_PAD + NET_IP_ALIGN, gfp, 0, NUMA_NO_NODE);

	if (skb) {
		skb_reserve(skb, NET_SKB_PAD + NET_IP_ALIGN);
		skb->dev = dev;
	}
	return skb;
}

struct sk_buff *__netdev_alloc_skb(struct net_device *dev, unsigned int length, gfp_t gfp_mask)
{
	return lx_alloc_rx_skb(dev, length, gfp_mask);
}

struct sk_buff *napi_alloc_skb(struct napi_struct *napi, unsigned int length)
{
	return lx_alloc_rx_skb(napi->dev, length, GFP_ATOMIC);
}

static void lx_free_skb(struct sk_buff *skb)
{
	if (!skb || !refcount_dec_and_test(&skb->users))
		return;
	kfree(skb->head);
	kfree(skb);
}

void sk_skb_reason_drop(const struct sock *sk, struct sk_buff *skb, enum skb_drop_reason reason)
{
	lx_free_skb(skb);
}

void dev_kfree_skb_irq_reason(struct sk_buff *skb, enum skb_drop_reason reason)
{
	lx_free_skb(skb);
}

void dev_kfree_skb_any_reason(struct sk_buff *skb, enum skb_drop_reason reason)
{
	lx_free_skb(skb);
}

void *skb_put(struct sk_buff *skb, unsigned int len)
{
	void *tmp = skb_tail_pointer(skb);

	skb->tail += len;
	skb->len += len;
	if (WARN_ON(skb->tail > skb->end)) {
		skb->tail -= len;
		skb->len -= len;
		return NULL;
	}
	return tmp;
}

void *skb_push(struct sk_buff *skb, unsigned int len)
{
	if (WARN_ON(skb->data - len < skb->head))
		return NULL;
	skb->data -= len;
	skb->len += len;
	return skb->data;
}

void *skb_pull(struct sk_buff *skb, unsigned int len)
{
	return len > skb->len ? NULL : __skb_pull(skb, len);
}

/* Only non-linear skbs need pulling, and Nova never builds those. */
void *__pskb_pull_tail(struct sk_buff *skb, int delta)
{
	return NULL;
}

int skb_copy_bits(const struct sk_buff *skb, int offset, void *to, int len)
{
	if (offset < 0 || len < 0 || offset + len > skb_headlen(skb))
		return -EFAULT;
	memcpy(to, skb->data + offset, len);
	return 0;
}

void skb_copy_and_csum_dev(const struct sk_buff *skb, u8 *to)
{
	unsigned int len = skb_headlen(skb);

	memcpy(to, skb->data, len);
	if (skb->ip_summed == CHECKSUM_PARTIAL) {
		unsigned int start = skb_checksum_start_offset(skb);
		__wsum csum = csum_partial(to + start, len - start, 0);

		*(__sum16 *)(to + start + skb->csum_offset) = csum_fold(csum);
	}
}

/* Linear skbs: a new head with more room, data copied over. */
int pskb_expand_head(struct sk_buff *skb, int nhead, int ntail, gfp_t gfp_mask)
{
	unsigned int size = skb_end_offset(skb) + nhead + ntail;
	unsigned int data_size = SKB_DATA_ALIGN(size);
	long off = skb->data - skb->head;
	u8 *data;

	if (WARN_ON(skb_shared(skb) || skb_is_nonlinear(skb)))
		return -EINVAL;
	data = kmalloc(data_size + SKB_DATA_ALIGN(sizeof(struct skb_shared_info)), gfp_mask);
	if (!data)
		return -ENOMEM;
	memcpy(data + nhead, skb->head, skb_tail_pointer(skb) - skb->head);
	memcpy(data + data_size, skb_shinfo(skb), offsetof(struct skb_shared_info, frags));
	kfree(skb->head);
	skb->head = data;
	skb->data = data + nhead + off;
	skb->tail += nhead;
	skb_set_end_offset(skb, data_size);
	if (skb_mac_header_was_set(skb))
		skb->mac_header += nhead;
	skb->network_header += nhead;
	if (skb_transport_header_was_set(skb))
		skb->transport_header += nhead;
	atomic_set(&skb_shinfo(skb)->dataref, 1);
	return 0;
}

int ___pskb_trim(struct sk_buff *skb, unsigned int len)
{
	if (WARN_ON(skb_is_nonlinear(skb)))
		return -EINVAL;
	__skb_trim(skb, len);
	return 0;
}

void skb_trim(struct sk_buff *skb, unsigned int len)
{
	if (skb->len > len)
		__skb_trim(skb, len);
}

int __skb_pad(struct sk_buff *skb, int pad, bool free_on_error)
{
	int ntail = pad - (skb->end - skb->tail);

	if (ntail > 0 && pskb_expand_head(skb, 0, ntail, GFP_ATOMIC)) {
		if (free_on_error)
			lx_free_skb(skb);
		return -ENOMEM;
	}
	memset(skb->data + skb->len, 0, pad);
	return 0;
}

void skb_tstamp_tx(struct sk_buff *orig_skb, struct skb_shared_hwtstamps *hwtstamps)
{
}

struct vlan_type_depth __vlan_get_protocol_offset(const struct sk_buff *skb, __be16 type, int mac_offset)
{
	struct vlan_type_depth r = { .type = type, .depth = ETH_HLEN };

	while (eth_type_vlan(r.type)) {
		struct vlan_hdr vhdr;

		if (skb_copy_bits(skb, mac_offset + r.depth, &vhdr, sizeof(vhdr)) < 0)
			return (struct vlan_type_depth){ .type = 0 };
		r.type = vhdr.h_vlan_encapsulated_proto;
		r.depth += VLAN_HLEN;
	}
	return r;
}

netdev_features_t passthru_features_check(struct sk_buff *skb, struct net_device *dev,
					  netdev_features_t features)
{
	return features;
}

struct rtnl_link_stats64 *dev_get_stats(struct net_device *dev, struct rtnl_link_stats64 *storage)
{
	memset(storage, 0, sizeof(*storage));
	if (dev->netdev_ops->ndo_get_stats64)
		dev->netdev_ops->ndo_get_stats64(dev, storage);
	else
		netdev_stats_to_stats64(storage, &dev->stats);
	return storage;
}

unsigned long dev_trans_start(struct net_device *dev)
{
	unsigned long res = READ_ONCE(netdev_get_tx_queue(dev, 0)->trans_start);
	unsigned int i;

	for (i = 1; i < dev->num_tx_queues; i++) {
		unsigned long t = READ_ONCE(netdev_get_tx_queue(dev, i)->trans_start);

		if (time_after(t, res))
			res = t;
	}
	return res;
}

void netdev_rss_key_fill(void *buffer, size_t len)
{
	get_random_bytes(buffer, len);
}

void netif_napi_set_irq_locked(struct napi_struct *napi, int irq)
{
	napi->irq = irq;
}

void netif_queue_set_napi(struct net_device *dev, unsigned int queue_index, enum netdev_queue_type type,
			  struct napi_struct *napi)
{
}

int net_ratelimit(void)
{
	static unsigned long last;
	static int burst;

	if (time_after(jiffies, last + 5 * HZ)) {
		last = jiffies;
		burst = 0;
	}
	return burst++ < 10;
}

int ethtool_op_get_ts_info(struct net_device *dev, struct kernel_ethtool_ts_info *info)
{
	info->so_timestamping = SOF_TIMESTAMPING_TX_SOFTWARE | SOF_TIMESTAMPING_RX_SOFTWARE |
				SOF_TIMESTAMPING_SOFTWARE;
	info->phc_index = -1;
	return 0;
}

void do_trace_netlink_extack(const char *msg)
{
}

/* Per-CPU softnet state: one CPU; only inline helpers touch it (xmit recursion). */
DEFINE_PER_CPU_ALIGNED(struct softnet_data, softnet_data);

/* ---------------------------------------------------------------- NAPI */

static LIST_HEAD(lx_napi_poll_list);

void netif_napi_add_weight_locked(struct net_device *dev, struct napi_struct *napi,
				  int (*poll)(struct napi_struct *, int), int weight)
{
	INIT_LIST_HEAD(&napi->poll_list);
	napi->dev = dev;
	napi->poll = poll;
	napi->weight = weight;
	/* Like Linux, a new instance starts disabled until napi_enable(). */
	set_bit(NAPI_STATE_SCHED, &napi->state);
	set_bit(NAPI_STATE_NPSVC, &napi->state);
	list_add_rcu(&napi->dev_list, &dev->napi_list);
}

void __netif_napi_del_locked(struct napi_struct *napi)
{
	list_del_init(&napi->poll_list);
	list_del_rcu(&napi->dev_list);
}

void napi_enable_locked(struct napi_struct *n)
{
	clear_bit(NAPI_STATE_SCHED, &n->state);
	clear_bit(NAPI_STATE_NPSVC, &n->state);
}

void napi_enable(struct napi_struct *n)
{
	napi_enable_locked(n);
}

void napi_disable_locked(struct napi_struct *n)
{
	unsigned long flags;

	set_bit(NAPI_STATE_DISABLE, &n->state);
	/* Polls run only in the "linux" task: a scheduled one is simply dropped. */
	local_irq_save(flags);
	list_del_init(&n->poll_list);
	local_irq_restore(flags);
	set_bit(NAPI_STATE_SCHED, &n->state);
	set_bit(NAPI_STATE_NPSVC, &n->state);
	clear_bit(NAPI_STATE_MISSED, &n->state);
	clear_bit(NAPI_STATE_DISABLE, &n->state);
}

void napi_disable(struct napi_struct *n)
{
	napi_disable_locked(n);
}

bool napi_schedule_prep(struct napi_struct *n)
{
	if (test_bit(NAPI_STATE_DISABLE, &n->state))
		return false;
	if (test_and_set_bit(NAPI_STATE_SCHED, &n->state)) {
		set_bit(NAPI_STATE_MISSED, &n->state);
		return false;
	}
	return true;
}

/* Called from interrupt handlers (interrupts off) and from task context. */
void __napi_schedule(struct napi_struct *n)
{
	unsigned long flags;

	local_irq_save(flags);
	if (list_empty(&n->poll_list))
		list_add_tail(&n->poll_list, &lx_napi_poll_list);
	local_irq_restore(flags);
}

void __napi_schedule_irqoff(struct napi_struct *n)
{
	__napi_schedule(n);
}

bool napi_complete_done(struct napi_struct *n, int work_done)
{
	/* An interrupt arrived while polling: stay scheduled for another round. */
	if (test_and_clear_bit(NAPI_STATE_MISSED, &n->state))
		return false;
	clear_bit(NAPI_STATE_SCHED, &n->state);
	return true;
}

/* Like net_rx_action: poll each scheduled instance with interrupts on. One that used its
 * whole budget (or was rescheduled from its interrupt meanwhile) stays on the list. */
void lx_emul_napi_poll(void)
{
	unsigned long flags;
	LIST_HEAD(list);

	local_irq_save(flags);
	list_splice_init(&lx_napi_poll_list, &list);
	local_irq_restore(flags);

	local_bh_disable();
	while (!list_empty(&list)) {
		struct napi_struct *n = list_first_entry(&list, struct napi_struct, poll_list);

		list_del_init(&n->poll_list);
		n->poll(n, n->weight);
		if (test_bit(NAPI_STATE_SCHED, &n->state))
			__napi_schedule(n);
	}
	local_bh_enable();
}

/* ---------------------------------------------------------------- receive path */

static int lx_deliver(struct sk_buff *skb)
{
	const u8 *frame = skb_mac_header_was_set(skb) ? skb_mac_header(skb) : skb->data;
	size_t len = skb->len + (skb->data - frame);

	nova_netdev_rx(skb->dev, frame, len);
	lx_free_skb(skb);
	return NET_RX_SUCCESS;
}

int netif_receive_skb(struct sk_buff *skb)
{
	return lx_deliver(skb);
}

int netif_rx(struct sk_buff *skb)
{
	return lx_deliver(skb);
}

int __netif_rx(struct sk_buff *skb)
{
	return lx_deliver(skb);
}

gro_result_t gro_receive_skb(struct gro_node *gro, struct sk_buff *skb)
{
	lx_deliver(skb);
	return GRO_NORMAL;
}

/* ---------------------------------------------------------------- eth.c's neighbours */

/* Linux's protocol offloads, flow dissector and network namespaces are not part of
 * Nova; eth.c only reaches these from paths Nova does not use (GRO, eth_get_headlen,
 * firmware-provided MAC addresses). */
struct net init_net;
struct flow_dissector flow_keys_basic_dissector;

void dev_add_offload(struct packet_offload *po)
{
}

void dev_remove_offload(struct packet_offload *po)
{
}

struct packet_offload *gro_find_receive_by_type(__be16 type)
{
	return NULL;
}

struct packet_offload *gro_find_complete_by_type(__be16 type)
{
	return NULL;
}

bool __skb_flow_dissect(const struct net *net, const struct sk_buff *skb, struct flow_dissector *flow_dissector,
			void *target_container, const void *data, __be16 proto, int nhoff, int hlen,
			unsigned int flags)
{
	return false;
}

u32 __skb_get_poff(const struct sk_buff *skb, const void *data, const struct flow_keys_basic *keys, int hlen)
{
	return 0;
}

struct fwnode_handle *__dev_fwnode(struct device *dev)
{
	return NULL;
}

int fwnode_property_read_u8_array(const struct fwnode_handle *fwnode, const char *propname, u8 *val, size_t nval)
{
	return -EINVAL;
}

/* ---------------------------------------------------------------- bridge to Nova */

void *nova_lx_netdev_get(unsigned int index)
{
	unsigned int i, n = 0;

	for (i = 0; i < ARRAY_SIZE(lx_netdevs); i++)
		if (lx_netdevs[i] && n++ == index)
			return lx_netdevs[i];
	return NULL;
}

/* What `ip link set dev up` does: __dev_open plus the receive filter. */
int nova_lx_netdev_open(void *opaque)
{
	struct net_device *dev = opaque;
	const struct net_device_ops *ops = dev->netdev_ops;
	int err = 0;

	if (netif_running(dev))
		return 0;
	set_bit(__LINK_STATE_START, &dev->state);
	rtnl_lock();
	if (ops->ndo_validate_addr)
		err = ops->ndo_validate_addr(dev);
	if (!err && ops->ndo_open)
		err = ops->ndo_open(dev);
	if (err) {
		clear_bit(__LINK_STATE_START, &dev->state);
	} else {
		dev->flags |= IFF_UP;
		if (ops->ndo_set_rx_mode)
			ops->ndo_set_rx_mode(dev);
	}
	rtnl_unlock();
	return err;
}

void nova_lx_netdev_mac(void *opaque, u8 mac[6])
{
	struct net_device *dev = opaque;

	memcpy(mac, dev->dev_addr, ETH_ALEN);
}

bool nova_lx_netdev_carrier(void *opaque)
{
	return netif_carrier_ok(opaque);
}

/* Whether the driver would take a frame now (Nova's stack keeps packets otherwise). */
bool nova_lx_netdev_tx_ready(void *opaque)
{
	struct net_device *dev = opaque;

	return netif_running(dev) && netif_carrier_ok(dev) && !netif_xmit_stopped(netdev_get_tx_queue(dev, 0));
}

int nova_lx_netdev_xmit(void *opaque, const u8 *frame, size_t len)
{
	struct net_device *dev = opaque;
	struct netdev_queue *txq = netdev_get_tx_queue(dev, 0);
	struct sk_buff *skb;
	netdev_tx_t ret;

	if (!netif_running(dev) || len < ETH_HLEN)
		return -EINVAL;
	/* Like Linux's qdisc layer: nothing is sent without carrier (many drivers enable
	 * their transmitter only on link up), the caller keeps the frame and retries. */
	if (!netif_carrier_ok(dev) || netif_xmit_stopped(txq))
		return 1;
	skb = __alloc_skb(len + NET_SKB_PAD, GFP_ATOMIC, 0, NUMA_NO_NODE);
	if (!skb)
		return -ENOMEM;
	skb_reserve(skb, NET_SKB_PAD);
	skb_put_data(skb, frame, len);
	skb_reset_mac_header(skb);
	skb->dev = dev;
	skb->protocol = eth_hdr(skb)->h_proto;
	skb->ip_summed = CHECKSUM_NONE;

	local_bh_disable();
	ret = dev->netdev_ops->ndo_start_xmit(skb, dev);
	local_bh_enable();
	if (ret == NETDEV_TX_BUSY) {
		lx_free_skb(skb);
		return 1;
	}
	return 0;
}

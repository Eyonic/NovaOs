// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul for Linux programs: the files Linux drivers create, used through Nova's Linux
 * system call layer (kernel/src/linux/lxfile.rs).
 *
 * Character devices: register_chrdev() and cdev_add() record their file operations, and
 * every device added with a device number (DRM's card0, renderD128) is listed for
 * Nova's /dev and /sys. Opening a node makes a struct file on the node's inode and runs
 * the driver's open, as chrdev_open() does.
 *
 * Open files: a program's descriptor holds a struct lx_ufile, which owns one reference
 * to the struct file. Nova calls the file operations: ioctl (compat_ioctl for 32-bit
 * programs, with TS_COMPAT set so in_compat_syscall() is right), read, write, llseek,
 * poll. Poll registers the lx_ufile on the wait queues the driver names with
 * poll_wait(), once; their wakeups bump a counter (epoll's edge detection) and wake
 * Nova's poll and epoll waiters, from any context.
 *
 * File descriptors made inside Linux code (dma-buf and sync-file exports) are the
 * calling program's own: get_unused_fd_flags() reserves a slot in its table and
 * fd_install() fills it; fget() finds the struct file behind a program's descriptor.
 *
 * Mappings: mmap() builds a vm_area_struct (struct lx_vma) for the Nova mapping and
 * calls f_op->mmap. Page faults in it call vm_ops->fault, whose vmf_insert_pfn*() (or
 * a returned page) maps frames through Nova (nova_vma_insert); remap_pfn_range() and
 * vm_insert_page() map at once. unmap_mapping_range() zaps every mapping of a file range
 * in every address space (TTM moving a buffer, a purged shmem object), so the next
 * access faults again. munmap and exit close the vma (vm_ops->close), fork duplicates
 * it (vm_ops->open). Nova owns the page tables; a vma knows its address space only
 * through Nova's token (`space`, 0 once Nova has let go of it).
 *
 * User memory: copy_from_user() and friends check the range against the program's
 * mappings (nova_user_range) and then copy; pages not present yet are filled by
 * Nova's page-fault handler, which also serves faults Linux code takes on user memory.
 */
#include <linux/fs.h>
#include <linux/file.h>
#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/device/class.h>
#include <linux/poll.h>
#include <linux/mm.h>
#include <linux/mman.h>
#include <linux/mm_types.h>
#include <linux/pagemap.h>
#include <linux/pci.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/compat.h>
#include <linux/sched.h>
#include <linux/sched/signal.h>
#include <linux/mutex.h>
#include <linux/list.h>
#include "nova.h"

/* ---- provided by Nova (Rust, kernel/src/linux/lxfile.rs) ---- */

/* Whether [addr, addr + len) is mapped (with write access if asked) in the program. */
bool nova_user_range(u64 addr, u64 len, bool write);
/* A descriptor slot of the calling program (-EMFILE, or -EBADF outside a program). */
int nova_fd_reserve(bool cloexec);
void nova_fd_unreserve(int fd);
/* Fill a reserved slot with an open file (the lx_ufile's reference moves to it). */
void nova_fd_install(int fd, void *ufile);
/* The struct file behind descriptor fd, with a reference (NULL if not a Linux file). */
struct file *nova_fd_get(int fd);
/* Wake Nova's poll/select/epoll waiters (any context). */
void nova_poll_wake(void);
/* Map frame pfn at va in address space `space` (page-table flags: RW, PWT, PCD, PAT, NX);
 * `page` is a struct page reference the mapping keeps (put again when it goes), or NULL. */
bool nova_vma_insert(u64 space, struct lx_vma *v, u64 va, u64 pfn, u64 pte_flags, struct page *page);
/* Remove the mappings of v in [start, end) of address space `space`. Both check that v
 * is still mapped there (a zap may race with munmap). */
void nova_vma_zap(u64 space, struct lx_vma *v, u64 start, u64 end);

/* ---------------------------------------------------------------- character devices */

struct lx_chrdev {
	struct list_head node;
	dev_t base;
	unsigned int count;
	const struct file_operations *fops;
	struct cdev *cdev;
};

static LIST_HEAD(lx_chrdevs);
static unsigned int lx_next_major = 240; /* Linux's first dynamic major (LOCAL/EXPERIMENTAL) */

static int lx_chrdev_add(dev_t base, unsigned int count, const struct file_operations *fops, struct cdev *cdev)
{
	struct lx_chrdev *c = kzalloc(sizeof(*c), GFP_KERNEL);

	if (!c)
		return -ENOMEM;
	c->base = base;
	c->count = count;
	c->fops = fops;
	c->cdev = cdev;
	list_add_tail(&c->node, &lx_chrdevs);
	return 0;
}

static void lx_chrdev_remove(dev_t base, struct cdev *cdev)
{
	struct lx_chrdev *c, *next;

	list_for_each_entry_safe(c, next, &lx_chrdevs, node) {
		if (cdev ? c->cdev == cdev : c->base == base && !c->cdev) {
			list_del(&c->node);
			kfree(c);
		}
	}
}

static const struct file_operations *lx_chrdev_fops(dev_t devt)
{
	struct lx_chrdev *c;

	list_for_each_entry(c, &lx_chrdevs, node)
		if (devt >= c->base && devt < c->base + c->count)
			return c->fops;
	return NULL;
}

int register_chrdev_region(dev_t from, unsigned count, const char *name)
{
	return 0;
}

void unregister_chrdev_region(dev_t from, unsigned count)
{
}

int __register_chrdev(unsigned int major, unsigned int baseminor, unsigned int count, const char *name,
		      const struct file_operations *fops)
{
	/* As Linux: 0 for a fixed major, the major number when one is assigned. */
	unsigned int m = major ? major : lx_next_major++;
	int err = lx_chrdev_add(MKDEV(m, baseminor), count, fops, NULL);

	if (err)
		return err;
	return major ? 0 : m;
}

void __unregister_chrdev(unsigned int major, unsigned int baseminor, unsigned int count, const char *name)
{
	lx_chrdev_remove(MKDEV(major, baseminor), NULL);
}

void cdev_init(struct cdev *cdev, const struct file_operations *fops)
{
	memset(cdev, 0, sizeof(*cdev));
	cdev->ops = fops;
}

int cdev_add(struct cdev *p, dev_t dev, unsigned count)
{
	p->dev = dev;
	p->count = count;
	return lx_chrdev_add(dev, count, p->ops, p);
}

void cdev_del(struct cdev *p)
{
	lx_chrdev_remove(p->dev, p);
}

/* ---------------------------------------------------------------- device nodes */

/* Devices with a device number, in the order they were added, and their inodes (one per
 * node, made on first open, as a device file system keeps one). */
struct lx_devnode {
	struct list_head node;
	struct device *dev;
	struct inode *inode;
};

static LIST_HEAD(lx_devnodes);
static DEFINE_MUTEX(lx_devnode_lock);

void lx_devnode_add(struct device *dev)
{
	struct lx_devnode *n = kzalloc(sizeof(*n), GFP_KERNEL);

	if (!n)
		return;
	n->dev = dev;
	mutex_lock(&lx_devnode_lock);
	list_add_tail(&n->node, &lx_devnodes);
	mutex_unlock(&lx_devnode_lock);
}

void lx_devnode_del(struct device *dev)
{
	struct lx_devnode *n, *next;

	mutex_lock(&lx_devnode_lock);
	list_for_each_entry_safe(n, next, &lx_devnodes, node) {
		if (n->dev == dev) {
			list_del(&n->node);
			iput(n->inode);
			kfree(n);
		}
	}
	mutex_unlock(&lx_devnode_lock);
}

bool nova_lx_devnode(unsigned int index, struct nova_devnode *out)
{
	struct lx_devnode *n;
	bool found = false;

	memset(out, 0, sizeof(*out));
	mutex_lock(&lx_devnode_lock);
	list_for_each_entry(n, &lx_devnodes, node) {
		struct device *dev = n->dev, *parent = dev->parent;
		umode_t mode = 0;
		char *path = NULL;

		if (index--)
			continue;
		if (dev->class && dev->class->devnode)
			path = dev->class->devnode(dev, &mode);
		strscpy(out->path, path ?: dev_name(dev), sizeof(out->path));
		kfree(path);
		strscpy(out->name, dev_name(dev), sizeof(out->name));
		out->major = MAJOR(dev->devt);
		out->minor = MINOR(dev->devt);
		if (parent) {
			strscpy(out->parent, dev_name(parent) ?: "", sizeof(out->parent));
			if (parent->bus)
				strscpy(out->bus, parent->bus->name, sizeof(out->bus));
			if (parent->driver)
				strscpy(out->driver, parent->driver->name, sizeof(out->driver));
			if (dev_is_pci(parent)) {
				struct pci_dev *pdev = to_pci_dev(parent);

				out->vendor = pdev->vendor;
				out->device = pdev->device;
				out->subvendor = pdev->subsystem_vendor;
				out->subdevice = pdev->subsystem_device;
				out->class_code = pdev->class;
				out->revision = pdev->revision;
			}
		}
		found = true;
		break;
	}
	mutex_unlock(&lx_devnode_lock);
	return found;
}

/* The inode of node `devt`, with a reference for the caller. */
static struct inode *lx_devnode_inode(dev_t devt, char *name, size_t size)
{
	struct inode *inode = NULL;
	struct lx_devnode *n;

	mutex_lock(&lx_devnode_lock);
	list_for_each_entry(n, &lx_devnodes, node) {
		if (n->dev->devt != devt)
			continue;
		if (!n->inode) {
			inode = alloc_anon_inode(NULL);
			if (IS_ERR(inode)) {
				inode = NULL;
				break;
			}
			inode->i_mode = S_IFCHR | 0666;
			inode->i_rdev = devt;
			n->inode = inode;
		}
		inode = n->inode;
		atomic_inc(&inode->i_count);
		strscpy(name, dev_name(n->dev), size);
		break;
	}
	mutex_unlock(&lx_devnode_lock);
	return inode;
}

/* ---------------------------------------------------------------- open files */

#define LX_POLL_QUEUES 4

struct lx_ufile;

struct lx_poll_entry {
	wait_queue_head_t *head;
	wait_queue_entry_t wait;
	struct lx_ufile *uf;
};

struct lx_ufile {
	struct file *file;
	poll_table pt;
	struct lx_poll_entry queues[LX_POLL_QUEUES];
	unsigned int nqueues;
	atomic64_t events;	/* wakeups seen on its wait queues */
};

static int lx_ufile_wake(wait_queue_entry_t *wait, unsigned mode, int sync, void *key)
{
	struct lx_poll_entry *e = container_of(wait, struct lx_poll_entry, wait);

	atomic64_inc(&e->uf->events);
	nova_poll_wake();
	return 0;
}

/* poll_wait(): stay on each wait queue the driver names until the file is closed. */
static void lx_ufile_queue(struct file *file, wait_queue_head_t *head, poll_table *pt)
{
	struct lx_ufile *uf = container_of(pt, struct lx_ufile, pt);
	struct lx_poll_entry *e;
	unsigned int i;

	for (i = 0; i < uf->nqueues; i++)
		if (uf->queues[i].head == head)
			return;
	if (WARN_ON_ONCE(uf->nqueues == LX_POLL_QUEUES))
		return;
	e = &uf->queues[uf->nqueues++];
	e->head = head;
	e->uf = uf;
	init_waitqueue_func_entry(&e->wait, lx_ufile_wake);
	add_wait_queue(head, &e->wait);
}

static struct lx_ufile *lx_ufile_new(struct file *file)
{
	struct lx_ufile *uf = kzalloc(sizeof(*uf), GFP_KERNEL);

	if (!uf)
		return NULL;
	uf->file = file;
	init_poll_funcptr(&uf->pt, lx_ufile_queue);
	atomic64_set(&uf->events, 1);
	return uf;
}

/* The last descriptor of an open file is gone: leave the wait queues (they belong to
 * the file's driver data), then drop the file reference (f_op->release on the last). */
void nova_lx_ufile_close(struct lx_ufile *uf)
{
	unsigned int i;

	for (i = 0; i < uf->nqueues; i++)
		remove_wait_queue(uf->queues[i].head, &uf->queues[i].wait);
	fput(uf->file);
	kfree(uf);
}

/* Open device node `devt` (as chrdev_open). Returns the open file, or NULL with *err. */
struct lx_ufile *nova_lx_ufile_open(u32 devt, u32 flags, int *err)
{
	const struct file_operations *fops = lx_chrdev_fops(devt);
	struct lx_ufile *uf;
	struct inode *inode;
	struct file *file;
	char name[32] = "";

	*err = -ENXIO;
	if (!fops)
		return NULL;
	inode = lx_devnode_inode(devt, name, sizeof(name));
	if (!inode)
		return NULL;
	inode->i_fop = fops;
	file = alloc_file_pseudo(inode, NULL, name, flags & ~(O_CREAT | O_NOCTTY | O_TRUNC | O_CLOEXEC), fops);
	if (IS_ERR(file)) {
		iput(inode);
		*err = PTR_ERR(file);
		return NULL;
	}
	if (file->f_op->open) {
		*err = file->f_op->open(inode, file);
		if (*err) {
			file->f_op = NULL;
			fput(file);
			return NULL;
		}
	}
	uf = lx_ufile_new(file);
	if (!uf) {
		fput(file);
		*err = -ENOMEM;
	}
	return uf;
}

long nova_lx_ufile_ioctl(struct lx_ufile *uf, u32 cmd, u64 arg, bool compat)
{
	struct file *file = uf->file;
	long ret;

	if (!compat) {
		if (!file->f_op->unlocked_ioctl)
			return -ENOTTY;
		ret = file->f_op->unlocked_ioctl(file, cmd, arg);
	} else {
		if (!file->f_op->compat_ioctl)
			return -ENOTTY;
		/* in_compat_syscall() for the driver's conversions (drm_ioc32.c). */
		current_thread_info()->status |= TS_COMPAT;
		ret = file->f_op->compat_ioctl(file, cmd, (u32)arg);
		current_thread_info()->status &= ~TS_COMPAT;
	}
	return ret == -ENOIOCTLCMD ? -ENOTTY : ret;
}

long nova_lx_ufile_read(struct lx_ufile *uf, u64 buf, u64 len)
{
	struct file *file = uf->file;

	if (!file->f_op->read)
		return -EINVAL;
	return file->f_op->read(file, (char __user *)buf, len, &file->f_pos);
}

long nova_lx_ufile_write(struct lx_ufile *uf, u64 buf, u64 len)
{
	struct file *file = uf->file;

	if (!file->f_op->write)
		return -EINVAL;
	return file->f_op->write(file, (const char __user *)buf, len, &file->f_pos);
}

s64 nova_lx_ufile_llseek(struct lx_ufile *uf, s64 offset, int whence)
{
	struct file *file = uf->file;

	if (!file->f_op->llseek)
		return -ESPIPE;
	return file->f_op->llseek(file, offset, whence);
}

/* Ready events (EPOLL* bits) and the wakeup count, for poll, select and epoll. */
u32 nova_lx_ufile_poll(struct lx_ufile *uf, u64 *events)
{
	struct file *file = uf->file;
	__poll_t mask;

	uf->pt._key = ~(__poll_t)0;
	mask = file->f_op->poll ? file->f_op->poll(file, &uf->pt) : DEFAULT_POLLMASK;
	*events = atomic64_read(&uf->events);
	return (__force u32)mask;
}

/* fcntl(F_SETFL): the flags drivers look at (O_NONBLOCK for DRM's event reads). */
void nova_lx_ufile_setfl(struct lx_ufile *uf, u32 flags)
{
	struct file *file = uf->file;
	const u32 settable = O_APPEND | O_NONBLOCK | O_NDELAY | FASYNC | O_DIRECT | O_NOATIME;

	spin_lock(&file->f_lock);
	file->f_flags = (file->f_flags & ~settable) | (flags & settable);
	spin_unlock(&file->f_lock);
}

void nova_lx_ufile_info(struct lx_ufile *uf, struct nova_ufile_info *info)
{
	struct file *file = uf->file;
	struct inode *inode = file_inode(file);

	info->mode = inode->i_mode;
	info->rdev_major = MAJOR(inode->i_rdev);
	info->rdev_minor = MINOR(inode->i_rdev);
	info->ino = inode->i_ino;
	strscpy(info->name, file->f_path.dentry->d_name.name, sizeof(info->name));
	info->anon = !S_ISCHR(inode->i_mode);
}

/* fget() on a program's descriptor: its file with a new reference. */
struct file *nova_lx_ufile_get(struct lx_ufile *uf)
{
	return get_file(uf->file);
}

/* ---------------------------------------------------------------- file descriptors */

int get_unused_fd_flags(unsigned flags)
{
	return nova_fd_reserve(flags & O_CLOEXEC);
}

void put_unused_fd(unsigned int fd)
{
	nova_fd_unreserve(fd);
}

/* Takes over the caller's file reference, as Linux's fd_install(). */
void fd_install(unsigned int fd, struct file *file)
{
	struct lx_ufile *uf = lx_ufile_new(file);

	if (!uf) {
		/* Out of memory with the slot reserved: the descriptor reads as closed. */
		nova_fd_unreserve(fd);
		fput(file);
		return;
	}
	nova_fd_install(fd, uf);
}

struct file *fget(unsigned int fd)
{
	return nova_fd_get(fd);
}

struct fd fdget(unsigned int fd)
{
	struct file *file = fget(fd);

	return file ? CLONED_FD(file) : EMPTY_FD;
}

#ifdef CONFIG_COMPAT
long compat_ptr_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	if (!file->f_op->unlocked_ioctl)
		return -ENOIOCTLCMD;
	return file->f_op->unlocked_ioctl(file, cmd, (unsigned long)compat_ptr(arg));
}
#endif

/* ---------------------------------------------------------------- user memory */

static bool lx_user_ok(unsigned long addr, unsigned long len, bool write)
{
	/* Kernel pointers (Linux code passing its own buffers) are always accessible. */
	if (addr >= TASK_SIZE_MAX)
		return true;
	return nova_user_range(addr, len, write);
}

/* For __get_user_N / __put_user_N (thunks.S). */
bool nova_lx_user_ok(unsigned long addr, unsigned long len, bool write)
{
	return lx_user_ok(addr, len, write);
}

unsigned long _copy_from_user(void *to, const void __user *from, unsigned long n)
{
	if (!lx_user_ok((unsigned long)from, n, false)) {
		memset(to, 0, n);
		return n;
	}
	memcpy(to, (const void __force *)from, n);
	return 0;
}

unsigned long _copy_to_user(void __user *to, const void *from, unsigned long n)
{
	if (!lx_user_ok((unsigned long)to, n, true))
		return n;
	memcpy((void __force *)to, from, n);
	return 0;
}

/* ---------------------------------------------------------------- mappings */

struct lx_vma {
	struct vm_area_struct vma;
	struct list_head node;
	u64 space;	/* Nova's address-space token; 0 once Nova let go of the mapping */
};

/* Linux code only reads vm_mm in paths Nova never takes (fault retries): one mm. */
static struct mm_struct lx_mm = {
	.mmap_lock = __RWSEM_INITIALIZER(lx_mm.mmap_lock),
	.mm_count = ATOMIC_INIT(1),
};
static LIST_HEAD(lx_vmas);
static DEFINE_MUTEX(lx_vma_lock);

static struct lx_vma *lx_vma(struct vm_area_struct *vma)
{
	return container_of(vma, struct lx_vma, vma);
}

static void lx_vma_link(struct lx_vma *v)
{
	mutex_lock(&lx_vma_lock);
	list_add_tail(&v->node, &lx_vmas);
	mutex_unlock(&lx_vma_lock);
}

static u64 lx_pte_flags(struct vm_area_struct *vma, pgprot_t prot)
{
	u64 flags = pgprot_val(prot) & (_PAGE_RW | _PAGE_PWT | _PAGE_PCD | _PAGE_PAT | _PAGE_NX);

	if (!(vma->vm_flags & VM_WRITE))
		flags &= ~_PAGE_RW;
	return flags;
}

static vm_fault_t lx_insert(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn, pgprot_t prot,
			    struct page *page)
{
	struct lx_vma *v = lx_vma(vma);

	if (addr < vma->vm_start || addr >= vma->vm_end)
		return VM_FAULT_SIGBUS;
	if (!v->space)
		return VM_FAULT_NOPAGE; /* the mapping is going away */
	return nova_vma_insert(v->space, v, addr & PAGE_MASK, pfn, lx_pte_flags(vma, prot), page) ? VM_FAULT_NOPAGE
												    : VM_FAULT_OOM;
}

vm_fault_t vmf_insert_pfn_prot(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn, pgprot_t pgprot)
{
	return lx_insert(vma, addr, pfn, pgprot, NULL);
}

vm_fault_t vmf_insert_pfn(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn)
{
	return lx_insert(vma, addr, pfn, vma->vm_page_prot, NULL);
}

vm_fault_t vmf_insert_mixed(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn)
{
	return lx_insert(vma, addr, pfn, vma->vm_page_prot, NULL);
}

int vm_insert_page(struct vm_area_struct *vma, unsigned long addr, struct page *page)
{
	vm_fault_t ret;

	get_page(page);
	ret = lx_insert(vma, addr, page_to_pfn(page), vma->vm_page_prot, page);
	if (ret != VM_FAULT_NOPAGE) {
		put_page(page);
		return ret == VM_FAULT_OOM ? -ENOMEM : -EFAULT;
	}
	return 0;
}

int remap_pfn_range(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn, unsigned long size,
		    pgprot_t prot)
{
	unsigned long end = addr + PAGE_ALIGN(size);

	if (addr < vma->vm_start || end > vma->vm_end)
		return -EINVAL;
	vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
	for (; addr < end; addr += PAGE_SIZE, pfn++)
		if (lx_insert(vma, addr, pfn, prot, NULL) != VM_FAULT_NOPAGE)
			return -ENOMEM;
	return 0;
}

/* Zap the mappings of [holebegin, holebegin + holelen) of `mapping` everywhere. */
void unmap_mapping_range(struct address_space *mapping, loff_t const holebegin, loff_t const holelen,
			 int even_cows)
{
	pgoff_t first = holebegin >> PAGE_SHIFT;
	pgoff_t last = holelen ? first + ((holelen + PAGE_SIZE - 1) >> PAGE_SHIFT) - 1 : ULONG_MAX;
	struct lx_vma *v;

	mutex_lock(&lx_vma_lock);
	list_for_each_entry(v, &lx_vmas, node) {
		struct vm_area_struct *vma = &v->vma;
		pgoff_t vfirst = vma->vm_pgoff, vlast = vfirst + vma_pages(vma) - 1, s, e;

		if (!v->space || !vma->vm_file || vma->vm_file->f_mapping != mapping)
			continue;
		if (vlast < first || vfirst > last)
			continue;
		s = max(first, vfirst);
		e = min(last, vlast);
		nova_vma_zap(v->space, v, vma->vm_start + ((s - vfirst) << PAGE_SHIFT),
			     vma->vm_start + ((e - vfirst + 1) << PAGE_SHIFT));
	}
	mutex_unlock(&lx_vma_lock);
}

/* A vma for mmap() of a Linux file at [addr, addr + len) in address space `space`;
 * nova_lx_vma_mmap() then runs the driver's mmap. */
struct lx_vma *nova_lx_vma_new(struct lx_ufile *uf, u64 space, u64 addr, u64 len, u32 prot, u32 flags, u64 pgoff,
			       int *err)
{
	struct file *file = uf->file;
	vm_flags_t vm_flags = VM_MAYREAD | VM_MAYWRITE | VM_MAYEXEC;
	struct lx_vma *v;

	*err = -ENODEV;
	if (!file->f_op->mmap)
		return NULL;
	if (prot & PROT_READ)
		vm_flags |= VM_READ;
	if (prot & PROT_WRITE)
		vm_flags |= VM_WRITE;
	if (prot & PROT_EXEC)
		vm_flags |= VM_EXEC;
	*err = -EACCES;
	if (!(file->f_mode & FMODE_READ))
		return NULL;
	if ((flags & MAP_TYPE) == MAP_SHARED || (flags & MAP_TYPE) == MAP_SHARED_VALIDATE) {
		if (!(file->f_mode & FMODE_WRITE)) {
			if (prot & PROT_WRITE)
				return NULL;
			vm_flags &= ~VM_MAYWRITE;
		} else {
			vm_flags |= VM_SHARED;
		}
		vm_flags |= VM_MAYSHARE;
	}
	*err = -ENOMEM;
	v = kzalloc(sizeof(*v), GFP_KERNEL);
	if (!v)
		return NULL;
	INIT_LIST_HEAD(&v->node);
	INIT_LIST_HEAD(&v->vma.anon_vma_chain);
	v->vma.vm_mm = &lx_mm;
	v->vma.vm_start = addr;
	v->vma.vm_end = addr + len;
	vm_flags_init(&v->vma, vm_flags);
	v->vma.vm_page_prot = vm_get_page_prot(vm_flags);
	v->vma.vm_pgoff = pgoff;
	v->vma.vm_file = get_file(file);
	v->space = space;
	return v;
}

int nova_lx_vma_mmap(struct lx_vma *v)
{
	struct file *file = v->vma.vm_file;
	int err = file->f_op->mmap(file, &v->vma);

	if (!err)
		lx_vma_link(v);
	return err;
}

/* A vma whose driver mmap failed: only the file reference goes. */
void nova_lx_vma_free(struct lx_vma *v)
{
	if (v->vma.vm_file)
		fput(v->vma.vm_file);
	kfree(v);
}

bool nova_lx_vma_writable(struct lx_vma *v)
{
	return v->vma.vm_flags & VM_WRITE;
}

/* A page fault at `addr` in the mapping: 0 when it is mapped now, else VM_FAULT_* bits. */
u32 nova_lx_vma_fault(struct lx_vma *v, u64 addr, bool write)
{
	struct vm_area_struct *vma = &v->vma;
	struct vm_fault vmf = {
		.vma = vma,
		.gfp_mask = GFP_KERNEL,
		.pgoff = vma->vm_pgoff + ((addr - vma->vm_start) >> PAGE_SHIFT),
		.address = addr & PAGE_MASK,
		.real_address = addr,
		.flags = FAULT_FLAG_USER | (write ? FAULT_FLAG_WRITE : 0),
	};
	vm_fault_t ret;

	if (!vma->vm_ops || !vma->vm_ops->fault)
		return VM_FAULT_SIGBUS;
	ret = vma->vm_ops->fault(&vmf);
	if (ret & VM_FAULT_ERROR)
		return ret;
	if (!(ret & VM_FAULT_NOPAGE) && vmf.page) {
		/* The handler returned a page (with a reference): map it, the mapping keeps
		 * the reference. */
		struct page *page = vmf.page;

		if (ret & VM_FAULT_LOCKED)
			folio_unlock(page_folio(page));
		if (lx_insert(vma, addr, page_to_pfn(page), vma->vm_page_prot, page) != VM_FAULT_NOPAGE) {
			put_page(page);
			return VM_FAULT_OOM;
		}
	}
	return 0;
}

/* munmap of part of a mapping: it now covers [start, end) (vm_pgoff follows the start). */
/*
 * These two are called with Nova's address-space lock held, so they take no lock that a
 * zap (which needs that address-space lock) could hold: plain stores, which no other task
 * can interleave with (Nova's kernel code is not preempted). A zap that read the old
 * values finds the vma gone when it gets the address space (nova_vma_zap checks).
 */
void nova_lx_vma_trim(struct lx_vma *v, u64 start, u64 end)
{
	struct vm_area_struct *vma = &v->vma;

	vma->vm_pgoff += (start - vma->vm_start) >> PAGE_SHIFT;
	vma->vm_start = start;
	vma->vm_end = end;
}

/* Nova no longer maps the vma anywhere (munmap, exit): no more inserts or zaps. */
void nova_lx_vma_detach(struct lx_vma *v)
{
	WRITE_ONCE(v->space, 0);
}

/* After detaching: vm_ops->close, then the file reference goes. */
void nova_lx_vma_close(struct lx_vma *v)
{
	mutex_lock(&lx_vma_lock);
	list_del(&v->node);
	mutex_unlock(&lx_vma_lock);
	if (v->vma.vm_ops && v->vma.vm_ops->close)
		v->vma.vm_ops->close(&v->vma);
	if (v->vma.vm_file)
		fput(v->vma.vm_file);
	kfree(v);
}

/* fork: the same mapping in the child's address space (pages fault in again there). */
struct lx_vma *nova_lx_vma_dup(struct lx_vma *v, u64 space)
{
	struct lx_vma *n = kmemdup(v, sizeof(*v), GFP_KERNEL);

	if (!n)
		return NULL;
	INIT_LIST_HEAD(&n->vma.anon_vma_chain);
	n->space = space;
	if (n->vma.vm_file)
		get_file(n->vma.vm_file);
	if (n->vma.vm_ops && n->vma.vm_ops->open)
		n->vma.vm_ops->open(&n->vma);
	lx_vma_link(n);
	return n;
}

/* A page reference a mapping kept (vm_insert_page, faults that returned pages). */
void nova_lx_put_page(struct page *page)
{
	put_page(page);
}

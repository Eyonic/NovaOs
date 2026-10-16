// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul files: struct file objects, inodes and dentries for the pseudo file systems
 * DRM and dma-buf use (anonymous inodes, kern_mount), a file descriptor table, shmem
 * files for TTM's swap storage, and the few seq_file and mmap entry points they need.
 *
 * DRM hands out file descriptors for buffers (PRIME), fences (sync files) and leases;
 * dma-buf ties a buffer's lifetime to its file. The objects here are real struct files
 * with Linux's reference counting and release order (f_op->release, then the dentry's
 * d_release, then the inode), so Nova's Linux-program fd layer can later wrap them. For
 * now the descriptor table is kernel-internal: nothing in user space sees these numbers.
 *
 * shmem files keep their pages in the mapping's xarray, in RAM: Nova has no swap, so
 * "writing out" a page only unlocks it.
 */
#include <linux/fs.h>
#include <linux/file.h>
#include <linux/anon_inodes.h>
#include <linux/pseudo_fs.h>
#include <linux/fs_context.h>
#include <linux/mount.h>
#include <linux/dcache.h>
#include <linux/shmem_fs.h>
#include <linux/pagemap.h>
#include <linux/seq_file.h>
#include <linux/mm.h>
#include <linux/sched/mm.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>
#include <linux/uaccess.h>
#include <linux/xarray.h>
#include "nova.h"

/* ---------------------------------------------------------------- super blocks and inodes */

static atomic64_t lx_next_ino = ATOMIC64_INIT(1);

static struct super_block *lx_sb_alloc(struct file_system_type *type, unsigned long magic)
{
	struct super_block *sb = kzalloc(sizeof(*sb), GFP_KERNEL);

	if (sb) {
		sb->s_type = type;
		sb->s_magic = magic;
	}
	return sb;
}

struct inode *alloc_anon_inode(struct super_block *sb)
{
	struct inode *inode = kzalloc(sizeof(*inode), GFP_KERNEL);
	struct address_space *mapping;

	if (!inode)
		return ERR_PTR(-ENOMEM);
	inode->i_sb = sb;
	inode->i_ino = atomic64_inc_return(&lx_next_ino);
	inode->i_mode = S_IRUSR | S_IWUSR;
	atomic_set(&inode->i_count, 1);
	spin_lock_init(&inode->i_lock);
	mapping = &inode->i_data;
	mapping->host = inode;
	xa_init_flags(&mapping->i_pages, XA_FLAGS_LOCK_IRQ);
	init_rwsem(&mapping->invalidate_lock);
	init_rwsem(&mapping->i_mmap_rwsem);
	mapping->i_mmap = RB_ROOT_CACHED;
	mapping->gfp_mask = GFP_HIGHUSER_MOVABLE;
	spin_lock_init(&mapping->i_private_lock);
	inode->i_mapping = mapping;
	return inode;
}

static const struct file_operations lx_shmem_fops;
static void lx_shmem_truncate(struct address_space *mapping, pgoff_t start, pgoff_t end);

void iput(struct inode *inode)
{
	if (!inode || !atomic_dec_and_test(&inode->i_count))
		return;
	/* The last reference: free a shmem file's pages with it. */
	if (inode->i_fop == &lx_shmem_fops)
		lx_shmem_truncate(inode->i_mapping, 0, ULONG_MAX);
	xa_destroy(&inode->i_data.i_pages);
	kfree(inode);
}

void inode_set_bytes(struct inode *inode, loff_t bytes)
{
	inode->i_blocks = bytes >> 9;
	inode->i_bytes = bytes & 511;
}

/* ---------------------------------------------------------------- pseudo file systems */

/* Mounting a pseudo file system: run its init_fs_context (which calls init_pseudo) to
 * learn its magic number and operations, and make a super block from them. */
struct pseudo_fs_context *init_pseudo(struct fs_context *fc, unsigned long magic)
{
	struct pseudo_fs_context *ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);

	if (ctx) {
		ctx->magic = magic;
		fc->fs_private = ctx;
	}
	return ctx;
}

struct vfsmount *kern_mount(struct file_system_type *type)
{
	struct fs_context fc = { .fs_type = type };
	struct pseudo_fs_context *ctx;
	struct vfsmount *mnt;
	int err;

	if (type->init_fs_context) {
		err = type->init_fs_context(&fc);
		if (err)
			return ERR_PTR(err);
	}
	ctx = fc.fs_private;
	mnt = kzalloc(sizeof(*mnt), GFP_KERNEL);
	if (mnt)
		mnt->mnt_sb = lx_sb_alloc(type, ctx ? ctx->magic : 0);
	if (!mnt || !mnt->mnt_sb) {
		kfree(mnt);
		kfree(ctx);
		return ERR_PTR(-ENOMEM);
	}
	if (ctx) {
		mnt->mnt_sb->s_op = ctx->ops;
		mnt->mnt_sb->__s_d_op = ctx->dops;
	}
	kfree(ctx);
	return mnt;
}

void kern_unmount(struct vfsmount *mnt)
{
	if (IS_ERR_OR_NULL(mnt))
		return;
	kfree(mnt->mnt_sb);
	kfree(mnt);
}

int simple_pin_fs(struct file_system_type *type, struct vfsmount **mount, int *count)
{
	if (!*mount) {
		struct vfsmount *mnt = kern_mount(type);

		if (IS_ERR(mnt))
			return PTR_ERR(mnt);
		*mount = mnt;
	}
	++*count;
	return 0;
}

void simple_release_fs(struct vfsmount **mount, int *count)
{
	if (--*count == 0) {
		kern_unmount(*mount);
		*mount = NULL;
	}
}

void kill_anon_super(struct super_block *sb)
{
}

/* ---------------------------------------------------------------- dentries and files */

static struct dentry *lx_dentry_alloc(struct super_block *sb, struct inode *inode, const char *name)
{
	struct dentry *dentry = kzalloc(sizeof(*dentry), GFP_KERNEL);

	if (!dentry)
		return NULL;
	dentry->__d_name.name = kstrdup(name, GFP_KERNEL);
	dentry->__d_name.len = strlen(name);
	dentry->d_inode = inode;
	dentry->d_sb = sb;
	dentry->d_op = sb ? sb->__s_d_op : NULL;
	dentry->d_parent = dentry;
	dentry->d_lockref.count = 1;
	return dentry;
}

/* Drop a dentry reference: the last one releases it (dma-buf frees its buffer in
 * d_release) and its inode reference. */
static void lx_dput(struct dentry *dentry)
{
	if (!dentry || --dentry->d_lockref.count > 0)
		return;
	if (dentry->d_op && dentry->d_op->d_release)
		dentry->d_op->d_release(dentry);
	iput(dentry->d_inode);
	kfree(dentry->__d_name.name);
	kfree(dentry);
}

static struct file *lx_file_alloc(const struct path *path, int flags, const struct file_operations *fops)
{
	struct file *file = kzalloc(sizeof(*file), GFP_KERNEL);

	if (!file)
		return ERR_PTR(-ENOMEM);
	spin_lock_init(&file->f_lock);
	mutex_init(&file->f_pos_lock);
	file_ref_init(&file->f_ref, 1);
	file->__f_path = *path;
	file->f_inode = path->dentry->d_inode;
	file->f_mapping = file->f_inode->i_mapping;
	file->f_flags = flags;
	file->f_mode = OPEN_FMODE(flags);
	file->f_op = fops;
	return file;
}

struct file *alloc_file_pseudo(struct inode *inode, struct vfsmount *mnt, const char *name, int flags,
			       const struct file_operations *fops)
{
	struct path path = { .mnt = mnt };
	struct file *file;

	path.dentry = lx_dentry_alloc(mnt ? mnt->mnt_sb : inode->i_sb, inode, name);
	if (!path.dentry)
		return ERR_PTR(-ENOMEM);
	file = lx_file_alloc(&path, flags, fops);
	if (IS_ERR(file)) {
		/* The caller still owns its inode reference on failure. */
		path.dentry->d_inode = NULL;
		lx_dput(path.dentry);
	}
	return file;
}

/* Every anonymous-inode file shares one inode, as on Linux. */
static struct inode *lx_anon_inode;

struct file *anon_inode_getfile(const char *name, const struct file_operations *fops, void *priv, int flags)
{
	struct file *file;

	if (!lx_anon_inode) {
		struct inode *inode = alloc_anon_inode(NULL);

		if (IS_ERR(inode))
			return ERR_CAST(inode);
		lx_anon_inode = inode;
	}
	atomic_inc(&lx_anon_inode->i_count);
	file = alloc_file_pseudo(lx_anon_inode, NULL, name, flags & (O_ACCMODE | O_NONBLOCK), fops);
	if (IS_ERR(file)) {
		iput(lx_anon_inode);
		return file;
	}
	file->private_data = priv;
	return file;
}

/* A second open file on an existing path (DRM leases): runs the file's open method. */
struct file *dentry_open(const struct path *path, int flags, const struct cred *creds)
{
	struct inode *inode = path->dentry->d_inode;
	const struct file_operations *fops = inode->i_fop;
	struct file *file;
	int err;

	if (!fops)
		return ERR_PTR(-ENODEV);
	file = lx_file_alloc(path, flags, fops);
	if (IS_ERR(file))
		return file;
	path->dentry->d_lockref.count++;
	atomic_inc(&inode->i_count);
	if (fops->open) {
		err = fops->open(inode, file);
		if (err) {
			file->f_op = NULL;
			fput(file);
			return ERR_PTR(err);
		}
	}
	return file;
}

void fput(struct file *file)
{
	if (!atomic_long_add_negative(-1, &file->f_ref.refcnt))
		return;
	if (file->f_op && file->f_op->release)
		file->f_op->release(file->f_inode, file);
	lx_dput(file->f_path.dentry);
	kfree(file);
}

char *dynamic_dname(char *buffer, int buflen, const char *fmt, ...)
{
	char temp[64];
	va_list args;
	int sz;

	va_start(args, fmt);
	sz = vsnprintf(temp, sizeof(temp), fmt, args) + 1;
	va_end(args);
	if (sz > sizeof(temp) || sz > buflen)
		return ERR_PTR(-ENAMETOOLONG);
	buffer += buflen - sz;
	return memcpy(buffer, temp, sz);
}

loff_t default_llseek(struct file *file, loff_t offset, int whence)
{
	switch (whence) {
	case SEEK_END:
		offset += i_size_read(file_inode(file));
		break;
	case SEEK_CUR:
		offset += file->f_pos;
		break;
	case SEEK_SET:
		break;
	default:
		return -EINVAL;
	}
	if (offset < 0)
		return -EINVAL;
	file->f_pos = offset;
	return offset;
}

/* ---------------------------------------------------------------- file descriptors */

/* One table for the whole kernel until Linux programs can receive these files. */
#define LX_FDS 256
static struct file *lx_fds[LX_FDS];
static DECLARE_BITMAP(lx_fd_used, LX_FDS);
static DEFINE_SPINLOCK(lx_fd_lock);

int get_unused_fd_flags(unsigned flags)
{
	unsigned int fd;

	spin_lock(&lx_fd_lock);
	fd = find_next_zero_bit(lx_fd_used, LX_FDS, 3);
	if (fd < LX_FDS)
		__set_bit(fd, lx_fd_used);
	spin_unlock(&lx_fd_lock);
	return fd < LX_FDS ? fd : -EMFILE;
}

void put_unused_fd(unsigned int fd)
{
	if (fd < LX_FDS)
		clear_bit(fd, lx_fd_used);
}

void fd_install(unsigned int fd, struct file *file)
{
	if (WARN_ON(fd >= LX_FDS))
		return;
	spin_lock(&lx_fd_lock);
	lx_fds[fd] = file;
	spin_unlock(&lx_fd_lock);
}

struct file *fget(unsigned int fd)
{
	struct file *file = NULL;

	spin_lock(&lx_fd_lock);
	if (fd < LX_FDS && lx_fds[fd]) {
		file = lx_fds[fd];
		get_file(file);
	}
	spin_unlock(&lx_fd_lock);
	return file;
}

struct fd fdget(unsigned int fd)
{
	struct file *file = fget(fd);

	return file ? CLONED_FD(file) : EMPTY_FD;
}

/* ---------------------------------------------------------------- shmem */

static int lx_shmem_release(struct inode *inode, struct file *file)
{
	return 0;
}

static const struct file_operations lx_shmem_fops = {
	.release = lx_shmem_release,
};

struct file *shmem_file_setup(const char *name, loff_t size, vma_flags_t flags)
{
	struct inode *inode = alloc_anon_inode(NULL);
	struct file *file;

	if (IS_ERR(inode))
		return ERR_CAST(inode);
	inode->i_mode = S_IFREG | S_IRUSR | S_IWUSR;
	inode->i_size = size;
	inode->i_fop = &lx_shmem_fops;
	file = alloc_file_pseudo(inode, NULL, name, O_RDWR, &lx_shmem_fops);
	if (IS_ERR(file))
		iput(inode);
	return file;
}

struct folio *shmem_read_folio_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	struct folio *folio;
	void *old;

	folio = xa_load(&mapping->i_pages, index);
	if (folio) {
		folio_get(folio);
		return folio;
	}
	folio = (struct folio *)alloc_pages(gfp | __GFP_ZERO, 0);
	if (!folio)
		return ERR_PTR(-ENOMEM);
	/* One reference for the mapping, one for the caller. */
	old = xa_store(&mapping->i_pages, index, folio, GFP_KERNEL);
	if (xa_is_err(old)) {
		folio_put(folio);
		return ERR_PTR(xa_err(old));
	}
	folio->mapping = mapping;
	folio->index = index;
	mapping->nrpages++;
	folio_get(folio);
	return folio;
}

struct page *shmem_read_mapping_page_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	struct folio *folio = shmem_read_folio_gfp(mapping, index, gfp);

	return IS_ERR(folio) ? ERR_CAST(folio) : &folio->page;
}

static void lx_shmem_truncate(struct address_space *mapping, pgoff_t start, pgoff_t end)
{
	struct folio *folio;
	unsigned long index;

	xa_for_each_range(&mapping->i_pages, index, folio, start, end) {
		xa_erase(&mapping->i_pages, index);
		folio->mapping = NULL;
		mapping->nrpages--;
		folio_put(folio);
	}
}

void shmem_truncate_range(struct inode *inode, loff_t start, uoff_t end)
{
	lx_shmem_truncate(inode->i_mapping, start >> PAGE_SHIFT, end >> PAGE_SHIFT);
}

/* No swap: the page stays where it is. Like a successful writeout, unlock it. */
int shmem_writeout(struct folio *folio, struct swap_iocb **plug, struct list_head *folio_list)
{
	folio_unlock(folio);
	return 0;
}

/* ---------------------------------------------------------------- user memory, mmap */

char *strndup_user(const char __user *s, long n)
{
	char *p = kmalloc(n + 1, GFP_KERNEL);

	if (!p)
		return ERR_PTR(-ENOMEM);
	if (copy_from_user(p, s, n)) {
		kfree(p);
		return ERR_PTR(-EFAULT);
	}
	p[n] = '\0';
	return p;
}

void *vmemdup_user(const void __user *src, size_t len)
{
	void *p = kvmalloc(len, GFP_USER);

	if (!p)
		return ERR_PTR(-ENOMEM);
	if (copy_from_user(p, src, len)) {
		kvfree(p);
		return ERR_PTR(-EFAULT);
	}
	return p;
}

/*
 * Mapping GPU buffers into Linux programs comes with DRM's user interface (milestone 8).
 * Until then no Linux file here is ever mmap()ed, so there is nothing to zap and no
 * fault to serve.
 */
void unmap_mapping_range(struct address_space *mapping, loff_t const holebegin, loff_t const holelen,
			 int even_cows)
{
}

void vma_set_file(struct vm_area_struct *vma, struct file *file)
{
	get_file(file);
	swap(vma->vm_file, file);
	if (file)
		fput(file);
}

vm_fault_t vmf_insert_pfn_prot(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn,
			       pgprot_t pgprot)
{
	return VM_FAULT_SIGBUS;
}

unsigned long mm_get_unmapped_area(struct file *filp, unsigned long addr, unsigned long len,
				   unsigned long pgoff, unsigned long flags)
{
	return -ENOMEM;
}

pgprot_t vm_get_page_prot(vm_flags_t vm_flags)
{
	pteval_t val = _PAGE_PRESENT | _PAGE_USER | _PAGE_ACCESSED;

	if (vm_flags & VM_WRITE)
		val |= _PAGE_RW;
	if (!(vm_flags & VM_EXEC))
		val |= _PAGE_NX;
	return __pgprot(val);
}

/* ---------------------------------------------------------------- seq_file */

/* Only debugfs and /proc files print through seq_file; Nova has neither, but the
 * buffer logic is simple enough to keep correct. */
static void lx_seq_write(struct seq_file *m, const char *s, size_t len)
{
	if (m->count + len > m->size) {
		m->count = m->size;
		return;
	}
	memcpy(m->buf + m->count, s, len);
	m->count += len;
}

void seq_printf(struct seq_file *m, const char *fmt, ...)
{
	va_list args;
	int len;

	if (m->count >= m->size)
		return;
	va_start(args, fmt);
	len = vsnprintf(m->buf + m->count, m->size - m->count, fmt, args);
	va_end(args);
	m->count = len < m->size - m->count ? m->count + len : m->size;
}

void seq_putc(struct seq_file *m, char c)
{
	lx_seq_write(m, &c, 1);
}

void __seq_puts(struct seq_file *m, const char *s)
{
	lx_seq_write(m, s, strlen(s));
}

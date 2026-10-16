// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul memory management: page allocation with real struct pages, vmalloc and vmap,
 * folios, page attributes, memory statistics, LRU lists and shrinkers.
 *
 * Pages come from Nova's heap (direct-mapped, physically contiguous, naturally aligned
 * for their order), so page_address() and page_to_phys() are Linux's own arithmetic.
 * Their struct page entries live at vmemmap_base + pfn, as on Linux; Nova backs the part
 * of that array a new allocation uses with memory (nova_vmemmap_populate). A page is
 * freed when its reference count drops to zero, through Linux's put_page() path.
 *
 * Non-compound allocations of order > 0 get a reference on the first page only, as on
 * Linux. Compound pages (__GFP_COMP, folios larger than one page) are not supported:
 * such requests fail, and their callers (TTM's page pool) fall back to smaller orders.
 *
 * vmap() maps arbitrary pages into a window of Nova's address space (nova_vmap), for
 * TTM's kernel mappings of buffer objects. Page attributes: without CONFIG_X86_PAT,
 * Linux code asks for at most UC- (PCD) instead of write-combining, and the direct map
 * stays write-back (set_pages_array_*() change nothing; amdgpu only asks for uncached
 * system pages when it can use PAT).
 */
#include <linux/gfp.h>
#include <linux/mm.h>
#include <linux/mmzone.h>
#include <linux/vmalloc.h>
#include <linux/vmstat.h>
#include <linux/slab.h>
#include <linux/io.h>
#include <linux/pagemap.h>
#include <linux/swap.h>
#include <linux/folio_batch.h>
#include <linux/list_lru.h>
#include <linux/shrinker.h>
#include <asm/set_memory.h>
#include <asm/pgtable_types.h>
#include "nova.h"

bool nova_vmemmap_populate(u64 pfn, u64 count, u64 page_size);

struct page *__alloc_pages_noprof(gfp_t gfp, unsigned int order, int preferred_nid, nodemask_t *nodemask)
{
	size_t size = PAGE_SIZE << order;
	unsigned long pfn, i;
	struct page *page;
	void *mem;

	if ((gfp & __GFP_COMP) && order)
		return NULL;
	mem = nova_alloc(size, size, gfp & __GFP_ZERO, gfp & (GFP_DMA32 | GFP_DMA));
	if (!mem)
		return NULL;
	pfn = __pa(mem) >> PAGE_SHIFT;
	if (!nova_vmemmap_populate(pfn, 1UL << order, sizeof(struct page))) {
		nova_free(mem);
		return NULL;
	}
	page = pfn_to_page(pfn);
	for (i = 0; i < (1UL << order); i++) {
		memset(&page[i], 0, sizeof(page[i]));
		INIT_LIST_HEAD(&page[i].lru);
	}
	set_page_count(page, 1);
	return page;
}

struct folio *__folio_alloc_noprof(gfp_t gfp, unsigned int order, int preferred_nid, nodemask_t *nodemask)
{
	return (struct folio *)__alloc_pages_noprof(gfp & ~__GFP_COMP, order, preferred_nid, nodemask);
}

/* Give a page run back to Nova. A page split off a larger allocation (split_page) is
 * not an allocation of its own: it stays allocated, since Nova's heap cannot free part
 * of a block. */
static void lx_pages_free(struct page *page)
{
	void *addr = page_address(page);

	if (nova_alloc_size(addr))
		nova_free(addr);
	else
		pr_warn_once("lx_emul: page %px was split from a larger allocation; not freed\n", addr);
}

/* The last reference went away (put_page / folio_put). */
void __folio_put(struct folio *folio)
{
	lx_pages_free(&folio->page);
}

void __free_pages(struct page *page, unsigned int order)
{
	if (put_page_testzero(page))
		lx_pages_free(page);
}

/* Turn an order-n allocation into 2^n pages with their own references. */
void split_page(struct page *page, unsigned int order)
{
	unsigned long i;

	for (i = 1; i < (1UL << order); i++)
		set_page_count(page + i, 1);
}

void free_pages(unsigned long addr, unsigned int order)
{
	if (addr)
		__free_pages(virt_to_page((void *)addr), order);
}

unsigned long get_free_pages_noprof(gfp_t gfp_mask, unsigned int order)
{
	struct page *page = __alloc_pages_noprof(gfp_mask & ~__GFP_HIGHMEM, order, NUMA_NO_NODE, NULL);

	return page ? (unsigned long)page_address(page) : 0;
}

unsigned long get_zeroed_page_noprof(gfp_t gfp_mask)
{
	return get_free_pages_noprof(gfp_mask | __GFP_ZERO, 0);
}

/* vmalloc: large allocations come from the heap and are physically contiguous too,
 * which is more than vmalloc promises. */
void *vmalloc_noprof(unsigned long size)
{
	return nova_alloc(size, PAGE_SIZE, false, false);
}

void *vzalloc_noprof(unsigned long size)
{
	return nova_alloc(size, PAGE_SIZE, true, false);
}

void vfree(const void *addr)
{
	if (addr)
		nova_free(addr);
}

void *kvrealloc_node_align_noprof(const void *p, DECL_TOKEN_PARAMS(size, token), unsigned long align,
				  gfp_t flags, int nid)
{
	/* kvmalloc memory is heap memory here, like kmalloc memory. */
	return krealloc_node_align_noprof(p, PASS_TOKEN_PARAMS(size, token), align, flags, nid);
}

/* Allocations are exactly as large as requested (plus alignment padding nobody may use). */
size_t kmalloc_size_roundup(size_t size)
{
	return size;
}

unsigned int kmem_cache_size(struct kmem_cache *s)
{
	return s->size;
}

/* ---------------------------------------------------------------- folios */

/* Folio locks here guard TTM's backup copies, which never sleep while holding one, so
 * waiting for a lock is a matter of letting its holder run. */
void __folio_lock(struct folio *folio)
{
	while (!folio_trylock(folio))
		nova_yield();
}

void folio_unlock(struct folio *folio)
{
	clear_bit_unlock(PG_locked, folio_flags(folio, 0));
}

bool folio_mark_dirty(struct folio *folio)
{
	return !folio_test_set_dirty(folio);
}

bool set_page_dirty(struct page *page)
{
	return folio_mark_dirty(page_folio(page));
}

bool folio_clear_dirty_for_io(struct folio *folio)
{
	return folio_test_clear_dirty(folio);
}

/* No page reclaim: access and evictability hints have nothing to update. */
void folio_mark_accessed(struct folio *folio)
{
}

void mark_page_accessed(struct page *page)
{
}

void check_move_unevictable_folios(struct folio_batch *fbatch)
{
}

void __folio_batch_release(struct folio_batch *fbatch)
{
	unsigned int i;

	for (i = 0; i < folio_batch_count(fbatch); i++)
		folio_put(fbatch->folios[i]);
	folio_batch_reinit(fbatch);
}

/* ---------------------------------------------------------------- memory size */

atomic_long_t _totalram_pages;
atomic_long_t vm_node_stat[NR_VM_NODE_STAT_ITEMS];
struct pglist_data contig_page_data;


void lx_emul_mm_init(void)
{
	u64 total, free;

	nova_mem_info(&total, &free);
	atomic_long_set(&_totalram_pages, total >> PAGE_SHIFT);
}

void si_meminfo(struct sysinfo *val)
{
	u64 total, free;

	nova_mem_info(&total, &free);
	memset(val, 0, sizeof(*val));
	val->totalram = total >> PAGE_SHIFT;
	val->freeram = free >> PAGE_SHIFT;
	val->mem_unit = PAGE_SIZE;
}

int page_is_ram(unsigned long pfn)
{
	return nova_phys_is_ram((u64)pfn << PAGE_SHIFT);
}

/* ---------------------------------------------------------------- page attributes */

pteval_t __default_kernel_pte_mask __read_mostly = ~0;

/* Linux's table for a CPU without PAT support: write-combining degrades to UC-. */
unsigned long cachemode2protval(enum page_cache_mode pcm)
{
	switch (pcm) {
	case _PAGE_CACHE_MODE_WB:
		return 0;
	case _PAGE_CACHE_MODE_UC:
		return _PAGE_PCD | _PAGE_PWT;
	default:
		return _PAGE_PCD;
	}
}

pgprot_t pgprot_writecombine(pgprot_t prot)
{
	return __pgprot(pgprot_val(prot) | cachemode2protval(_PAGE_CACHE_MODE_WC));
}

int set_pages_array_wc(struct page **pages, int numpages)
{
	return 0;
}

int set_pages_array_uc(struct page **pages, int numpages)
{
	return 0;
}

int set_pages_array_wb(struct page **pages, int numpages)
{
	return 0;
}

int set_pages_wb(struct page *page, int numpages)
{
	return 0;
}

/* ---------------------------------------------------------------- vmap, memremap */


static void *lx_vmap_phys(const u64 *phys, size_t count, pgprot_t prot)
{
	return nova_vmap(phys, count, pgprot_val(prot) & (_PAGE_PWT | _PAGE_PCD));
}

void *vmap(struct page **pages, unsigned int count, unsigned long flags, pgprot_t prot)
{
	unsigned int i;
	void *addr;
	u64 *phys;

	/* Physically contiguous, cached pages are in the direct map already. */
	for (i = 1; i < count; i++)
		if (page_to_pfn(pages[i]) != page_to_pfn(pages[0]) + i)
			break;
	if (i == count && !(pgprot_val(prot) & (_PAGE_PWT | _PAGE_PCD)))
		return page_address(pages[0]);
	phys = kvmalloc_array(count, sizeof(*phys), GFP_KERNEL);
	if (!phys)
		return NULL;
	for (i = 0; i < count; i++)
		phys[i] = page_to_phys(pages[i]);
	addr = lx_vmap_phys(phys, count, prot);
	kvfree(phys);
	return addr;
}

void vunmap(const void *addr)
{
	/* Direct-map addresses (contiguous vmaps) need no unmapping. */
	if (addr && (unsigned long)addr >= VMALLOC_START && (unsigned long)addr < VMALLOC_END)
		nova_vunmap(addr);
}

void *memremap(resource_size_t offset, size_t size, unsigned long flags)
{
	size_t count = PAGE_ALIGN(size + offset_in_page(offset)) >> PAGE_SHIFT, i;
	pgprot_t prot = (flags & MEMREMAP_WB) ? PAGE_KERNEL : pgprot_writecombine(PAGE_KERNEL);
	void *addr;
	u64 *phys;

	if ((flags & MEMREMAP_WB) && page_is_ram(PHYS_PFN(offset)))
		return __va(offset);
	phys = kvmalloc_array(count, sizeof(*phys), GFP_KERNEL);
	if (!phys)
		return NULL;
	for (i = 0; i < count; i++)
		phys[i] = (offset & PAGE_MASK) + i * PAGE_SIZE;
	addr = lx_vmap_phys(phys, count, prot);
	kvfree(phys);
	return addr ? addr + offset_in_page(offset) : NULL;
}

void memunmap(void *addr)
{
	vunmap((void *)((unsigned long)addr & PAGE_MASK));
}

/* ---------------------------------------------------------------- LRU lists, shrinkers */

/*
 * Nova does not reclaim memory under pressure, so shrinkers are registered but never
 * called. A list_lru keeps its items on its one node's list, which TTM's page pool walks
 * itself when it trims its caches.
 */
int __list_lru_init(struct list_lru *lru, bool memcg_aware, struct shrinker *shrinker)
{
	lru->node = kzalloc(sizeof(*lru->node), GFP_KERNEL);
	if (!lru->node)
		return -ENOMEM;
	INIT_LIST_HEAD(&lru->node->lru.list);
	spin_lock_init(&lru->node->lru.lock);
	return 0;
}

bool list_lru_add(struct list_lru *lru, struct list_head *item, int nid, struct mem_cgroup *memcg)
{
	struct list_lru_one *l = &lru->node->lru;

	spin_lock(&l->lock);
	if (!list_empty(item)) {
		spin_unlock(&l->lock);
		return false;
	}
	list_add_tail(item, &l->list);
	l->nr_items++;
	atomic_long_inc(&lru->node->nr_items);
	spin_unlock(&l->lock);
	return true;
}

void list_lru_isolate(struct list_lru_one *list, struct list_head *item)
{
	list_del_init(item);
	list->nr_items--;
}

void list_lru_isolate_move(struct list_lru_one *list, struct list_head *item, struct list_head *head)
{
	list_move(item, head);
	list->nr_items--;
}

unsigned long list_lru_walk_node(struct list_lru *lru, int nid, list_lru_walk_cb isolate, void *cb_arg,
				 unsigned long *nr_to_walk)
{
	struct list_lru_one *l = &lru->node->lru;
	struct list_head *item, *n;
	unsigned long isolated = 0;

	spin_lock(&l->lock);
restart:
	list_for_each_safe(item, n, &l->list) {
		if (!*nr_to_walk)
			break;
		--*nr_to_walk;
		switch (isolate(item, l, cb_arg)) {
		case LRU_REMOVED_RETRY:
			isolated++;
			atomic_long_dec(&lru->node->nr_items);
			goto restart;
		case LRU_REMOVED:
			isolated++;
			atomic_long_dec(&lru->node->nr_items);
			break;
		case LRU_ROTATE:
			list_move_tail(item, &l->list);
			break;
		case LRU_RETRY:
			goto restart;
		case LRU_STOP:
			goto out;
		default:
			break;
		}
	}
out:
	spin_unlock(&l->lock);
	return isolated;
}

struct shrinker *shrinker_alloc(unsigned int flags, const char *fmt, ...)
{
	return kzalloc(sizeof(struct shrinker), GFP_KERNEL);
}

void shrinker_register(struct shrinker *shrinker)
{
}

void shrinker_free(struct shrinker *shrinker)
{
	kfree(shrinker);
}

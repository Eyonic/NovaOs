// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul memory management: page allocation with real struct pages, and vmalloc.
 *
 * Pages come from Nova's heap (direct-mapped, physically contiguous, naturally aligned
 * for their order), so page_address() and page_to_phys() are Linux's own arithmetic.
 * Their struct page entries live at vmemmap_base + pfn, as on Linux; Nova backs the part
 * of that array a new allocation uses with memory (nova_vmemmap_populate). A page is
 * freed when its reference count drops to zero, through Linux's put_page() path.
 *
 * Non-compound allocations of order > 0 get a reference on the first page only, as on
 * Linux. Compound pages (__GFP_COMP, folios larger than one page) arrive with DRM/TTM.
 */
#include <linux/gfp.h>
#include <linux/mm.h>
#include <linux/vmalloc.h>
#include <linux/slab.h>
#include "nova.h"

bool nova_vmemmap_populate(u64 pfn, u64 count, u64 page_size);

struct page *__alloc_pages_noprof(gfp_t gfp, unsigned int order, int preferred_nid, nodemask_t *nodemask)
{
	size_t size = PAGE_SIZE << order;
	unsigned long pfn, i;
	struct page *page;
	void *mem;

	if (WARN_ON_ONCE((gfp & __GFP_COMP) && order))
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

/* The last reference went away (put_page / folio_put). */
void __folio_put(struct folio *folio)
{
	nova_free(folio_address(folio));
}

void __free_pages(struct page *page, unsigned int order)
{
	if (put_page_testzero(page))
		nova_free(page_address(page));
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

/* vmalloc: Nova has no separate virtual area; large allocations come from the heap and
 * are physically contiguous too, which is more than vmalloc promises. */
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

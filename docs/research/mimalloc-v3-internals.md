# mimalloc v3.5.1 internals, read for a single-threaded wasm32 Rust port

Source: `/home/bpowers/src/mimalloc-v3` (v3.5.1, `MI_MALLOC_VERSION 30501`, include/mimalloc.h:11).
All `file:line` references below are into that tree. The v2.2.7 checkout at
`/home/bpowers/src/mimalloc` is only used for a few contrasts.

Two build configurations are tracked throughout:

- "x64 default": 64-bit, release (`NDEBUG`), no secure/padding/guarded. This is the
  configuration the mimalloc authors tune for. In it `MI_PAGE_META_IS_ALIGNED=1`,
  `MI_PAGE_META_IS_SEPARATED=1`, `MI_PAGE_MAP_FLAT=0`.
- "wasm32 wasi": what `cmake -DCMAKE_SYSTEM_NAME=WASI` produces. CMakeLists.txt:476-479
  forces `MI_FREE_USE_PAGEMAP=ON`, which in bits.h:140-146 selects
  `MI_PAGE_MAP_FLAT=1` (because `MI_MAX_VABITS` is 32, bits.h:126-128), which in
  types.h:135-141 forces `MI_PAGE_META_IS_SEPARATED=0` and leaves
  `MI_PAGE_META_IS_ALIGNED` undefined (types.h:147-154 only defines it when
  `MI_FREE_USE_PAGEMAP` is off). So on wasm the page header lives in-band at the
  start of the page's first slice, and every `free` goes through the flat page map.

Struct sizes and derived constants were verified by compiling probes against the
headers (host x64 with `cc`, and `clang --target=wasm32-wasip1 -fsyntax-only`).

## 0. Constant table

| constant | where | x64 default | wasm32 wasi |
|---|---|---|---|
| `MI_INTPTR_SIZE` / `MI_SIZE_SIZE` | bits.h:65,68 | 8 | 4 |
| `MI_MAX_ALIGN_SIZE` | types.h:37-39 | 16 | 16 |
| `MI_ARENA_SLICE_SHIFT` = 13 + `MI_SIZE_SHIFT` | types.h:195 | 16 (64 KiB) | 15 (32 KiB) |
| `MI_BCHUNK_BITS` | types.h:202-212 | 512 | 256 |
| `MI_ARENA_CHUNK_SIZE` = bits x slice | types.h:215 | 32 MiB | 8 MiB |
| `MI_SMALL_PAGE_SIZE` = 1 slice | types.h:227 | 64 KiB | 32 KiB |
| `MI_MEDIUM_PAGE_SIZE` = 8 slices | types.h:228 | 512 KiB | 256 KiB |
| `MI_LARGE_PAGE_SIZE` = `MI_SIZE_SIZE` x medium | types.h:229 | 4 MiB (64 slices) | 1 MiB (32 slices) |
| `MI_SMALL_MAX_OBJ_SIZE` = (small page - 4 KiB)/6 | types.h:470 | 10240 | 4778 |
| `MI_MEDIUM_MAX_OBJ_SIZE` = (medium page - 4 KiB)/6 | types.h:472 | 86698 | 43008 |
| `MI_LARGE_MAX_OBJ_SIZE` = large page / 8 | types.h:473 | 524288 | 131072 |
| `MI_LARGE_MAX_OBJ_WSIZE` | types.h:478 | 65536 | 32768 |
| `MI_MAX_SINGLETON_BIN` | types.h:485-493 | 60 | 56 |
| `MI_ARENA_BIN_COUNT` = singleton bin + 1 | types.h:715 | 61 | 57 |
| `MI_SMALL_WSIZE_MAX` / `MI_SMALL_SIZE_MAX` | mimalloc.h:122-123 | 128 / 1024 | 128 / 512 |
| `MI_PAGES_DIRECT` | types.h:557 | 129 | 129 |
| `MI_BIN_HUGE` / `MI_BIN_FULL` / `MI_BIN_COUNT` | mimalloc-stats.h:97, types.h:236-237 | 73 / 74 / 75 | same |
| `MI_ARENA_MIN_SIZE` = 1 chunk | types.h:716 | 32 MiB | 8 MiB |
| `MI_ARENA_MAX_SIZE` = 512 chunks | types.h:717, bitmap.h:103 | 16 GiB | 2 GiB |
| `MI_ARENA_MAX_CHUNK_OBJ_SIZE` | types.h:221 | 32 MiB | 8 MiB |
| `MI_ARENA_ALIGNMENT` | types.h:245-251 | 256 MiB (meta aligned) | 32 KiB (= slice) |
| `MI_PAGE_ALIGN` = slice align | types.h:463 | 64 KiB | 32 KiB |
| `MI_PAGE_MAX_OVERALLOC_ALIGN` | types.h:467 | 64 KiB | 32 KiB |
| `MI_PAGE_MAX_START_BLOCK_ALIGN2` | types.h:465 | 4 KiB | 4 KiB |
| `MI_PAGE_MIN_COMMIT_SIZE` | types.h:243 | 16 KiB | 16 KiB |
| `MI_MAX_VABITS` / `MI_MIN_VABITS` | bits.h:119-137 | 47 / 43 | 32 / 32 |
| `sizeof(mi_page_t)` | types.h:425-456 | 128 (120 without `self`) | 72 |
| `mi_page_info_size()` = align_up(sizeof page, 16) | internal.h:839-841 | 128 | 80 |
| `sizeof(mi_theap_t)` | types.h:561-597 | 8096 | 6312 |
| `sizeof(mi_heap_t)` | types.h:617-638 | 6464 | 5368 |
| `sizeof(mi_stats_t)` | mimalloc-stats.h:101-116 | 4368 | 4360 |
| `sizeof(mi_page_queue_t)` | types.h:528-533 | 32 | 16 |
| `sizeof(mi_arena_t)` | types.h:730-757 | 648 | 328 |
| `sizeof(mi_bchunk_t)` / `mi_bitmap_t` / `mi_bbitmap_t` (with one chunk) | bitmap.h:85-114, 260-270 | 64 / 192 / 512 | 32 / 96 / 256 |
| default `arena_reserve` (KiB option) | options.c:51-57 | 1 GiB | 128 MiB, then /4 = 32 MiB (arena.c:350-352) |
| default `arena_max_object_size` | options.c:59-61 | 2 GiB | 256 MiB |

Defaults from options.c:113-179 that matter for the algorithms below: `purge_delay`
1000 ms, `arena_purge_mult` 4 (so arena purges are delayed 4 s, arena.c:2252-2262),
`page_full_retain` 2, `page_max_candidates` 4, `generic_collect` 10000,
`page_reclaim_on_free` 0, `page_max_reclaim` -1 (unlimited), `page_cross_thread_max_reclaim`
32, `page_commit_on_demand` 0, `arena_eager_commit` 2, `purge_decommits` 1.

## 1. Core data structures

### 1.1 Blocks and the free list encoding

A free block is just a `mi_block_t { mi_encoded_t next; }` (types.h:366-368), one
machine word overlaid on the block's own memory. With `MI_SECURE < 3` and `MI_DEBUG == 0`
`MI_ENCODE_FREELIST` is not defined (types.h:110-112) and `mi_block_next` /
`mi_block_set_next` are plain loads and stores of `block->next`
(internal.h:1257-1305). Only when `MI_ENCODE_FREELIST` is on does the page carry
`keys[MI_PAGE_KEY_COUNT]` (types.h:451-455) and the next pointer is stored as
`rotl(p ^ k2, k1) + k1` (internal.h:1216-1236) with a corrupted-list check that the
decoded pointer lies inside the page (internal.h:1283-1296). For the port: a raw
pointer, nothing else.

### 1.2 `mi_page_t` (types.h:425-456)

The layout is "optimized for `free.c:mi_free` and `alloc.c:mi_page_alloc`" (types.h:423):
the first 64 bytes on x64 hold everything the two fast paths touch.

| field | type | offset x64 (aligned/pagemap) | offset wasm32 | purpose | MT-only? |
|---|---|---|---|---|---|
| `self` | `_Atomic(mi_page_t*)` | 0 / absent | absent | only with `MI_PAGE_META_IS_ALIGNED`: each slice's meta slot points to the real page struct of the (possibly multi-slice) page (arena.c:1085-1094) | no, but only for the aligned-meta scheme |
| `xthread_id` | `_Atomic(size_t)` | 8 / 0 | 0 | owning thread id with two flag bits in the low bits: `MI_PAGE_IN_FULL_QUEUE=1`, `MI_PAGE_HAS_INTERIOR_POINTERS=2` (types.h:374-378). 0 means abandoned, 4 abandoned-and-mapped, 8 detached (types.h:384-386). Compared against the caller's thread id in one XOR in `mi_free_nonnull` (free.c:241-243) | thread id part yes; the two flags no |
| `free` | `mi_block_t*` | 16 / 8 | 4 | list of blocks malloc pops from | no |
| `used` | `size_t` | 24 / 16 | 8 | blocks in use, including ones sitting in `xthread_free` (types.h:408) | no |
| `local_free` | `mi_block_t*` | 32 / 24 | 12 | blocks freed by the owner, migrated to `free` only when `free` is empty | no (see section 11 for whether to keep it) |
| `block_size` | `size_t` | 40 / 32 | 16 | const block size, > 0; 0 marks an unused meta slot (arena.c:979,1291) | no |
| `page_offset` | `size_t` | 48 / 40 | 20 | byte distance from `&page` to the first block (`mi_page_start`, internal.h:825-828); large when meta is out of band | no |
| `capacity` | `uint16_t` | 56 / 48 | 24 | blocks whose free-list links have been initialised (see extend) | no |
| `reserved` | `uint16_t` | 58 / 50 | 26 | total blocks the page memory can hold; `reserved==1` means singleton (internal.h:853-855) | no |
| `slice_pcommitted` | `uint16_t` | 60 / 52 | 28 | committed bytes in OS-page units from the slice start, 0 if fully committed (internal.h:897-906) | no, but commit-on-demand only |
| `retire_expire` | `uint8_t` | 62 / 54 | 30 | countdown for retired (all-free) pages | no |
| `free_is_zero` | `bool` | 63 / 55 | 31 | blocks on `free` are known zero | no |
| `xthread_free` | `_Atomic(uintptr_t)` | 64 / 56 | 32 | blocks freed by other threads, low bit = "owned" (types.h:388-393, internal.h:1101-1131) | yes |
| `theap` | `mi_theap_t*` | 72 / 64 | 36 | owning thread-local heap (kept even when abandoned so reclaim-on-free can find the originating theap, page.c:298-300) | yes |
| `heap` | `mi_heap_t*` | 80 / 72 | 40 | owning first-class heap | yes (multi-heap) |
| `next`, `prev` | `mi_page_t*` | 88,96 / 80,88 | 44,48 | doubly linked page queue per bin | no |
| `memid` | `mi_memid_t` (24 / 20 bytes) | 104 / 96 | 52 | provenance: arena pointer + `slice_index` + `slice_count` (u32 each), or OS base+size; plus `memkind`, `is_pinned`, `initially_committed`, `initially_zero` (types.h:309-336) | partly |
| `keys[]` | `uintptr_t[1 or 2]` | absent | absent | only with encoded free lists or padding | no |

`used`, `capacity`, `reserved` invariants (types.h:408-413, page.c:116-117):
`used - |thread_free| + |free| + |local_free| == capacity <= reserved`. "Full" means
`used == reserved` (internal.h:930-934), not `used == capacity`; "expandable" means
`capacity < reserved` (internal.h:923-927); "mostly used" means at most `reserved/8`
blocks are not in use (internal.h:937-941).

The page struct is 128 bytes on x64 with `self` and 120 without; on wasm32 it is 72
bytes and `mi_page_info_size()` rounds that to 80. A static `mi_page_empty`
(init.c:16-45) with `free == NULL` and `block_size == 0` is what every
`pages_free_direct` slot points at until a real page exists, so the allocation fast
path needs no NULL test (alloc.c:61-75).

### 1.3 `mi_page_queue_t` and the theap (types.h:528-533, 561-597)

A queue is `{ first, last, count, block_size }`. There are `MI_BIN_COUNT = 75` queues:
bins 0..72 for size classes, bin 73 (`MI_BIN_HUGE`) for singleton pages, bin 74
(`MI_BIN_FULL`) for full pages. The huge and full queues are recognised by sentinel
block sizes `MI_LARGE_MAX_OBJ_SIZE + 1 word` and `+ 2 words` (page-queue.c:40-50,
init.c:79-80).

`mi_theap_t` ("thread heap") is the per-thread allocation state:

- `pages_free_direct[MI_PAGES_DIRECT]` (129 pointers, first in the struct for the fast
  path): for each word-size `wsize` in 0..128 a page in bin `_mi_bin(wsize*8)` that
  probably has a free block, or `mi_page_empty` (types.h:563).
- `tld`, `heap`, `subproc`, `refcount` (types.h:565-568): MT/multi-heap plumbing.
- `heartbeat` (deferred-free callback counter), `random` (chacha state for secure
  free-list shuffling and page keys), `page_count`.
- `page_retired_min`, `page_retired_max`: bin index range that may contain retired pages
  (types.h:573-574).
- `pages_full_size`, `generic_count`, `generic_collect_count` (types.h:575-577).
- theap list links `tnext/tprev/hnext/hprev` (types.h:579-582): MT.
- `page_full_retain`, `allow_page_reclaim`, `allow_page_abandon`, `is_detached`
  (types.h:584-587): policy knobs described in section 3.
- `pages[MI_BIN_COUNT]` (types.h:594): the 75 queues (2400 bytes on x64).
- `memid`, `stats` (4368 bytes of statistics, types.h:596).

Total 8096 bytes on x64, 6312 on wasm32; more than half is `mi_stats_t`.

### 1.4 `mi_heap_t`, `mi_tld_t`, `mi_subproc_t` (types.h:617-700)

`mi_heap_t` is the v3 "first-class heap" that can be allocated into from any thread.
It holds: `subproc`, `heap_seq`, heap list links, a dynamic thread-local key `theap`
(each thread finds its theap for this heap through `_mi_thread_local_get`,
prim-tls.h:389-410), `exclusive_arena`, `numa_node`, the list of theaps plus lock,
`abandoned_count[MI_BIN_COUNT]` (atomic counters used to short-circuit abandoned-page
searches, arena.c:739-741), `os_abandoned_pages` list plus lock, and
`arena_pages[MI_MAX_ARENAS=160]` pointers to per-arena `mi_arena_pages_t` (the
bitmaps of pages owned/abandoned by this heap in that arena, types.h:722-726). Every
field except `stats` exists for multi-threading or multi-heap support.

`mi_tld_t` (types.h:690-700): thread id, sequence number, numa node, subproc, list of
theaps, `recurse` flag for the deferred-free callback, `is_in_threadpool`. Entirely
MT/process plumbing.

`mi_subproc_t` (types.h:650-679): the process-global state (arena array and count,
arena reserve lock, purge expiry, main heap, heap list, `theap_meta` used with a lock
to allocate metadata such as tld and theap structs (subproc.c:29-47), thread counts,
stats). Only the arena array is conceptually needed by a port.

### 1.5 Arenas and bitmaps (types.h:729-757, bitmap.h)

`mi_arena_t`: `memid`, `subproc`, `arena_idx`, `start`, `slice_count`, `info_slices`
(slices at the arena start used by its own metadata), numa, exclusivity,
`purge_expire`, optional `commit_fun`, `total_size`/`parent` (for OS ranges larger than
one arena), and five bitmaps over slices: `slices_free` (a binned bitmap), `slices_committed`,
`slices_dirty` (slice has been handed out before, so it is not zero), `slices_purge`
(freed but not yet decommitted), and `pages_main` (bitmaps of pages owned/abandoned
by the main heap), plus `pages_meta` (an array of `slice_count` `mi_page_t` when meta is
separated but not aligned; arena.c:1767-1772).

Bitmaps (bitmap.h:15-59): a `mi_bfield_t` is one machine word; a `mi_bchunk_t` is
`MI_BCHUNK_BITS` bits (512 on x64, 256 on wasm32), cache aligned, and allocations never
span chunks; a `mi_bitmap_t` has a one-chunk "chunkmap" (one bit per chunk that may
have set bits) followed by up to 512 (x64) chunks, hence the 16 GiB / 2 GiB maximum arena.
`mi_bbitmap_t` (bitmap.h:259-270) additionally keeps `chunkmap_bins[5]`, assigning each
chunk a size bin `MI_CBIN_SMALL` (1 slice), `MI_CBIN_MEDIUM` (8), `MI_CBIN_LARGE`
(`MI_BFIELD_BITS` slices), `MI_CBIN_OTHER`, `MI_CBIN_HUGE`, or `MI_CBIN_NONE`
(mimalloc-stats.h:85-93, bitmap.h:249-257). A chunk gets its bin from the first
allocation whose start index is 0 in that chunk (bitmap.c:1857-1860) and returns to
`NONE` when all its bits are free again (bitmap.c:1672-1677). The search
(`mi_bbitmap_try_find_and_clear_generic`, bitmap.c:1801-1884) visits chunks of the
requested bin first and then only `NONE` chunks, so small pages never fragment a chunk
reserved for medium or large pages. Searches start at `tseq % cycle` (thread sequence
number) to spread threads (bitmap.c:1288-1290); single-threaded this is always 0.
Run finding: 1 bit uses `ctz`+atomic and (bitmap.c:594-612); 8 bits finds a byte-aligned
all-ones byte (bitmap.c:713-783), so medium pages are always 8-slice aligned; other
lengths use `clearNX` (within a word, or crossing one word boundary, bitmap.c:793-849,
not aligned) or `clearNC` (across words within a chunk, bitmap.c:855-916). The
"X" (exactly one word = large page) fast path is commented out
(bitmap.h:326,336; bitmap.c:1899-1901), so large pages are not naturally aligned to
their size in v3.5.1.

### 1.6 The page map: how `_mi_ptr_page` works

Three schemes exist, chosen at compile time (internal.h:671-815):

1. Flat map (`MI_PAGE_MAP_FLAT`, internal.h:671-707, page-map.c:16-210). One byte
   per slice: 0 = not a page, 1 = this slice starts a page, `1 < ofs <= 127` = the page
   starts `ofs-1` slices earlier (page-map.c:18-22). Lookup is
   `ofs = map[p >> SLICE_SHIFT]; page = ((p >> SLICE_SHIFT) + 1 - ofs) << SLICE_SHIFT`
   (internal.h:687-692), one dependent load plus shifts. The map covers `1 << vbits`
   bytes; for 32-bit `vbits = 32` and the map is `2^(32-15)` = 128 KiB (page-map.c:60;
   the comment at page-map.c:26 says 64 KiB assuming 64 KiB slices). Because the map is
   at most 1 MiB it is committed eagerly (page-map.c:61) and `map[0] = 1` makes
   `_mi_ptr_page(NULL) == NULL` (page-map.c:92). On x64 with 47 bits a flat map would be
   2 GiB of reserve, so it is only used for `MI_MAX_VABITS <= 40`. Registration writes
   `i+1` into `slice_count` consecutive entries (page-map.c:147-171); for singleton
   pages larger than `MI_LARGE_PAGE_SIZE` only the first 64 slices are registered
   (page-map.c:142), which is why interior-pointer support is bounded. Unregistration
   zeroes the range (page-map.c:173-183).
2. Two-level map (default on 64-bit, internal.h:711-769, page-map.c:214-511). Root
   array of `2^(vbits - 13 - 16)` submap pointers (for 47 bits: 2^18 entries = 2 MiB
   reserved, committed on demand in 64 KiB steps), each submap 8192 `mi_page_t*`
   = 64 KiB covering 512 MiB of address space, allocated with `_mi_os_zalloc` on first
   use under a lock (page-map.c:386-412). Lookup: two dependent loads. The stored value
   is the `mi_page_t*` itself (which may live in the arena's separated meta array), so
   no arithmetic is needed after the load.
3. Aligned meta (`MI_PAGE_META_IS_ALIGNED`, x64 default, internal.h:773-802): arenas
   are aligned to `MI_PAGE_META_ALIGNMENT` = 4096 slices = 256 MiB (types.h:245-248),
   and the first `4096 * sizeof(mi_page_t)` bytes of every 256 MiB stretch are an array
   of page structs, one per slice (arena.c:97-104, 1776-1789). `_mi_aligned_ptr_page0`
   is `page_metas = p & ~(256MiB-1); idx = (p / 64KiB) % 4096; &page_metas[idx]`
   (internal.h:776-788) followed by an acquire load of `->self` to reach the real
   struct for multi-slice pages (internal.h:790-801). No page map access at all on the
   free path; the two-level map is still built for checked frees and `mi_is_in_heap_region`.
   Additionally `MI_PAGE_META_SMALL_IS_ALIGNED` (types.h:159-167) puts the header of
   pages with `block_size <= MI_SMALL_SIZE_MAX` in-band at the slice start so
   `mi_free_small` can compute the page as `p & ~(MI_SMALL_PAGE_SIZE-1)` without even
   the `self` load (free.c:183-191, arena.c:980-988).

`_mi_ptr_page` (internal.h:804-815) dispatches: checked lookup in secure/checked
builds, the aligned lookup when available, otherwise the unchecked map lookup.
`mi_ptr_page_is_valid_ex` (free.c:172-223) is the release-build entry used by `mi_free`;
in the wasm build it reduces to the flat lookup and a NULL test.

## 2. Size classes

### 2.1 `mi_bin` (page-queue.c:60-96)

Input is the byte size including any padding; `wsize = (size + W - 1) / W` with `W =
sizeof(void*)` (internal.h:572-575). The minimum alignment selects a variant
(page-queue.c:24-32): `MI_MAX_ALIGN_SIZE` (16) is more than 2 words on 32-bit so
`MI_ALIGN4W` is defined there; on 64-bit 16 is exactly 2 words so `MI_ALIGN2W` is
defined.

```
if MI_ALIGN4W: if wsize <= 4: return wsize <= 1 ? 1 : (wsize+1) & ~1     // 4-byte words: 4, 8, 16 bytes
if MI_ALIGN2W: if wsize <= 8: return wsize <= 1 ? 1 : (wsize+1) & ~1     // 8-byte words: 8,16,32,48,64
if wsize > MI_LARGE_MAX_OBJ_WSIZE: return MI_BIN_HUGE (73)
if MI_ALIGN4W and wsize <= 16: wsize = (wsize+3) & ~3                   // round to 16 bytes
wsize--
b   = index of highest set bit of wsize
bin = (b << 2) + ((wsize >> (b-2)) & 3) - 3
```

So above the exact small bins there are exactly four bins per power of two, i.e. the
block sizes are `2^k * {1, 1.25, 1.5, 1.75}` and the worst internal fragmentation is
12.5% (page-queue.c:89). The queue block sizes are the static table
`MI_PAGE_QUEUES_EMPTY` in init.c:67-80, in words: 1, 1,2,3,4,5,6,7,8, 10,12,14,16,
20,24,28,32, 40,48,56,64, ..., 458752, 524288, then the huge sentinel and the full
sentinel. Bin 0 is a dummy duplicate of bin 1 (`QNULL(1)` twice) so that bin indices
are 1-based for real sizes.

Bins actually reachable (computed by re-implementing the function):

- 64-bit (W=8): bytes 8, 16, 32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
  384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, ... 458752, 524288.
  Queue bins 3, 5, 7 (24, 40, 56 bytes) exist in the table but are never selected
  because of the double-word rounding.
- wasm32 (W=4): bytes 4, 8, 16, 32, 48, 64, 80, 96, 112, 128, 160, ... 114688, 131072.
  Bins 3, 5, 6, 7, 9, 11 are never selected (12, 20, 24, 28, 40, 56 bytes).
  The largest non-huge bin is 56 (131072 bytes = `MI_LARGE_MAX_OBJ_SIZE`).

Table indices above `MI_MAX_SINGLETON_BIN` (61..72 on x64, 57..72 on wasm32) are never
returned because `mi_bin` tests `wsize > MI_LARGE_MAX_OBJ_WSIZE` first and answers
`MI_BIN_HUGE` (page-queue.c:79-81); their queues stay empty forever.

`_mi_bin_size(bin)` reads the block size back out of `_mi_theap_empty.pages[bin]`
(page-queue.c:108-111) and `mi_good_size(size)` returns it for sizes up to
`MI_LARGE_MAX_OBJ_SIZE`, otherwise rounds to the OS page size (page-queue.c:114-124).

### 2.2 `pages_free_direct` (types.h:557,563; internal.h:651-656; page-queue.c:204-244)

For `size <= MI_SMALL_SIZE_MAX` (1024 / 512 bytes) allocation never calls `mi_bin`:
`idx = wsize_from_size(size)` indexes `pages_free_direct[0..128]`. Whenever the head of
a small-bin queue changes (`mi_page_queue_push`, `_remove`, `_enqueue_from_ex` at
page-queue.c:252-414), `mi_theap_queue_first_update` rewrites the range of direct slots
that map to that bin: it walks back over previous queues with the same bin to find the
first word size the bin covers (page-queue.c:228-237) and sets every slot in
`[start, idx]` to the new head or to `mi_page_empty`. Because there are 129 slots and
only 21 reachable bins under 1 KiB on x64, one queue-head change can rewrite up to 16 slots
(e.g. bin 24, 897..1024 bytes on x64, owns wsizes 113..128).

### 2.3 Page kinds by block size (arena.c:1193-1224)

`_mi_arenas_page_alloc` picks: alignment > `MI_PAGE_MAX_OVERALLOC_ALIGN` -> singleton;
`block_size <= MI_SMALL_MAX_OBJ_SIZE` -> small page (1 slice); `<= MI_MEDIUM_MAX_OBJ_SIZE`
-> medium page (8 slices); `<= MI_LARGE_MAX_OBJ_SIZE` and `MI_ENABLE_LARGE_PAGES` ->
large page (`MI_SIZE_SIZE * 8` slices); else singleton. Note the thresholds are on the
*bin* block size (the queue's block size is passed, page.c:346), and that
`MI_SMALL_MAX_OBJ_SIZE` (10 KiB) is much larger than `MI_SMALL_SIZE_MAX` (1 KiB): the
latter is the direct-table limit, the former is the small-page limit. The max object
sizes are chosen so that a page holds at least 6 blocks after the 4 KiB start alignment
(types.h:469-473).

`MI_MAX_SINGLETON_BIN` (60 on x64, 56 on wasm32) is the highest bin that can live in a
regular (non-singleton) page and sizes the per-bin abandoned-page bitmaps
(`MI_ARENA_BIN_COUNT`, types.h:715; checked by the assertion at arena.c:1196).

## 3. Allocation

### 3.1 Fast path (alloc.c:56-137, 144-174, 264-272)

`mi_malloc(size)` -> `mi_theap_malloc(_mi_theap_default(), size)` (alloc.c:270-272).
`_mi_theap_default()` is a plain initial-exec thread local on Linux and wasi
(prim-tls.h:247-260), initialised to the address of the static read-only
`_mi_theap_empty` so no NULL check is needed (init.c:120-144; the wasi TLS model is
`MI_TLS_MODEL_LOCAL`, prim-tls.h:42-50).

`_mi_theap_malloc_zero_ex` (alloc.c:243-257): if `size <= MI_SMALL_SIZE_MAX` go to
`mi_theap_malloc_small_zero_nonnull` (alloc.c:147-174):

1. `page = theap->pages_free_direct[wsize_from_size(size)]` (internal.h:651-656).
2. `mi_page_malloc_zero(theap, page, size, zero, ppage)` (alloc.c:59-137):
   `block = page->free; used = page->used;` (an `asm("":::"memory")` forces the `used`
   load before the test, alloc.c:70-72); `if (block == NULL) return _mi_malloc_generic(...)`;
   `page->free = block->next; page->used = used + 1;` then, only when `zero`, either
   `memset(block, 0, block_size)` if `!page->free_is_zero` or just `block->next = 0`
   (alloc.c:121-129). The comment says the inlined routine is about 7 instructions with
   a single test (alloc.c:58). Statistics and debug fills are compiled out in release.

Larger sizes go straight to `mi_theap_malloc_generic` -> `_mi_malloc_generic`
(alloc.c:177-201). `mi_zalloc` is the same path with `zero = true` (alloc.c:293-303);
`mi_calloc` adds an overflow-checked multiply (alloc.c:305-313).

### 3.2 `_mi_malloc_generic` (page.c:1085-1117)

The `zero` flag and a huge-alignment request are packed into one parameter
(`zero_huge_alignment & 1` is zero, the rest is the alignment) to keep the fast path
at four arguments (page.c:1089-1097).

Fast-ish path (page.c:1101-1114): if the theap is initialised, `++theap->generic_count
< 1000`, no huge alignment, and `req_size < MI_SMALL_MAX_OBJ_SIZE`, then
`pq = &theap->pages[_mi_bin(size)]` (internal.h:957-961) and
`mi_page_queue_find_free(theap, pq)` (page.c:903-918), then `_mi_page_malloc_zero`.
Note this does not do "collect", does not handle full pages for medium blocks and does
not do the deferred-free callback.

Everything else goes to `mi_malloc_generic_fallback` (page.c:1048-1082):

1. `mi_malloc_generic_admin` (page.c:1011-1042): initialise the thread if needed;
   every 1000 generic calls reset `generic_count`, and either every `generic_collect`
   (10000) generic calls run a full `mi_theap_collect(theap, false)` or otherwise run
   `_mi_deferred_free` (user callback + heartbeat, page.c:990-999) and
   `_mi_theap_collect_retired(theap, false)`.
2. `mi_find_page` (page.c:950-975): reject `> MI_MAX_ALLOC_SIZE` (`PTRDIFF_MAX`,
   types.h:240); choose the queue (the huge queue when a huge alignment is requested);
   huge queue -> `mi_huge_page_alloc`; else `mi_page_queue_find_free`.
3. On failure: `mi_theap_collect(theap, true)` (force purge etc.) and retry once
   (page.c:1056-1059); then ENOMEM.
4. `_mi_page_malloc_zero` on the found page (page.c:1074), and if the page is a medium
   or large page (`block_size > MI_SMALL_MAX_OBJ_SIZE`) and is now full, move it out of
   the queue immediately with `mi_page_to_full` (page.c:1078-1080). Small pages are
   moved out lazily during the next search instead.

### 3.3 Finding a page: `mi_page_queue_find_free` (page.c:878-918)

First the head page is tried cheaply (`mi_page_queue_lookup_free_first`,
page.c:879-901): `mi_page_free_quick_collect(page)` (page.c:204-212) returns true if
`page->free != NULL`, or if `local_free != NULL` in which case it does
`free = local_free; local_free = NULL; free_is_zero = false`. On success
`retire_expire = 0` and the page is returned. Only if the head has nothing does the
full search run:

`mi_page_queue_find_free_ex(theap, pq, first_try)` (page.c:765-876), "next fit" over
the queue:

- For each page: `count++`, `candidate_limit--`. If no immediate free block, run
  `_mi_page_free_collect(page, false)` (page.c:214-243), which swaps in `xthread_free`
  (atomic exchange, page.c:186-201, then walks the list to count it and fix `used`,
  page.c:150-183) and moves `local_free` to `free`.
- If still nothing available and the page is not expandable, it is full:
  `page_full_retain--` (starts at `theap->page_full_retain` = 2 for small pages, 0 for
  bigger, page.c:771). Once negative, `mi_page_to_full(page, pq)` (page.c:374-389):
  with `allow_page_abandon` (the default, since `page_full_retain >= 0`, theap.c:230)
  the page is *abandoned* via `_mi_page_abandon` (page.c:291-304): removed from the
  queue, `xthread_id` set to 0 (but `page->theap` kept so a later free can reclaim into
  the same theap), and `_mi_arenas_page_abandon` (arena.c:1314-1365) which, for a full
  page, does nothing but release ownership: the page is now referenced only by the page
  map and will come back when one of its blocks is freed (section 4.3). Without
  abandoning (theaps that can be destroyed, `theap_meta`, or `page_full_retain = -1`)
  it goes to `pages[MI_BIN_FULL]` with the `MI_PAGE_IN_FULL_QUEUE` flag
  (page-queue.c:287, 413).
- Otherwise the page is a candidate. First candidate sets
  `candidate_limit = page_max_candidates` (4). A later page replaces the candidate if
  the current candidate is completely free (which is then freed right there,
  page.c:807-810), or if it has `used >= candidate->used` and is not "mostly used"
  (page.c:811-814): prefer fuller pages so emptier ones can drain and be freed, but do
  not pick pages within 1/8 of full. Stop when a candidate has an immediately available
  block or after 4 more pages (page.c:816-819).
- After the loop: if the candidate needs extension, `mi_page_extend_free`. If there is
  no page, `_mi_theap_collect_retired(theap, false)` then `mi_page_fresh(theap, pq)`
  (page.c:344-351) which allocates from the arenas; if that returns NULL on the first
  try the whole search is retried once because a reclaimed abandoned page may now be in
  the queue (page.c:859-863). The chosen page is moved to the front of the queue and
  `retire_expire = 0` (page.c:868-869).

`mi_page_fresh_alloc` (page.c:308-341): `_mi_arenas_page_alloc(theap, block_size,
page_alignment)`; if the arena returned an abandoned page it is reclaimed with
`_mi_theap_page_reclaim` (page.c:277-289; collect, then `mi_page_queue_push_at_end`)
and extended if it has no immediately free block; a fresh page is pushed at the front.

### 3.4 Page initialisation and lazy extension (page.c:614-758)

`_mi_page_init` (page.c:709-758) only sets the keys (secure builds) and calls
`mi_page_extend_free` once, so a fresh page starts with a short free list.

`mi_page_extend_free` (page.c:630-706):

- `extend = reserved - capacity`, capped by `max_extend = bsize >= MI_MAX_EXTEND_SIZE ?
  MI_MIN_EXTEND : MI_MAX_EXTEND_SIZE / bsize` with `MI_MAX_EXTEND_SIZE = 8 KiB` and
  `MI_MIN_EXTEND = 1` (`8 * MI_SECURE` in secure builds) (page.c:618-623, 651-659). So
  at most 8 KiB worth of blocks are threaded per extension: 1024 8-byte blocks, 8 1 KiB
  blocks, 1 block for anything >= 8 KiB. The comment: going from 1 to 8 increased the
  `lean` benchmark's rss by 50% (page.c:656-658).
- Commit-on-demand (page.c:664-690): if `slice_pcommitted != 0` the extension is
  capped to one slice and the needed range is committed in `mi_page_min_commit_size()`
  (16 KiB or OS page) steps. Off by default (`page_commit_on_demand = 0`) and always off
  for small pages (arena.c:1150-1152).
- `mi_page_free_list_extend` (page.c:589-612): writes `block[i].next = &block[i+1]` for
  `i` in `[capacity, capacity+extend)` sequentially, links the last to the old `free`
  (usually NULL) and sets `free` to the first. `capacity += extend`. The secure variant
  (`MI_SECURE >= 2`, page.c:533-587) splits the range into up to 64 slices and threads
  through them in random order using the theap's chacha stream; irrelevant otherwise.
- The comment at page.c:627-629 records that bump-pointer allocation for fresh blocks
  was tried and did not speed up any benchmark.

### 3.5 Huge (singleton) allocation (page.c:920-946)

`block_size = _mi_os_good_alloc_size(size)` (os.c:97-106: round up to 4 KiB below
512 KiB, 64 KiB below 2 MiB, 256 KiB below 8 MiB, 1 MiB below 32 MiB, else 4 MiB),
then `mi_page_fresh_alloc(theap, huge_queue, block_size, page_alignment)`. The page has
`reserved == 1`, sits in `pages[MI_BIN_HUGE]`, and is freed as soon as its block is
freed (section 4.2).

## 4. Free

### 4.1 `mi_free` fast path (free.c:234-269)

```
mi_free(p):
  page = page-map lookup (mi_ptr_page_is_valid_ex, free.c:172-223); return if NULL
  xtid = thread_id ^ page->xthread_id                          (free.c:241-242)
  xtid == 0            -> local, no flags: mi_free_block_local(page, p, check_full=false)
  xtid <= 3            -> local but in full queue or has interior pointers: mi_free_generic_local
  (xtid & 3) == 0      -> another thread (or abandoned) and no flags: mi_free_block_mt
  else                 -> mi_free_generic_mt
```

The single XOR classifies four cases because the thread id has its low two bits clear
(prim-tls.h:185-190) and the page flags live in those bits (types.h:371-378). On wasi
the "thread id" is the address of a thread-local dummy variable (prim-tls.h:177-183,
`MI_NO_THREAD_POINTER`).

`mi_free_block_local` (free.c:28-57): in release builds the padding and double-free
checks are no-ops (free.c:613-617, 744-747); then

```
used = page->used - 1
block->next = page->local_free
page->used = used
page->local_free = block
if used == 0 and page->retire_expire == 0: _mi_page_retire(page)
else if check_full and page is in the full queue: _mi_page_unfull(page)
```

Blocks go to `local_free`, never directly to `free`; they become allocatable only when
`free` runs dry and the generic path (or the queue search) swaps the lists
(page.c:204-212, 221-227). `mi_free_generic_local` (free.c:148-153) first unaligns
interior pointers with `_mi_page_ptr_unalign` (free.c:104-114:
`adjust = (p - page_start) & (block_size-1)` for power-of-two sizes, else `%`) if the
page has `MI_PAGE_HAS_INTERIOR_POINTERS`, and calls `mi_free_block_local` with
`check_full = true`.

`mi_free_size(p, size)` (free.c:308-343) ignores the size in release builds unless the
aligned-meta scheme is on, in which case `size <= MI_SMALL_SIZE_MAX` routes to
`mi_free_small` (mask instead of map lookup). `mi_free_small(_nonnull)` (free.c:281-294)
is the runtime-oriented entry that masks the pointer with `MI_SMALL_PAGE_SIZE` when
`MI_PAGE_META_SMALL_IS_ALIGNED` (free.c:183-185).

### 4.2 Retire (page.c:414-518)

When a free makes `used == 0` the page is not released immediately: `_mi_page_retire`
(page.c:424-457) keeps it if the queue holds at most `MI_RETIRE_MAX_PAGES = 3` pages, is
not the huge or full queue, and either it is the only page in the queue or the block
size is below `MI_SMALL_SIZE_MAX` (1 KiB). It then sets `retire_expire =
MI_RETIRE_CYCLES (16)` for `block_size <= MI_SMALL_MAX_OBJ_SIZE` and `16/4 = 4` for
larger blocks (page.c:445) and widens `theap->page_retired_min/max` to include the bin
(page.c:449-450). Otherwise `_mi_page_free`. Retired pages stay in their queue (at
whatever position) and are found again by allocation, which clears `retire_expire`
(page.c:869, 892). The release note for v3.5.0 mentions raising the retired count from
1 to 3 (readme.md, 2026-08-18 entry).

`_mi_theap_collect_retired(theap, force)` (page.c:501-518) runs every 1000 generic
allocations (page.c:1038), from every collect (theap.c:135), and before allocating a
fresh page (page.c:856). It scans bins `[page_retired_min, page_retired_max]`, and in
each looks only at the first up to 3 pages with `retire_expire != 0` (retired pages
are always near the head because allocation moves used pages to the front,
page.c:499-500). `mi_page_try_retire` (page.c:481-497): if still all free, decrement;
at 0 (or when forced) `_mi_page_free`; if not all free, `retire_expire = 0`.

`_mi_page_free(page, pq)` (page.c:393-412): clear the interior-pointer flag, unlink
from the queue (`mi_page_queue_remove`, which may rewrite `pages_free_direct`),
`mi_page_set_theap(page, NULL)`, and `_mi_arenas_page_free`.

### 4.3 Full and unfull transitions

With the default `allow_page_abandon`, a full page leaves the theap entirely
(section 3.3). A later `mi_free` into it sees `xthread_id == 0` and takes the
multi-threaded path: `mi_free_block_mt` (free.c:63-97) pushes the block on
`xthread_free` with a CAS while setting the owned bit, and because the page was not
owned, calls `mi_free_try_collect_mt` (free.c:486-521): collect (the `_partly` variant
avoids a second atomic for small blocks, page.c:251-269), then in order: free the page
if all blocks are free (`mi_abandoned_page_try_free`, free.c:378-385); reclaim it into
the current theap if `page_reclaim_on_free >= 0` and the block size is at most medium
(`mi_abandoned_page_try_reclaim`, free.c:434-482: with the default option 0 only if
`page->theap` is this thread's theap, which in a single-threaded program it always is,
and subject to `page_max_reclaim`, default unlimited) which pushes it at the end of its
bin queue (page.c:277-289); otherwise re-abandon it as "mapped" (findable by allocation
through the arena's abandoned bitmap) if it fell below 7/8 used (free.c:388-397,
arena.c:1369-1391); otherwise release ownership again (free.c:402-424).

So in a single-threaded run the lifecycle of a full small page is: page fills -> stays in
queue until a search visits it and `page_full_retain` is exhausted -> abandoned (owner
id 0) -> first free into it takes the CAS path and pulls it back to the end of the queue.
Medium and large pages are abandoned immediately when they fill (page.c:1078-1080).

The non-abandoning alternative is `pages[MI_BIN_FULL]` plus `_mi_page_unfull`
(page.c:359-372): a free into a page with `MI_PAGE_IN_FULL_QUEUE` moves it back to its
bin queue at the end (`mi_page_queue_enqueue_from_full`, page-queue.c:420-423; the
comment notes inserting at the front slowed `alloc-test`). `mi_theap_collect_full_pages`
(page.c:460-479) is only run from `_mi_theap_collect_retired` when abandoning is off.

### 4.4 `used` and `free`: what a free does not do

A free never touches `capacity`, never writes the page map, never checks the block
against the page bounds (release), and never coalesces. The only extra work is the
`used == 0` test and, for abandoned or full pages, the queue move. `mi_usable_size`
(free.c:540-556) is `page->block_size` (minus the interior offset for aligned blocks).

## 5. Page lifecycle in arenas

### 5.1 Getting slices (arena.c:240-335, 497-570, 781-872)

`mi_arenas_page_regular_alloc` (arena.c:1140-1163): (1) try to find an abandoned page
of this bin in the heap's per-arena `pages_abandoned[bin]` bitmaps
(`mi_arenas_page_try_find_abandoned`, arena.c:725-779; skipped when
`heap->abandoned_count[bin] == 0`), claiming ownership with an atomic or on
`xthread_free`; (2) decide commit (always for pages of one min-commit unit or when
`page_commit_on_demand == 0`, arena.c:1149-1152); (3) `mi_arenas_page_alloc_fresh`
(arena.c:951-1137) then `_mi_page_init`.

`mi_arenas_page_alloc_fresh_area` (arena.c:781-872): if arena allocation is allowed,
not an OS-alignment request, and `slice_count <= arena_max_object_size / slice`, call
`mi_arenas_try_alloc` (arena.c:525-570): iterate the subproc's arenas starting at a
heap/thread dependent index (arena.c:427-482), in each `mi_arena_try_alloc_at`
(arena.c:240-335) = `mi_bbitmap_try_find_and_clearN(slices_free, tseq, slice_count)`,
then set `slices_dirty` bits (the return value "all were clear" together with the
arena being `initially_zero` gives `memid.initially_zero`, arena.c:255-260), and
commit/mark `slices_committed` as needed. If no arena has room, reserve a new one under
`arena_reserve_lock` (arena.c:551-563) and retry. If arenas are disallowed or the
object is too big, fall back to the OS (`mi_arena_os_alloc_aligned`, arena.c:573-591)
with the page aligned to `MI_ARENA_SLICE_ALIGN`.

Slice counts per page kind (arena.c:1201-1213): small 1, medium 8, large
`MI_SIZE_SIZE * 8` (64 on x64, 32 on wasm32), singleton `ceil((info_size + block_size) /
slice)` where `info_size` is `mi_page_info_size()` normally and `MI_PAGE_ALIGN` for
OS-aligned singletons (arena.c:1166-1178), 0 with aligned meta.

### 5.2 Arena reservation and growth (arena.c:341-407, 1896-1917)

`mi_arena_reserve`: start from the `arena_reserve` option (1 GiB on 64-bit, 128 MiB on
32-bit), divide by 4 when the OS has no virtual reserve (wasm: 32 MiB), round to slices,
and for the n-th arena multiply by `2^clamp(n/8, 0, 16)` (so 8 arenas of each size,
then double), always at least `req_size` plus one chunk (32 MiB, 8 MiB on wasm32), rounded to a chunk (over-reserve for
metadata, arena.c:365), clamped to `[MI_ARENA_MIN_SIZE, MI_ARENA_MAX_SIZE]`. Eager
commit only when the OS overcommits (option 2, arena.c:384-387). On failure retry with
`4 * MI_ARENA_MIN_SIZE`. `MI_MAX_ARENAS` is 160 (types.h:611), stopping at 156.

`mi_reserve_os_memory_ex2` (arena.c:1896-1917) does `_mi_os_alloc_aligned(size,
MI_ARENA_ALIGNMENT, ...)` and `mi_manage_os_memory_ex2` (arena.c:1804-1871) which splits
ranges bigger than 16 GiB into sub-arenas and calls `mi_arena_initialize`
(arena.c:1686-1802): compute `info_slices` = `align_up(sizeof(mi_arena_t)) + (4 +
MI_ARENA_BIN_COUNT) bitmaps + 1 binned bitmap (+ slice_count * sizeof(mi_page_t) when
meta is separated but not aligned)` rounded up to slices (arena.c:1638-1657); commit and
zero that prefix; carve the bitmaps; mark `[info_slices, slice_count)` free
(arena.c:1791) and, with aligned meta, additionally reserve the first
`4096*sizeof(mi_page_t)/64KiB = 8` slices of every 256 MiB stretch (arena.c:1776-1789).
For a 32 MiB wasm arena of 1024 slices: arena struct 328 B, 60 plain bitmaps of 1024
bits (`mi_bitmap_size` = 64 B header + 4 chunks of 32 B = 192 B each, bitmap.c:1049-1060)
and one binned bitmap (`mi_bbitmap_size` = 224 B header + 128 B = 352 B,
bitmap.c:1583-1594), about 12 KiB in total, so the info fits in one 32 KiB slice (and
nothing is needed for page meta since it is in-band).

### 5.3 Page header placement and block start (arena.c:875-905, 951-1137)

In the separated schemes the page struct comes from `pages_meta[slice_index]` or the
aligned meta array (`mi_arena_page_meta`, arena.c:911-948) and `block_start` is 0,
except that power-of-two block sizes between one word and
`MI_PAGE_BLOCK_START_MAX_OFFSET = 8 * MI_INTPTR_BITS = 512` bytes get
`block_start = align_up(mi_page_info_size(), block_size)` plus `3*block_size` for sizes
under 64 (arena.c:994-1002), which preserves natural alignment while shifting the
first blocks of different pages by different amounts (cache-set spreading).

In the in-band scheme (wasm, and `MI_PAGE_META_SMALL_IS_ALIGNED` small pages) the page
struct is at the slice start and `mi_page_block_start(block_size, os_align)`
(arena.c:875-905) gives the offset of the first block:

- OS-aligned singleton: `MI_PAGE_ALIGN` (one whole slice).
- power-of-two block <= 4 KiB: `align_up(info_size, block_size)` (+ `3*block_size` if
  block < 64), so blocks are naturally aligned to their size (page starts are slice
  aligned).
- block a multiple of 4 KiB: `align_up(info_size, 4 KiB)` to keep blocks OS-page
  aligned (`MI_PAGE_OSPAGE_BLOCK_ALIGN2`, types.h:466).
- else `info_size` (80 on wasm32, 128 on x64), rounded to 16.

`reserved = (page_size - block_start) / block_size` (arena.c:1064), `page_offset =
start - page` (arena.c:1070), `free_is_zero = memid.initially_zero` (arena.c:1076),
`slice_pcommitted` (arena.c:1079), owner set (arena.c:1081-1082), then
`_mi_page_map_register` (arena.c:1121). A wasm32 small page of 8-byte blocks thus
starts its blocks at offset 112 and holds 4082 blocks; a 4096-byte-block page starts at
4096 and holds 7.

Natural alignment guarantee summarised (used by `mi_malloc_is_naturally_aligned`,
alloc-aligned.c:18-28): blocks whose bin size is a power of two up to 4 KiB are aligned
to their size; blocks whose bin size is a multiple of 4 KiB are 4 KiB aligned; nothing
else is promised.

### 5.4 Freeing pages back and purging (arena.c:1226-1308, 1443-1500, 2248-2445)

`_mi_arenas_page_free` -> `mi_arenas_page_free_prim`: unregister from the page map,
clear the heap's `pages` bit, fix commit accounting, set `block_size = 0` for separated
meta, and `_mi_arenas_free(subproc, slice_start, full_size, memid)`
(arena.c:1443-1500): for arena memory schedule a purge and set the `slices_free` bits
(`mi_bbitmap_setN`, which also returns the chunk to bin NONE if it becomes all free).
OS memory is returned with `_mi_os_free`.

Purging (arena.c:2252-2445): `mi_arena_schedule_purge` marks the slices in
`slices_purge` and sets the arena's and subproc's `purge_expire = now + purge_delay *
arena_purge_mult` (4000 ms by default); with delay 0 it decommits immediately.
`mi_arenas_try_purge` runs from `_mi_arenas_collect` (called by `mi_theap_collect`,
theap.c:142-144, i.e. every 10000 generic allocations or on forced collects), visits
`max_arena/4 + 1` arenas, and for expired arenas walks `slices_purge` in ranges of at
least `_mi_os_minimal_purge_size()` (OS page, or 2 MiB with THP), re-claims the still
free slices from `slices_free` with `mi_bbitmap_try_clearNC`, decommits or resets them
(`_mi_os_purge_ex`, os.c:666-689) and clears `slices_committed`. Nothing is ever
unmapped; arenas persist for the process lifetime (only `destroy_on_exit` frees them,
arena.c:1552-1576).

### 5.5 What the wasi prim does (src/prim/wasi/prim.c, selected by prim.c:17-19)

- `_mi_prim_mem_init` (wasi/prim.c:22-28): `page_size = 64 KiB`, `alloc_granularity =
  16`, `has_overcommit = false`, `has_partial_free = false`, `has_virtual_reserve =
  false`.
- Memory comes from `sbrk` because prim.c:18 defines `MI_USE_SBRK` before including
  the file; the `__builtin_wasm_memory_grow` branch (wasi/prim.c:54-61) is dead code in
  this configuration. With `MI_USE_SBRK` on `__wasi__` the fresh memory is not
  memset (wasi/prim.c:49-51) but `_mi_prim_alloc` still reports `*is_zero = false`
  (wasi/prim.c:121-127), so mimalloc never learns that grown memory is zero.
- Alignment (wasi/prim.c:67-118): for `try_alignment > 1` it reads the current break
  (`sbrk(0)`), computes `alloc_size = align_up((aligned_current - current) + size,
  64 KiB)` and grows by that; the gap up to the aligned start is wasted forever (there is
  no free: `_mi_prim_free` is a no-op, wasi/prim.c:34-38). If another thread moved the
  break in between it gives up and the caller over-allocates (os.c:384-434), wasting up
  to `alignment` bytes.
- `_mi_prim_commit/decommit/reset/reuse/protect` are no-ops; decommit reports
  `needs_recommit = false` (wasi/prim.c:134-159). So purging costs CPU in the bitmaps and
  frees nothing.
- No huge pages, one NUMA node, `clock()`/`clock_gettime` clock, `getenv` for options,
  no strong randomness (`_mi_prim_random_buf` returns false, so chacha is seeded from
  the clock and an address, random.c:175-198), no thread hooks.
- `os.c:364-369`: on 32-bit a direct aligned allocation is only attempted when the
  alignment is at most `alloc_granularity` or at most `size/4`; otherwise it goes
  straight to over-allocation (`size + alignment`, os.c:392-434), and because
  `has_partial_free` is false the whole over-allocation stays mapped.

Concrete wasm waste from this: the 128 KiB flat page map is requested 64 KiB aligned
(`_mi_os_alloc_aligned(reserve_size, 1, ...)` rounds the alignment to the OS page,
os.c:468) which is more than `size/4`, so it costs a 192 KiB sbrk with 64 KiB unused;
each 32 MiB arena reservation rounds the break up to 64 KiB first.

The emscripten prim (src/prim/emscripten/prim.c) is different: it layers mimalloc on
`emmalloc_memalign`/`emmalloc_free` so holes can be reused, and it also reports
`is_zero = false` (emscripten/prim.c:76-94).

## 6. Aligned allocation (alloc-aligned.c)

`mi_theap_malloc_zero_aligned_at(theap, size, alignment, offset, zero, ppage)`
(alloc-aligned.c:197-241):

1. Reject non-power-of-two alignments (alloc-aligned.c:200-202).
2. Fast path (alloc-aligned.c:220-236): if `size <= MI_SMALL_SIZE_MAX` and `alignment
   <= size`, look at the direct page for the size; if its head free block satisfies
   `((free + offset) & (alignment-1)) == 0` allocate it with `_mi_page_malloc_zero`.
   This exploits the natural alignment of power-of-two bins.
3. Generic (alloc-aligned.c:160-188): if `offset == 0` and
   `mi_malloc_is_naturally_aligned(size, alignment)` (alloc-aligned.c:18-28: `alignment
   <= size` and the bin size is a power of two <= 4 KiB, or `alignment == 4 KiB` and the
   bin size is a 4 KiB multiple), do a normal allocation.
4. Over-allocation (alloc-aligned.c:68-157): if `alignment > MI_PAGE_MAX_OVERALLOC_ALIGN`
   (one slice) allocate a singleton page through the generic path with
   `huge_alignment = alignment` (the size is forced above `MI_SMALL_SIZE_MAX` to reach
   `_mi_malloc_generic`, alloc-aligned.c:86-88); `_mi_arenas_page_alloc` then uses
   `mi_arenas_page_singleton_alloc` with `os_align = true`, allocating from the OS with
   an align offset of one slice so that `slice_start + MI_PAGE_ALIGN` is aligned
   (arena.c:858-862, os.c:511-537). Otherwise allocate `oversize = max(size, 16) +
   alignment - 1` from the normal bins (alloc-aligned.c:95-96), then `aligned_p = p +
   adjust`. If `adjust != 0` set the page flag `MI_PAGE_HAS_INTERIOR_POINTERS`
   (alloc-aligned.c:113-114; atomic or on `xthread_id`, internal.h:1016-1018), which
   makes every future free in that page take the generic path and call
   `_mi_page_ptr_unalign` (free.c:150, 104-114). The flag is cleared only when the page
   is freed or retired (page.c:401, 430).

`mi_usable_size` of an interior pointer is `block_size - adjust` (free.c:529-538). There
is no `MI_BLOCK_ALIGNMENT_MAX` in v3 (that was the v2 name, v2 types.h:221); the
equivalent threshold is `MI_PAGE_MAX_OVERALLOC_ALIGN`.

Aligned realloc (alloc-aligned.c:347-376): reuse in place if `newsize <= size`, `newsize
>= size/2`, and `(p + offset)` is still aligned; else allocate aligned, copy, zero the
tail if requested (from `copy_size - word` to the new usable size), free.

## 7. Zeroing

Three sources of "known zero":

1. `memid.initially_zero` for a fresh arena range: true only if the arena's own memory was
   zero from the OS (`arena->memid.initially_zero`) and every slice's `slices_dirty` bit
   was clear (arena.c:254-264), or if a commit reported zero memory (arena.c:279-281).
   OS allocations set it from the prim's `is_zero` (os.c:438). The wasi prim always says
   false, so on wasm no page is ever known zero.
2. `page->free_is_zero` is initialised from `memid.initially_zero` (arena.c:1076) and
   cleared the first time `local_free` (recycled blocks) is migrated into `free`
   (page.c:210, 226, 238, 259). It is never set back to true. So only the never-used tail
   of a fresh page benefits.
3. `mi_page_malloc_zero` (alloc.c:120-129): with `zero` and `free_is_zero` it writes only
   `block->next = 0` (the one word the free list dirtied); otherwise `memset(block, 0,
   block_size)` of the full block size (not the requested size, issue #63).

`_mi_theap_realloc_zero` never allocates zeroed; it zeroes only from
`align_down(copy_size - word)` to the new usable size (alloc.c:431-442). Metadata is
allocated with `_mi_meta_zalloc` (subproc.c:29-37) through the meta theap.

## 8. realloc (alloc.c:378-547)

`mi_theap_realloc_zero_ex` (alloc.c:393-453):

- `p == NULL` behaves as malloc (`mi_theap_realloc` short-circuits, alloc.c:459-467).
- `size = _mi_page_usable_size(page, p)` = block size (minus interior offset).
- In place if `newsize <= size && newsize >= size/2 && newsize > 0` and the page belongs
  to the caller's heap (alloc.c:415-430): no data movement, no bookkeeping change at
  all. A shrink to below half moves the block to a smaller bin.
- Otherwise `_mi_theap_malloc_zero(newsize, zero=false)`, copy `min(newsize, size)`
  bytes with `_mi_memcpy_aligned`, zero the tail if requested, `mi_free(p)`
  (alloc.c:432-451). A `newsize == 0` request writes one zero byte (issue #725,
  alloc.c:443-445).
- `mi_expand` (alloc.c:379-391) only succeeds for shrinks or same-bin sizes.

`MI_PADDING` (debug canaries after each block, types.h:96-105, 544-555) and
`_mi_padding_shrink` are compiled out in release (`MI_PADDING 0` is hard-set at
types.h:68 before the conditional at 98, so padding is effectively never on in v3.5.1
unless the build system defines it).

## 9. Multi-threading and process-level machinery that a single-threaded port deletes

Everything in this list exists only because several threads (or several heaps or
sub-processes) share the allocator:

- `page->xthread_free` and the owned bit; `mi_free_block_mt`, `mi_free_generic_mt`,
  `mi_free_try_collect_mt`, `mi_abandoned_page_*` (free.c:59-97, 155-167, 370-521);
  `mi_page_thread_free_collect`, `mi_page_thread_collect_to_local`,
  `_mi_page_free_collect_partly` (page.c:150-201, 245-269); the `used` semantics that
  include not-yet-collected thread frees (types.h:408).
- `page->xthread_id` as a thread id and the XOR dispatch in `mi_free_nonnull`
  (free.c:236-262); `mi_page_set_theap`'s CAS loop (internal.h:1020-1032).
- Abandonment: `_mi_page_abandon`, `_mi_arenas_page_abandon/unabandon/
  try_reabandon_to_mapped` (page.c:291-304, arena.c:1314-1434), the
  `MI_THREADID_ABANDONED(_MAPPED)` states, `mi_arenas_page_try_find_abandoned`
  (arena.c:725-779), `heap->abandoned_count[]`, `heap->os_abandoned_pages`, the
  per-heap per-arena `mi_arena_pages_t` bitmaps (`pages`, `pages_abandoned[61]`,
  types.h:722-726; `mi_arena_pages_alloc` arena.c:1671-1684), `mi_bitmap_try_find_and_claim`
  and `mi_bitmap_clear_once_set` (bitmap.c:1340-1380, 1426-1435), the reclaim options
  (`page_reclaim_on_free`, `page_max_reclaim`, `page_cross_thread_max_reclaim`) and
  `page_full_retain`/`allow_page_abandon`. In a single thread "abandon" is only an
  expensive way to implement the full queue.
- Deferred/delayed free hooks: `_mi_deferred_free`, `mi_register_deferred_free`,
  `heartbeat`, `tld->recurse` (page.c:979-1004).
- `mi_theap_t` vs `mi_heap_t` split, `theap->tld/heap/subproc/refcount/tnext/hnext`,
  `_mi_theap_default`/`_mi_theap_cached` thread locals and the four TLS models
  (prim-tls.h), `mi_tld_t`, `mi_subproc_t`, subproc.c, threadlocal.c (dynamic TLS keys
  for first-class heaps), theap.c's create/detach/refcount code (theap.c:308-450),
  `_mi_thread_init/_done`, `mi_process_setup_auto_thread_done`, `theap_meta` and its
  lock (subproc.c:29-88), `_mi_meta_zalloc`.
- All `_Atomic` fields and `mi_atomic_*` ops in bitmaps (bitmap.c), arenas (arena.c),
  the page map (`committed_count`, submap CAS, page-map.c:386-412), `mi_lock_t` and the
  spin-lock fallback (atomic.h:506-545), `mi_atomic_do_once`, `arena_reserve_lock`,
  `purge_guard`, `tseq`-based search starts (bitmap.c:1288-1290, arena.c:427-452),
  `chunk_max_accessed` tracking.
- NUMA (`numa_node` fields, `_mi_os_numa_node*`, os.c:865-907), huge OS pages
  (os.c:726-862), `commit_fun`, `is_pinned`, `exclusive_arena`, sub-arenas/`parent`.
- Statistics (`mi_stats_t`, stats.c, all `mi_*_stat_*` macros; 4 KiB per theap and per
  heap), options parsing from the environment (options.c), verbose/error output.
- Secure/debug/guarded: free-list encoding and `keys`, padding canaries, guard pages
  (`MI_GUARDED`, alloc.c:877-964, free.c:794-818), randomised free lists
  (page.c:533-587), `MI_SECURE >= 5` arena guard pages, double-free checks
  (free.c:559-618), `mi_check_padding_on_free`, debug fills.
- Commit management: `slices_committed`, `slices_purge`, `purge_expire`,
  `slice_pcommitted`, `_mi_os_commit/decommit/reset/purge`, `mi_arena_purge*`
  (arena.c:2248-2445), `mi_option_page_commit_on_demand`. On wasm all are no-ops anyway.
- Heap visiting/destroy (`_mi_heap_visit_blocks`, `mi_heap_delete_page`,
  arena.c:2448-2653; theap.c:536-723), `mi_heap_new/delete/destroy`, arena unload/reload
  (commented out).
- The C++ `new`/`new_handler` glue, `strdup`/`realpath`, `alloc-override.c`,
  `alloc-posix.c`.

## 10. Existing wasm / wasi / 32-bit special-casing

- internal.h:113-115: `__EMSCRIPTEN__` defines `__wasi__` so all wasi paths apply to
  emscripten too (except the prim).
- prim/prim.c:17-19: `__wasi__` selects `wasi/prim.c` with `MI_USE_SBRK`.
- CMakeLists.txt:476-479: WASI turns on `MI_FREE_USE_PAGEMAP`, disabling the aligned
  meta scheme and (via bits.h:140-146 with `MI_MAX_VABITS = 32`) selecting the flat page
  map and in-band page headers (types.h:135-141).
- bits.h:126-135: 32-bit targets get `MI_MAX_VABITS = MI_MIN_VABITS = 32`, so the page map
  covers the whole address space and `_mi_checked_ptr_page` skips the max-address test
  (internal.h:695-699).
- types.h:195, 202-212, 229: slice 32 KiB, chunk 256 bits, large page 1 MiB on 32-bit;
  types.h:485-493: `MI_MAX_SINGLETON_BIN = 56`.
- page-queue.c:24-32: `MI_ALIGN4W` on 32-bit (16-byte minimum alignment with 4-byte
  words), which is what makes the wasm32 bin table skip 12/20/24/28/40/56 bytes.
- options.c:52-56: `arena_reserve` 128 MiB on 32-bit; arena.c:350-352: divided by 4
  without virtual reserve; options.c:59-61: `arena_max_object_size` 256 MiB on 32-bit.
- os.c:16-22: assumed physical memory 4 GiB on 32-bit; os.c:364-369: 32-bit direct
  aligned allocation heuristic; os.c:733-777: no huge-page address space claiming on
  32-bit.
- internal.h:1397-1404: 32-bit `_mi_random_shuffle` constants (Wellons' hash).
- atomic.h:18 and 506-545: without pthreads (wasi) `mi_lock_t` is a CAS spin lock that
  gives up after 10000 yields.
- random.c:180-182: no "unable to use secure randomness" warning on wasi.
- alloc.c:593: no `realpath` on wasi; alloc-override.c:377-383: `__libc_malloc` etc.
  forwards for wasi-libc.
- threadlocal.c:74-80: 12 index bits / 20 version bits for dynamic TLS keys on 32-bit.
- init.c:57-63: the `pages_free_direct` static initialiser has 129 entries without
  padding on both widths.
- Default option values relevant on wasm: `arena_eager_commit` 2 (no overcommit reported,
  so arenas are reserved uncommitted, but commit is a no-op), `purge_delay` 1000 and
  `arena_purge_mult` 4 (purges are scheduled and run, but decommit is a no-op),
  `page_commit_on_demand` 0.

## 11. Implications for a single-threaded pure-Rust wasm32 port

Target facts used below: `GlobalAlloc::dealloc` and `realloc` receive the original
`Layout` (size and align), so the size class of the block is known at free time;
`alloc_zeroed` is a separate entry point; linear memory is a flat 32-bit space that only
grows in 64 KiB pages and is never unmapped; one thread, no TLS; a flat page map for the
whole address space is `2^32 / 64 KiB = 65536` entries.

### 11.1 Keep

- Free-list sharding per page with one intrusive singly linked list of raw `u32` next
  pointers (no encoding, no keys). The 7-instruction fast path of `mi_page_malloc_zero`
  (alloc.c:59-137) is the target: load direct slot, load `free`, test, load `next`, store
  `free`, increment `used`, return.
- `pages_free_direct`: 129 slots indexed by `(size + 7) >> 3` covering 0..1024 bytes,
  updated by the queue-head-change logic of page-queue.c:209-244. On wasm32 this is 516
  bytes.
- Per-bin page queues (doubly linked, `first/last/count`), the 12.5% spacing of
  `mi_bin` (four bins per doubling, page-queue.c:82-95), next-fit candidate search with
  `page_max_candidates = 4`, "prefer fuller but not mostly used" (page.c:800-819),
  move-to-front on success, and the full queue plus unfull-on-free (page.c:359-372).
- Lazy free-list extension with `MI_MAX_EXTEND_SIZE = 8 KiB` and `MI_MIN_EXTEND = 1`
  (page.c:618-623). On wasm there is no commit, but limiting the cache footprint of a
  fresh page still matters. Do not bother with bump allocation; mimalloc tried it
  (page.c:627-629).
- Retire: `retire_expire = 16` for small-page blocks and `4` for larger, at most 3
  retired pages per queue, only when `count == 1` or `block_size < 1 KiB`
  (page.c:414-457), collected every 1000 generic calls and before allocating a fresh page
  (page.c:1024-1040, 856), scanning only `[page_retired_min, page_retired_max]`.
- Page kinds and slice-based arena allocation with a binned bitmap: small pages of one
  64 KiB slice (objects up to 10 KiB), medium pages of 8 slices (up to ~85 KiB), large
  pages of 64 slices (up to 512 KiB), singleton pages beyond, i.e. the x64 constants
  (types.h:227-229, 470-473), not the 32-bit ones. The 32 KiB slice on 32-bit exists to
  save address space; on wasm the natural slice is the 64 KiB `memory.grow` unit. Keep
  `MI_ENABLE_LARGE_PAGES = 1` for speed (allocating a 100 KiB object from a large page is
  a list pop; a singleton costs a bitmap search plus header init) but note the
  fragmentation caveat at types.h:125-127 and make it a `const`.
- The chunk-bin idea of `mi_bbitmap_t` (bitmap.c:1801-1884): reserve each 512-slice
  (32 MiB) chunk for one page kind so small pages cannot fragment the runs needed for
  medium and large pages. One kind byte per chunk (128 bytes for 4 GiB) plus a summary
  bit per chunk is enough; no atomics, no `tseq`.
- `_mi_os_good_alloc_size` rounding for singletons (os.c:97-106), collapsed to "round to
  slices".
- Zero tracking per page (`free_is_zero`) and the "known-zero if never handed out" rule
  from `slices_dirty` (arena.c:253-264), because `memory.grow` returns zeroed pages.

### 11.2 Drop

Everything in section 9, and specifically:

- The page map and every lookup through it. `Layout` gives the bin, the bin gives the
  page kind, and the page kind gives the header address by masking (section 11.3). The
  flat map would only cost 64 KiB plus one dependent load per free, but the mask costs
  nothing and needs no registration/unregistration on page alloc/free
  (page-map.c:147-183). Keep an optional debug-only flat map for checked frees.
- `mi_heap_t`/`mi_theap_t`/`mi_tld_t`/`mi_subproc_t`: one static allocator struct holding
  `pages_free_direct`, `pages[75]`, `page_retired_min/max`, `generic_count`, and the
  slice bitmaps. No TLS lookup on the fast path at all (mimalloc pays one on every
  `mi_malloc`, alloc.c:270-272, and one thread-id fetch on every `mi_free`, free.c:242).
- `xthread_id`, `xthread_free`, abandonment, reclaim, `page_full_retain`, the owned bit,
  the `MI_PAGE_HAS_INTERIOR_POINTERS` flag (see 11.3), `memid` (replace with the slice
  count in the header; the slice index is the header address shifted).
- Multiple arenas, arena reservation growth, `MI_MAX_ARENAS`, commit/purge bitmaps and
  timers, `slice_pcommitted`, `mi_option_*`, stats, random, secure/debug/guarded code.
- The `local_free` list. Its purposes are the deferred-free heartbeat, bounding the
  time between generic calls, and (with `xthread_free`) keeping the fast free path free
  of `free`-list writes; a `GlobalAlloc` has no heartbeat and no other thread. Pushing
  freed blocks directly onto `free` removes one pointer from the header and the list swap
  in `mi_page_free_quick_collect` (page.c:204-212), and gives immediate LIFO reuse.
  The only things that must move: clear `free_is_zero` on the first free into a page
  (instead of on migration), and drive retired-page collection from a counter in the
  generic path rather than from list exhaustion. Benchmark this against keeping
  `local_free`; mimalloc's own tuning assumes the split.

### 11.3 Header lookup without a page map

Put the 32-byte header in-band at the start of the page's first slice (this is exactly
what the wasm build of mimalloc already does, types.h:135-141, arena.c:1013-1016), and
make every page kind aligned to its own size: small pages are one 64 KiB slice; medium
pages of 8 slices are already 8-slice aligned in mimalloc because the 8-bit search finds
whole bytes (bitmap.c:713-783); for large pages enforce 64-slice alignment in the run
search (mimalloc does not, bitmap.h:336, so a port must add an aligned-run search or
use a two-level bitmap where a large page is exactly one word of the free bitmap).

Then, at `dealloc(p, layout)`:

```
(bin, oversize) = classify(layout)                // pure function of size and align, same as alloc
kind = kind_of_bin(bin)                           // small / medium / large / singleton
header = match kind {
  small     => p & !(64 KiB - 1),
  medium    => p & !(512 KiB - 1),
  large     => p & !(4 MiB - 1),
  singleton => if layout.align <= 4 KiB { p & !(64 KiB - 1) } else { p - 64 KiB },
}
```

The singleton rule follows mimalloc's block placement: a normal singleton's block starts
inside the first slice (`mi_page_block_start`, arena.c:875-905), and an OS-aligned
singleton's block starts exactly one slice after the header (arena.c:889-890,
1064-1070). No memory is read to find the header, and the header read that follows is
the one mimalloc also pays.

For alignments up to 4 KiB, avoid over-allocation entirely by (a) rounding the requested
size up to a multiple of the alignment before binning, which always yields a bin whose
size is a multiple of the alignment because bins in `[2^k, 2^(k+1))` are spaced
`2^(k-2)` apart and the two bins at `2^k` and `1.5 * 2^k` are exact for larger
alignments, and (b) placing the first block of every page at an offset aligned to the
largest power of two dividing the bin size (capped at 4 KiB). mimalloc only does (b)
for power-of-two bins and 4 KiB multiples (arena.c:891-899, alloc-aligned.c:18-28) and
otherwise over-allocates `size + align - 1` and marks the page as having interior
pointers, which slows every subsequent free in that page (free.c:150). For alignments
above 4 KiB use a singleton page whose run of slices is found with the required
alignment (blocks at `run_start + 64 KiB`). Because `dealloc` re-derives the same
classification from `Layout`, none of this needs a per-page flag.

### 11.4 Size classes for the port

Use 8-byte words on wasm32 rather than mimalloc's 4-byte words: bins 8, 16, 24, 32, 40,
48, 56, 64, then 80, 96, 112, 128, 160, ... 524288 (the `MI_PAGE_QUEUES_EMPTY` table at
init.c:69-78 read with `W = 8`, but without the `MI_ALIGN2W` rounding that skips 24, 40
and 56 in C because of `max_align_t`). Rust gives the alignment explicitly, so a 24-byte
class is safe for align <= 8 and the rounding rule of 11.3 handles align 16. Minimum
block 8 bytes: `i64`/`f64` need 8-byte alignment on wasm32 and the free-list link needs
4. This gives 8 exact bins plus 4 per doubling over 13 doublings, 60 bins up to 512 KiB, plus a huge
queue and a full queue. mimalloc allocates `MI_BIN_COUNT = 75` queues because table indices 61..72 (up to 4 MiB in words) are never reached below `MI_BIN_HUGE` on 64-bit; a port can size its queue array to 62 (harmless either way,
16 bytes per queue on wasm32).

### 11.5 Memory acquisition

Treat linear memory as one arena that only grows: a free-slice bitmap indexed by
absolute slice number `addr >> 16` (65536 bits = 8 KiB, static), with per-chunk summary
bits and kind bytes. On a miss, `memory.grow` by `max(needed, growth_step)` where the
step grows geometrically (mimalloc quarters its 128 MiB default to 32 MiB on wasm,
arena.c:350-352, which is too coarse for a 4 GiB space; something like
`max(needed, current/8, 1 MiB)` amortises the host call without over-committing). Use
the page index returned by `memory.grow` as the start of the new region rather than
assuming contiguity, since other code may grow memory too (mimalloc's `sbrk` path has the
same defence, wasi/prim.c:89-107). Slices above the highest slice ever handed out are
zero; slices that come back through the bitmap are dirty. Nothing is ever purged, so
delete the purge machinery outright rather than making it a no-op.

Do not emulate `MI_ARENA_ALIGNMENT` gymnastics: mimalloc wastes up to a 64 KiB page per
arena and 64 KiB for its 128 KiB page map on wasi (os.c:364-369, 392-434) because it
must align OS memory after the fact; a port that owns the slice bitmap simply grows by
whole 64 KiB pages.

### 11.6 Zeroing and realloc

- `alloc_zeroed`: if the page's `free_is_zero` is set, only the first word (the free-list
  link) needs clearing (alloc.c:125-128); otherwise `memset(p, 0, layout.size())`, not the
  whole block as mimalloc does (alloc.c:123, issue #63 is a C `rezalloc` contract that
  Rust does not have).
- `realloc(p, layout, new_size)`: in place when `new_size <= block_size &&
  new_size >= block_size/2` (alloc.c:415-430); else alloc, `copy_nonoverlapping(min)`,
  dealloc. `block_size` is known from `layout` without touching the header.

### 11.7 Recommended constants

| item | value | rationale |
|---|---|---|
| slice | 64 KiB | `memory.grow` unit; one wasm page |
| small / medium / large page | 64 KiB / 512 KiB / 4 MiB, each aligned to its size | mask-based header lookup |
| small / medium / large max object | 10240 / 81920 / 524288 | mimalloc x64 values, >= 6 blocks per page |
| header | 32 bytes in-band | `free`, `used`, `capacity`, `reserved`, `block_size`, `next`, `prev`, `slice_count`, flags |
| min block / small direct limit | 8 / 1024 bytes | 8-byte words, 129 direct slots |
| bins | 8..64 by 8, then 12.5% | 11.4 |
| extend | 8 KiB per extension | page.c:618 |
| retire | 16 / 4 cycles, <= 3 pages per queue | page.c:414-457 |
| candidates | 4 | options.c:164 |
| chunk (bitmap bin unit) | 512 slices = 32 MiB | bitmap.h:22-24 |
| growth | geometric, min 1 MiB | 11.5 |

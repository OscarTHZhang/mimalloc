# mimalloc v3 Design and Architecture

This document describes the design and implementation architecture of mimalloc
v3, with particular attention to:

- the allocator's data structures;
- its thread-ownership model;
- local and cross-thread allocation behavior;
- arena and virtual-memory management;
- the mechanisms that provide high performance, low overhead, and scalability;
- the invariants that a Rust reimplementation must preserve.

## Scope and Revision

This study was performed on:

| Property | Value |
|---|---|
| Branch | `main3` |
| Commit | `31d034d94cdb8e22f7d7ed55967f581a2d6e831d` |
| Commit date | September 16, 2026 |
| Study date | September 18, 2026 |
| Version line | mimalloc v3.5.3 development line |

The existing local build directory was configured as a Debug build. Unless a
debug or secure feature is discussed explicitly, this document describes the
release-mode architecture, where the allocation and free paths are optimized.

The primary implementation sources are:

- `include/mimalloc/types.h`
- `include/mimalloc/internal.h`
- `include/mimalloc/prim-tls.h`
- `src/alloc.c`
- `src/free.c`
- `src/page.c`
- `src/page-queue.c`
- `src/arena.c`
- `src/bitmap.c`
- `src/heap.c`
- `src/theap.c`
- `src/threadlocal.c`
- `src/page-map.c`
- `src/init.c`
- `src/os.c`

## Executive Summary

The central mimalloc v3 design decision is:

> A page has one local owner, and normal allocation state is modified only by
> that owner.

Each thread allocates from thread-owned pages using ordinary non-atomic loads
and stores. A free performed by another thread does not take the page away from
its owner and does not update the local allocation list. It atomically pushes
the block onto a separate remote-free list in that page.

This produces several levels of sharding:

1. A logical heap has a separate thread-local allocator for every thread that
   uses it.
2. Each thread-local allocator has a queue for every size class.
3. Each queue can contain multiple pages.
4. Every page has separate allocation, local-free, and remote-free lists.
5. Shared arena allocation is distributed across atomic bitmap chunks.

The result is that unrelated threads rarely modify the same memory location.
The common allocation and local-free paths contain no locks and no atomic
read-modify-write operations. Cross-thread frees contend only on the page that
contains the freed block.

mimalloc v3 differs materially from the older v2 segment architecture. In v3,
pages are allocated directly from arena slices. Remaining references to
"segments" in comments and deprecated options should not be treated as the
current ownership model.

## Architectural Overview

The main relationships are:

```text
mi_subproc_t
|
+-- mi_heap_t                         shared logical heap
|   |
|   +-- dynamic TLS key
|   +-- abandoned-page indexes
|   +-- per-arena page indexes
|   |
|   +-- one mi_theap_t per using thread
|       |
|       +-- direct small-page table
|       +-- size-segregated page queues
|       +-- thread-local statistics and sampling state
|       |
|       +-- owned mi_page_t instances
|           |
|           +-- blocks of one size class
|           +-- allocation free list
|           +-- local-free list
|           +-- atomic remote-free list
|
+-- mi_arena_t[]
|   |
|   +-- 64 KiB slices
|   +-- free-slice bitmap
|   +-- committed bitmap
|   +-- dirty bitmap
|   +-- purge bitmap
|   +-- page metadata
|
+-- metadata allocator
+-- global address-to-page map
```

## Terminology

### Heap

`mi_heap_t` is a shared, first-class logical heap. It can be used for allocation
from multiple threads.

A heap owns:

- a dynamic thread-local key;
- the list of thread-local allocators currently associated with it;
- per-arena page indexes;
- abandoned-page counts and bitmaps;
- optional arena and NUMA preferences;
- heap-level statistics and profiling state.

It is not normally the object that directly performs allocation.

### Theap

`mi_theap_t` means "thread-local heap." It is the object that executes ordinary
allocations.

For each `(thread, heap)` pair that performs allocations, mimalloc lazily
creates one theap. A theap owns page queues and may only allocate or reallocate
from its associated thread. Blocks allocated by it can still be freed by any
thread.

This split is what makes a v3 first-class heap usable concurrently without
putting a lock around each heap allocation.

### Page

`mi_page_t` is the unit of:

- size segregation;
- local allocation;
- local and remote free-list sharding;
- thread ownership;
- abandonment and reclamation;
- retirement and arena release.

Except for singleton pages, every page contains blocks of one fixed size.

"Page" in this document means a mimalloc page. An operating-system page is
written explicitly as "OS page."

### Arena

`mi_arena_t` is a shared virtual-memory reservation divided into 64 KiB slices.
Pages claim one or more contiguous arena slices.

Arenas are shared by threads and heaps inside a subprocess, so their free-space,
commit, and purge state is represented with atomic bitmaps.

### Subprocess

`mi_subproc_t` is an allocator isolation domain. It owns heaps, arenas,
metadata, and statistics. Most programs use only the main subprocess.

The abstraction also supports use cases such as multiple isolated language
interpreters in one OS process.

## Important 64-bit Release Constants

The following values are derived from the current source with normal 64-bit
release defaults:

| Concept | Value |
|---|---:|
| Machine word | 8 bytes |
| Fast small-allocation threshold | 1,024 bytes |
| Direct small-page entries | 129 |
| Arena slice size | 64 KiB |
| Small page size | 64 KiB |
| Small-page maximum object size | 10 KiB |
| Medium page size | 512 KiB |
| Medium-page maximum object size | approximately 84.7 KiB |
| Large page size | 4 MiB |
| Large-page maximum object size | 512 KiB |
| Arena bitmap chunk | 512 slices, or 32 MiB |
| Initial arena reservation | 1 GiB |
| Maximum individual arena | 16 GiB |
| Metadata alignment region | 256 MiB |
| Huge size-class bin | 73 |
| Total page queues in a theap | 75 |

The 1,024-byte fast-path threshold is not the same as the 10 KiB small-page
maximum. Allocations up to 1,024 bytes use the direct lookup table. Allocations
between 1,024 bytes and 10 KiB still use 64 KiB pages, but calculate a size bin
on the generic path.

### Current Structure Sizes

The following sizes were measured from the current headers using GCC on x86-64
with release-mode preprocessor settings:

| Structure | Size |
|---|---:|
| `mi_page_t` | 128 bytes |
| `mi_page_queue_t` | 32 bytes |
| `mi_tld_t` | 112 bytes |
| `mi_memid_t` | 24 bytes |
| `mi_arena_t` fixed header | 648 bytes |
| `mi_heap_t` | 6,528 bytes |
| `mi_theap_t` | 8,224 bytes |

These are implementation and ABI dependent, not public compatibility
guarantees. The heap and theap sizes include statistics arrays and large tables
that are not all touched by the hot allocation path.

## Size Classes and Page Kinds

mimalloc uses exact word-size classes for the smallest allocations and
approximately 12.5% exponential spacing for larger allocations.

The first small word sizes have direct bins. For larger sizes, the bin is
derived from:

- the highest set bit of the requested word count;
- the next two high bits.

This creates four subdivisions per power-of-two range.

The page kind is selected by the rounded block size:

| Page kind | Page size | Block-size range |
|---|---:|---:|
| Small | 64 KiB | up to 10 KiB |
| Medium | 512 KiB | up to approximately 84.7 KiB |
| Large | 4 MiB | up to 512 KiB |
| Singleton | Variable | over 512 KiB or very large alignment |

A singleton page contains exactly one block. It may still be backed by arena
slices if its size and alignment fit the arena policy. Very large or unusually
aligned allocations fall back to direct OS allocation.

## The Thread-Local Heap

The front of `mi_theap_t` contains:

```c
mi_page_t* pages_free_direct[MI_PAGES_DIRECT];
```

For a request of at most 1,024 bytes, the request is converted to a machine-word
count and used directly as an array index. The entry points at the first page in
the corresponding size-class queue.

The rest of the theap contains:

- a pointer to its thread-local data;
- atomic links to its logical heap and subprocess;
- a lifetime reference count;
- page abandonment and reclamation policy;
- sampling and profiling state;
- a random-number context;
- page retirement bounds;
- page counts and heartbeat state;
- links in both the thread's and heap's theap lists;
- one page queue per size bin;
- statistics.

The direct array is placed first to minimize address calculation and cache
traffic for small allocations.

## First-Class Heap Threading Model

Every `mi_heap_t` has a dynamic TLS key. When a thread allocates through a
shared heap:

1. mimalloc checks a dedicated "last used theap" TLS cache;
2. if the cached theap belongs to the requested heap, it is used directly;
3. otherwise, the heap's dynamic TLS key is queried;
4. if the current thread has no theap for that heap, one is created lazily;
5. the new theap is linked into both the thread and heap lifecycle lists.

This means:

```text
shared mi_heap_t
    |
    +-- thread A -> mi_theap_t A
    +-- thread B -> mi_theap_t B
    +-- thread C -> mi_theap_t C
```

All three threads share the heap's arenas and abandoned-page indexes, but their
ordinary page queues and allocation operations are independent.

## Page Data Structure

The release-mode `mi_page_t` is currently 128 bytes and deliberately split
across two cache lines.

The first cache line contains fields used by local allocation and free:

```text
self or thread ID
free
xused
local_free
block_size
page_offset
capacity
reserved
commit and retirement state
```

The second cache line starts with:

```text
xthread_free
theap
heap
queue links
memory provenance
```

`xthread_free` begins at byte offset 64. This separates the atomic location
modified by remote freeing threads from most state modified by the owning
thread.

### Constant Page Fields

Once a page is initialized, fields such as these remain constant for its
lifetime:

- block size;
- block-area offset;
- reserved block count;
- backing-memory provenance;
- owning logical heap.

Remote frees may read the fields required to locate the block start, even when
they do not own the page. Other non-atomic mutable fields require page
ownership.

## The Three Free Lists

Every page maintains three block lists:

| List | Writer | Atomic | Used for allocation |
|---|---|---:|---:|
| `free` | Page owner | No | Yes |
| `local_free` | Page owner | No | Not immediately |
| `xthread_free` | Any thread | Yes | After collection |

The block itself is the list node. The first machine word of a free block stores
the next pointer. No separate free-list node is allocated.

The key accounting invariants are:

```text
used - length(xthread_free) = number of actually live blocks
```

and:

```text
used - length(xthread_free)
  + length(free)
  + length(local_free)
  = capacity
```

The used count includes remotely freed blocks until the owning or temporary
collecting thread processes `xthread_free`.

### Why Local Frees Are Deferred

A local free pushes onto `local_free`, not directly onto `free`.

When `free` is exhausted, `local_free` is transferred to `free`. This has two
important effects:

1. The allocation head is not modified on every local free.
2. Exhausting `free` creates a deterministic administrative heartbeat.

Runtime systems can register deferred work that is invoked on these
administrative paths without adding a callback check to every allocation.

## Allocation Fast Path

For a normal small allocation:

```text
mi_malloc(size)
  |
  +-- load the default theap from TLS
  +-- index pages_free_direct by word count
  +-- load page->free
  +-- load the next pointer from the block
  +-- update page->free
  +-- increment page usage and allocation counters
  +-- return the block
```

There are no locks and no atomic read-modify-write instructions in this path.

The `xused` word packs the used and allocation counters. Allocation increments
both 16-bit counters with:

```c
xused.used_alloc += 0x10001;
```

This reduces field traffic and improves generated code.

## Generic Allocation Path

The direct allocation path falls back when:

- the direct page has no free block;
- the allocation exceeds the direct-size threshold;
- the theap is not initialized;
- sampling or guarded allocation is required;
- special alignment is requested.

The generic path:

1. initializes the thread and theap if needed;
2. selects the size-class queue;
3. collects local and remote frees from candidate pages;
4. looks for an immediately available page;
5. extends a partially initialized page if possible;
6. releases expired retired pages;
7. attempts to reclaim an abandoned page of the exact size class;
8. allocates fresh arena slices;
9. reserves another arena if necessary;
10. falls back to direct OS allocation when the arena policy does not apply.

If allocation initially fails, mimalloc performs a forced collection and tries
once more before reporting out of memory.

## Page Candidate Selection

Page search is not a pure first-fit scan.

The queue search examines a limited number of candidates, four by default. It
generally prefers the fuller usable page. Concentrating new allocations into a
fuller page increases the probability that a less-used page becomes completely
empty and can be returned to its arena.

This policy improves memory density without maintaining a globally sorted
structure.

Long-lived full pages are moved, retained, or abandoned so they are not scanned
on every allocation.

## Incremental Page Initialization

`reserved` and `capacity` are distinct:

```text
capacity <= reserved
```

- `reserved` is the number of blocks that fit in the page's address range.
- `capacity` is the number of blocks currently initialized and linked into the
  free list.

When extending a page, mimalloc normally initializes at most 8 KiB worth of
blocks at once. For commit-on-demand pages, it also commits backing memory
incrementally.

This avoids:

- touching an entire large page for a small number of allocations;
- constructing large free lists unnecessarily;
- converting virtual reservation into resident memory too early;
- losing zero-initialized OS pages before they are needed.

## Local Free Path

`mi_free` first recovers the page descriptor and compares the current thread ID
with the page's encoded owner ID.

The common local path:

```text
recover page
  |
  +-- confirm same thread and no exceptional page flags
  +-- validate or normalize the block pointer
  +-- decrement the used count
  +-- push the block onto local_free
  +-- retire the page if its used count becomes zero
```

This path uses no lock and no atomic read-modify-write operation.

## Thread ID and Page Flags

The low two bits of the page's atomic `xthread_id` are page flags:

- page is in the full queue;
- page contains interior or specially aligned pointers.

Real thread IDs are guaranteed to have these bits clear.

`mi_free` XORs the current thread ID with `xthread_id`. The result classifies
both thread ownership and page flags:

| Result | Meaning | Free path |
|---|---|---|
| Zero | Same thread, no flags | Fast local |
| Only flag bits set | Same thread, exceptional flags | Generic local |
| Different thread, no flag bits | Remote or abandoned, simple pointer | Fast remote |
| Different thread with flags | Remote or abandoned, exceptional pointer | Generic remote |

This avoids separate owner and flag tests on the common path.

## Cross-Thread Free Path

A cross-thread free pushes the block onto the page's atomic `xthread_free`
list:

```text
old = atomic_load(xthread_free)

loop:
    block.next = pointer_part(old)
    new = block_pointer | OWNED_BIT
    if compare_exchange(xthread_free, old, new):
        break
```

The usual remote free requires one successful CAS. It does not:

- acquire the owning thread's heap lock;
- modify the owning page queue;
- decrement the page's non-atomic used count;
- interact with a global remote-free queue.

Contention is therefore limited to remote frees targeting the same page.
Remote frees targeting different pages update different atomic words.

## Page Ownership Protocol

The low bit of `xthread_free` is an ownership bit:

```text
xthread_free = remote_free_head | ownership_bit
```

The central rule is:

> Mutable non-atomic page fields may be accessed only while the page ownership
> bit is held.

An active page is owned by its theap. When the page is abandoned, ownership is
released.

The first remote free that changes the ownership bit from zero to one:

1. publishes its block on the remote-free list;
2. atomically acquires temporary page ownership;
3. becomes responsible for collecting or redistributing the page.

That thread may:

- process remote frees;
- release the page if it became empty;
- reclaim the page into a local theap;
- publish it in an abandoned-page bitmap;
- release ownership again.

Combining the free-list head and ownership state in one word avoids a separate
lock or ownership field.

## Collecting Remote Frees

The normal owner collects remote frees by atomically replacing the remote-list
head with null while preserving the ownership bit.

It then:

1. walks the detached list;
2. validates its length against page capacity and used count;
3. appends it to `local_free`;
4. subtracts the collected count from `used`;
5. eventually moves `local_free` to `free`.

An optimized partial collector is used by the first remote free that claims an
abandoned page. It avoids an extra atomic exchange when it already has a pointer
into the remote list.

## Page Lifecycle

A normal page moves through these states:

```text
fresh arena slices
        |
        v
owned active page in a theap queue
        |
        +-- full but temporarily retained
        |
        +-- abandoned
        |      |
        |      +-- mapped as reusable by size class
        |      +-- unmapped because it is full or special
        |
        +-- reclaimed by allocation
        +-- reclaimed by a qualifying remote free
        |
        v
empty page
        |
        +-- temporarily retired
        |
        v
returned to arena
        |
        v
scheduled for reset or decommit
```

## Full-Page Retention

Small full pages are not necessarily abandoned immediately. By default, a theap
can retain two full small pages per size class.

This avoids ownership transfer when a full page is likely to receive a free
soon. Additional full small pages, and full medium or large pages, are more
aggressively abandoned.

The retained pages remain in the ordinary size-class queue but are moved away
from the queue head to reduce repeated scanning.

## Empty-Page Retirement

Immediately releasing an empty page can cause allocation/free oscillation in
workloads that repeatedly empty and reuse one size class.

mimalloc therefore supports short retirement:

- an empty page can remain available for approximately 16 administrative
  cycles;
- at most three pages per size bin are retained this way;
- forced collection releases them immediately.

Retirement is different from abandonment:

- a retired page is empty and still owned by its theap;
- an abandoned page contains live blocks and has no active thread owner.

## Thread Termination

When a thread terminates:

1. each of its theaps collects local and remote frees;
2. empty pages are released;
3. pages with live blocks are abandoned;
4. the default and cached theap TLS entries are reset;
5. the theaps are detached from their heaps;
6. the theap and thread-local metadata are released.

The thread and heap lists are protected only during lifecycle operations.
Ordinary allocation does not traverse them.

Special initialization and teardown paths exist because:

- some platform TLS implementations can allocate on first access;
- allocator initialization must not recursively call itself;
- thread-local destructors may run late during process or library shutdown;
- a heap can be destroyed concurrently with thread termination.

## Abandoned Pages

A page is logically abandoned when its encoded thread ID is one of the special
abandoned values.

The page's `theap` field is deliberately retained as an origin hint after
logical abandonment. Code must not determine abandonment solely from whether
`page->theap` is null.

The retained origin pointer allows mimalloc to determine whether a later free
occurred on the page's original thread-local allocator, without making the
origin the current owner.

## Mapped Abandoned Pages

An abandoned arena page with useful free capacity is recorded in:

```text
heap
  -> arena_pages[arena_index]
       -> pages_abandoned[size_bin]
```

Each `pages_abandoned` entry is an atomic bitmap indexed by the page's starting
arena slice.

This indexes abandoned pages by:

- logical heap;
- arena;
- block-size bin;
- slice address.

An allocation first checks an atomic abandoned count for its size bin. If the
count is nonzero, it searches only the corresponding bitmaps.

This replaces the older segment-wide abandoned-list design with a more direct
page-level lookup.

## Full Abandoned Pages

A completely full abandoned page is not useful for allocation, so it is not
placed in the reusable abandoned bitmap.

As remote frees arrive, the first freeing thread to acquire temporary ownership
updates the page state.

It may:

- free the page if it became empty;
- reclaim it into a suitable local theap;
- remap it into the abandoned bitmap once it is no longer more than
  approximately 7/8 full;
- release ownership while leaving it unmapped if it still has too little free
  capacity.

This avoids repeatedly advertising pages that are unlikely to satisfy more than
a small number of allocations.

## Reclaim-on-Free Policy

The current default is conservative:

- allocation-time reclamation from abandoned bitmaps is enabled;
- reclaim-on-free into the page's originating theap is allowed;
- arbitrary cross-thread reclaim-on-free is disabled by default;
- configurable limits prevent a theap from accumulating too many reclaimed
  pages in one size class.

Cross-thread reuse still occurs. It is simply driven primarily by allocation
demand rather than by whichever thread happens to free a block.

Threads marked as thread-pool workers use more conservative reclamation and
retention behavior because future tasks may have unrelated allocation patterns.

## Abandoned-Bitmap Race Protocol

There is a race between:

- an allocator clearing an abandoned bitmap bit and trying to claim the page;
- a remote free claiming the same page through the ownership bit.

The allocation-side protocol is:

1. atomically clear an abandoned bitmap bit;
2. try to acquire the page ownership bit;
3. if ownership acquisition fails, restore the bitmap bit;
4. continue searching.

The free-side `unabandon` operation can wait until an allocator involved in
this protocol has restored the bit, then clear it definitively.

This design avoids a global abandoned-page lock, but it can briefly busy-wait.
The allocator's common paths are lock-free in the practical sense, but the
entire implementation is not wait-free.

## Arena Structure

An arena is a large virtual-memory range divided into 64 KiB slices.

The arena contains or references:

- its backing-memory provenance;
- its subprocess and arena index;
- base address and slice count;
- NUMA and exclusivity settings;
- purge expiration state;
- optional custom commit callbacks;
- free-slice state;
- commit state;
- dirty state;
- scheduled purge state;
- page metadata;
- main-heap page indexes.

The variable-size bitmap storage follows the fixed arena header.

## Arena Atomic Bitmaps

The main arena bitmaps are:

| Bitmap | Meaning |
|---|---|
| `slices_free` | Slices available for allocation |
| `slices_committed` | Slices that are currently accessible |
| `slices_dirty` | Slices that may contain nonzero data |
| `slices_purge` | Free slices waiting to be reset or decommitted |

The free-slice bitmap is a binned, two-level atomic bitmap.

### Bitmap Fields and Chunks

A bitmap bit normally represents one 64 KiB slice.

On 64-bit platforms:

```text
one bitmap field = one 64-bit machine word
one bitmap chunk = eight fields = 512 bits
one chunk covers 512 * 64 KiB = 32 MiB
```

Chunks are cache-line aligned.

### Chunk Map

Scanning every chunk in a large arena would be expensive. A top-level chunk map
contains one bit per chunk and indicates whether that chunk may contain
available slices.

The chunk map is conservative:

- it may say that a chunk has space when it no longer does;
- it may be briefly clear during a race if code guarantees it will be restored.

This can cause a search to miss available memory temporarily. That is accepted
in exchange for avoiding a global lock or a more complicated epoch protocol.

### Chunk Size Bins

Free chunks are assigned an allocation category when first used:

- small page;
- medium page;
- large page;
- other;
- huge.

Searches prefer chunks assigned to the matching category and then unassigned
chunks. A small allocation therefore avoids fragmenting a chunk reserved for
medium or large pages.

This is coarse-grained segregation at the arena level, complementing the exact
block-size segregation inside pages.

### Distributing Bitmap Contention

Thread and heap sequence numbers influence the starting location for arena and
bitmap scans.

Threads do not all begin with the lowest arena and lowest bitmap word. Search
rotation spreads atomic claims across:

- arenas;
- chunk-map fields;
- bitmap chunks.

## Arena Reservation and Growth

If no existing arena has suitable free slices:

1. the thread enters the subprocess's arena-reservation lock;
2. it verifies that another thread did not already reserve an arena;
3. one new arena is reserved;
4. the allocation retries the normal atomic bitmap search.

The lock is reached only after existing arena searches fail.

On 64-bit systems:

- the initial configured arena reservation is 1 GiB;
- arena size doubles every eight arenas;
- an individual arena is capped at 16 GiB;
- a failed large reservation can fall back to 128 MiB.

Large externally supplied memory ranges can be represented by a parent arena
and multiple child arenas.

## Arena Search Locality

Arena search attempts to preserve locality and reduce contention:

- heaps are spread over different portions of the arena sequence;
- threads within a heap are spread within that heap's portion;
- a preferred NUMA node is searched first;
- nonmatching NUMA arenas are tried only after local candidates fail;
- exclusive heaps search only their requested arena and its children.

## Page Allocation from Arenas

Fresh page allocation performs these steps:

1. search for an abandoned page in the exact size class;
2. claim a suitable slice span from `slices_free`;
3. establish commit and zeroing state;
4. locate or initialize the page descriptor;
5. set block size, capacity, reservation, and memory provenance;
6. associate the page with the logical heap and current theap;
7. claim page ownership;
8. register the page in the address-to-page map;
9. initialize the first portion of its block free list.

Regular page sizes claim fixed slice counts:

| Page kind | Slice count |
|---|---:|
| Small | 1 |
| Medium | 8 |
| Large | 64 on 64-bit |

Singleton pages claim however many slices are required by the block and its
alignment.

## Page Metadata Placement

The normal 64-bit release configuration uses:

```text
MI_PAGE_MAP_FLAT              = 0
MI_PAGE_META_IS_SEPARATED     = 1
MI_PAGE_META_IS_ALIGNED       = 1
MI_PAGE_META_SMALL_IS_ALIGNED = 1
```

### Aligned Metadata Regions

Page metadata is organized relative to 256 MiB-aligned address regions.

Given an allocation pointer, mimalloc can:

1. align the pointer down to the metadata-region base;
2. derive its 64 KiB slice index;
3. index a page-metadata slot;
4. load the slot's `self` pointer.

This allows ordinary `mi_free` to recover the page arithmetically without a
general tree or hash-table lookup.

### Small-Page Optimization

For small pages, the actual `mi_page_t` is also placed at the start of the
64 KiB page slice. The specialized `mi_free_small` operation can align its
pointer directly down to 64 KiB.

This is an explicit space-versus-instruction tradeoff for language runtimes and
other callers that know the allocation was small.

## Address-to-Page Map

mimalloc also maintains a global page map.

In the normal 64-bit configuration it is a two-level structure:

- the first level contains atomic submap pointers;
- each submap covers a large virtual-address range;
- each entry maps a 64 KiB slice to its `mi_page_t`;
- first-level storage and submaps are committed or allocated on demand.

The page map supports:

- checked pointer validation;
- secure and checked-free configurations;
- `mi_is_in_heap_region`;
- safe heap ownership queries;
- configurations where aligned metadata recovery is unavailable.

Registration occurs before a page can return blocks to callers. Unregistration
occurs only after the page is empty and owned, so no valid live allocation can
subsequently require that mapping.

## Memory Provenance

`mi_memid_t` records how each memory range was obtained and how it must be
released.

Memory kinds include:

- none;
- external memory;
- static memory;
- direct OS memory;
- huge OS pages;
- remappable OS memory;
- arena memory;
- memory allocated through mimalloc itself.

The record also tracks:

- the true base and full size for OS allocations;
- arena pointer, slice index, and slice count for arena allocations;
- whether memory is pinned;
- whether it was initially committed;
- whether it was initially zero.

This avoids inferring release behavior only from an address.

## Commit, Dirty, and Zero State

Arena allocation distinguishes:

- virtual reservation;
- committed accessibility;
- whether memory may contain old data;
- whether the OS guarantees initial zeroing.

When an allocation claims free slices:

1. dirty bits are updated;
2. commit bits are inspected;
3. missing ranges are committed if requested;
4. the returned `mi_memid_t` records whether the result is committed and zero;
5. page initialization avoids zeroing memory unnecessarily.

This is essential to low resident-memory overhead. A large virtual reservation
does not imply that every page is immediately resident.

## Page Release and Purging

Returning an empty page to an arena immediately republishes its slices in the
free bitmap. The allocator can reuse them without waiting for an OS operation.

Reset or decommit is scheduled separately.

Current defaults are:

```text
base purge delay       = 1,000 ms
arena delay multiplier = 4
effective arena delay  = approximately 4 seconds
```

When the delay expires, a purging thread:

1. tries to claim the still-free slice range;
2. abandons the purge if another allocation has reused it;
3. otherwise resets or decommits the range;
4. updates the commit bitmap;
5. republishes the free slices.

This avoids reset/decommit and recommit oscillation.

On Linux, the current configuration can preserve transparent huge pages by
using a larger minimum purge unit rather than splitting them into small OS-page
ranges.

## OS Abstraction

`src/os.c` and `src/prim/*` separate allocator policy from platform primitives.

The abstraction provides:

- virtual reservation;
- aligned reservation;
- commit and decommit;
- reset or discard;
- guard-page protection;
- large and huge OS pages;
- NUMA discovery;
- virtual-address-width discovery;
- platform thread and process hooks.

The arena and page algorithms depend on semantic properties such as:

- whether the OS supports reserve without commit;
- whether it overcommits;
- whether partial ranges can be released;
- whether reset keeps memory committed;
- whether large pages are pinned;
- whether transparent huge pages should be preserved.

## TLS Models

mimalloc supports several platform-specific methods for retrieving the default
theap:

| Model | Typical use |
|---|---|
| Compiler thread-local variable | Linux and similar systems |
| `pthread_getspecific` or direct pthread slots | macOS and some Unix systems |
| Dynamic Windows TLS slot | Windows |
| Fixed platform TLS slot | Specialized configurations |

Some models can return null before initialization; others return a statically
allocated empty theap.

The empty theap is constructed so an allocation can enter the generic
initialization path without first checking a separate global initialization
flag.

The initial main-thread TLD and theap are also statically allocated. This avoids
requiring the allocator to allocate its own first allocator metadata.

## Dynamic TLS for First-Class Heaps

In addition to the fast default and cached theap TLS values, v3 implements
dynamic thread-local keys for arbitrary first-class heaps.

Each thread has a dynamically growing array of:

```text
slot version
slot value
```

A key combines:

- a slot index;
- a generation version.

The version prevents an old per-thread value from becoming visible when a TLS
slot is freed and reused for another heap.

The main heap uses a dedicated fast key. Additional heap keys are allocated
from a shared bitmap and are looked up in the per-thread array.

## Synchronization Inventory

The synchronization behavior can be summarized as:

| Operation | Synchronization |
|---|---|
| Small local allocation | None |
| Local free | None |
| Transfer `local_free` to `free` | None |
| Remote free | CAS on one page |
| Collect remote frees | Atomic exchange or CAS on one page |
| Claim abandoned page | Atomic bitmap operation plus page ownership bit |
| Allocate arena slices | Atomic binned bitmap |
| Publish released arena slices | Atomic binned bitmap |
| Reserve a new arena | Subprocess lock |
| Create per-arena heap indexes | Heap lock |
| Create or remove heap TLS keys | TLS-key lock |
| Link or detach theaps | Heap and TLD lifecycle locks |
| Allocate a page-map submap | Page-map lock |
| Allocate through the detached metadata theap | Metadata lock |

The locks are primarily used for:

- initialization;
- metadata allocation;
- rare structure expansion;
- heap creation or destruction;
- thread termination;
- arena reservation.

They are not part of ordinary local `malloc` or `free`.

## Why the Design Performs Well

### Local Operations Are Truly Local

The owner performs ordinary page operations without atomics. This matters more
than merely avoiding locks: even uncontended atomic read-modify-write
instructions can impose cache-coherence and ordering costs.

### Remote Contention Is Page-Sharded

There is no single remote-free queue for a heap or process. A remote free
updates only the page containing the block.

### Shared Heaps Decompose into Local Theaps

A first-class heap does not become an allocation bottleneck when many threads
use it. Every participating thread allocates through a private theap.

### Size and Address Locality

Objects of one size are placed together. Objects allocated close in time tend
to come from the same active page.

### Fuller Pages Are Preferred

Concentrating allocations increases the chance that other pages become empty
and releasable.

### Backing-Memory Search Is Bitmap-Based

Arena allocation uses bitscan and atomic word operations rather than a shared
balanced tree of free extents.

### Arena Searches Are Distributed

Thread and heap sequence numbers vary starting positions, reducing systematic
contention on the first bitmap words.

### Memory Is Touched Incrementally

Pages initialize and commit only part of their capacity at a time.

### Release to the Allocator Is Separate from Release to the OS

Arena slices are reusable immediately, while expensive OS purging is delayed.

### Metadata Lookup Is Address-Derived

Ordinary free can usually derive its page descriptor from address alignment,
avoiding a general map lookup.

## Memory-Overhead Properties

The low-overhead design comes from:

- intrusive free lists stored inside free blocks;
- fixed-size page descriptors;
- bitmap representation of arena state;
- lazy creation of per-thread per-heap theaps;
- virtual reservation without eager physical commitment;
- short bounded page retirement;
- release of empty pages to shared arenas;
- exact abandoned-page lookup instead of retaining large thread-private memory
  regions.

There are still explicit fixed costs:

- one TLD per initialized thread;
- one theap per `(thread, heap)` pair that allocates;
- one page descriptor per relevant arena slice;
- one set of abandoned-page bitmaps per `(heap, arena)` pair in use;
- per-heap and per-theap statistics.

These costs are designed to be predictable and are generally paid lazily.

## Fragmentation Controls

mimalloc controls fragmentation at several levels:

| Level | Mechanism |
|---|---|
| Block | Approximately 12.5% size-class spacing |
| Page | One block size per page |
| Page selection | Prefer fuller pages |
| Theap | Limit retained full and retired pages |
| Cross-thread | Abandon pages so another thread can reuse them |
| Arena chunk | Prefer matching small/medium/large chunk bins |
| Arena | Reuse freed slices before requesting OS memory |
| OS | Purge unused ranges after a delay |

No single mechanism is sufficient by itself. The important v3 improvement is
that pages containing live cross-thread objects no longer remain permanently
stranded in a terminated or inactive thread's private memory region.

## Security and Debug Modes

Optional configurations add:

- encoded free-list pointers with per-page keys;
- padding canaries;
- double-free detection;
- randomized free-list construction;
- randomized address selection;
- metadata guard pages;
- guard pages after mimalloc pages;
- checked pointer-to-page lookup.

These modes change structure layout and hot-path behavior. The release-mode
architecture should therefore be implemented and measured separately from
security instrumentation.

## Core Correctness Invariants

A reimplementation must preserve at least the following invariants.

### Page Accounting

```text
0 <= used <= capacity <= reserved
```

After remote frees are fully collected:

```text
used + length(free) + length(local_free) = capacity
```

Before collection:

```text
used - length(xthread_free) = live blocks
```

### Ownership

- An active page has exactly one owning theap.
- Only the owner may mutate non-atomic page state.
- An abandoned page has no thread owner.
- The remote-free ownership bit protects temporary access to abandoned page
  state.
- A page cannot be released while any live block from it exists.

### Registration

- A page is registered before any block can be returned to a caller.
- A page remains registered while any block is live.
- A page is unregistered only when empty and owned.

### Arena Slices

- A claimed page's slices are clear in `slices_free`.
- A released page's slices are set in `slices_free`.
- Purging must claim a free range before changing its OS state.
- A range is republished after purge completes.

### Heap and Theap

- Each theap belongs to exactly one logical heap.
- Each ordinary theap belongs to one thread.
- A heap can have at most one theap per participating thread.
- Heap destruction and thread termination must detach shared list links before
  freeing metadata.

### Memory Provenance

- Every allocation of page or metadata backing memory records how it was
  obtained.
- Release uses the recorded provenance rather than address heuristics.

## Implications for a Rust Reimplementation

A Rust implementation should preserve the architecture rather than translate
the C files mechanically.

One possible module decomposition is:

```text
allocator/
  block.rs
  page.rs
  remote_free.rs
  size_class.rs
  page_queue.rs
  thread_heap.rs
  heap.rs
  abandoned.rs
  bitmap.rs
  arena.rs
  page_map.rs
  memid.rs
  tls/
  os/
  facade.rs
```

### Concentrate Unsafe Code Around Invariants

Useful internal abstractions may include:

- a page-ownership token permitting non-atomic access;
- an abandoned-page handle exposing only atomic and immutable fields;
- a remote-free head responsible for pointer tagging and memory ordering;
- a claimed arena span;
- a page registration guard;
- a memory-provenance value controlling release.

The public allocation path should call a small unsafe allocator core instead of
duplicating unsafe pointer operations throughout the API.

### Intrusive Free Lists

Free blocks contain allocator metadata. Rust references cannot be created
casually for memory that is:

- uninitialized;
- currently owned by the caller;
- already freed;
- concurrently linked by another thread.

Free-list operations should use raw pointers and explicit lifetime-independent
operations. Safe references should exist only while their uniqueness and
validity are proven.

### Tagged Atomic Pointers

The remote-free head combines:

- a block pointer;
- an ownership bit.

The Rust implementation must define:

- alignment requirements that make the low bit available;
- provenance-preserving pointer conversion rules;
- compare-exchange memory ordering;
- when a block's next pointer becomes visible;
- when ownership permits non-atomic field access.

This component deserves an isolated proof and dedicated concurrency tests.

### TLS Must Be Recursion-Safe

Ordinary high-level TLS initialization may call into a platform runtime that
allocates memory.

The allocator must be able to:

- return a static empty state before full initialization;
- initialize the main thread without allocating allocator metadata through
  itself;
- support platform-specific fast TLS;
- run correctly during late thread-local destruction.

### Avoid Allocating Allocator Metadata Normally

Core allocator metadata cannot depend on general Rust containers that allocate
through the allocator being implemented.

Metadata structures need:

- static bootstrap storage;
- arena-backed allocation;
- direct OS allocation;
- fixed-capacity or manually expanded tables.

### Separate Policy from OS Primitives

The arena algorithm should consume a small platform interface describing:

- reserve;
- commit;
- decommit;
- reset;
- protect;
- release;
- page sizes;
- address width;
- NUMA topology;
- large-page capabilities.

Platform code should not decide page-reclamation or size-class policy.

## Recommended Rust Implementation Order

1. Implement size classes and page layout.
2. Implement a single-threaded page with `free` and `local_free`.
3. Implement page queues and direct small-size lookup.
4. Implement arena slices and nonconcurrent bitmaps.
5. Implement page registration and pointer-to-page recovery.
6. Implement thread-local theaps and bootstrap initialization.
7. Implement atomic remote free without abandonment.
8. Prove and test the ownership-bit protocol.
9. Implement page abandonment and allocation-time reclamation.
10. Implement first-class heaps with lazy per-thread theaps.
11. Implement retirement and delayed OS purging.
12. Implement heap destruction and thread shutdown races.
13. Add NUMA, huge pages, profiling, and security modes.
14. Add allocator replacement and C ABI integration.

The remote-free and abandoned-page state machines should not be implemented
until the single-owner page lifecycle is stable and thoroughly tested.

## Suggested Source Reading Order

| Order | Source | Focus |
|---:|---|---|
| 1 | `include/mimalloc/types.h` | Core data model |
| 2 | `src/alloc.c` | Allocation fast path |
| 3 | `src/free.c` | Local and remote free |
| 4 | `src/page.c` | Collection, retirement, abandonment |
| 5 | `src/page-queue.c` | Size bins and queue maintenance |
| 6 | `src/arena.c` | Page and arena allocation |
| 7 | `src/bitmap.h`, `src/bitmap.c` | Atomic backing-memory indexes |
| 8 | `include/mimalloc/prim-tls.h` | Fast TLS lookup |
| 9 | `src/theap.c`, `src/heap.c` | Heap and theap lifecycle |
| 10 | `src/init.c`, `src/threadlocal.c` | Bootstrap and teardown |
| 11 | `src/page-map.c` | Address-to-page mapping |
| 12 | `src/os.c`, `src/prim/*` | Platform memory operations |

## Conclusion

mimalloc v3 is best understood as a hierarchy of ownership and sharding:

```text
subprocess
  -> shared heaps
    -> per-thread theaps
      -> size-class page queues
        -> individually owned pages
          -> separate local and remote free lists
```

The page is the fundamental unit of allocation, contention, ownership,
abandonment, reclamation, and release.

The architecture achieves high performance by keeping the common path local,
and achieves scalability by ensuring that the uncommon shared paths are
distributed across many pages and bitmap words. It achieves low memory overhead
by using intrusive lists, fixed metadata, lazy commitment, compact bitmaps, and
page-level cross-thread reuse.

For a Rust rewrite, the primary design task is not exposing a `malloc`-shaped
API. It is representing the page ownership and lifetime protocol so that the
compiler, the unsafe implementation, and future maintainers all agree on when
page state may be accessed and by whom.

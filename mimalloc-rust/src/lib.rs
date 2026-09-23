//! A bottom-up Rust rewrite of mimalloc v3.
//!
//! Allocator behavior will be added one small, reviewed concept at a time.
//!
//! Phase 2 links unused blocks into a single-threaded intrusive free list.
//!
//! ```text
//! Page memory
//! +----------------+----------------+----------------+----------------+
//! | block 0: free  | block 1: used  | block 2: free  | block 3: free  |
//! | next -> block2 | user bytes     | next -> block3 | next -> null   |
//! +----------------+----------------+----------------+----------------+
//!          ^
//!          |
//! free list head -> block 0 -> block 2 -> block 3 -> null
//! ```

// TODO: remove this once they are used.
#![allow(dead_code)]

use std::ptr::NonNull;

use rustix::{
    io::Errno,
    mm::{MapFlags, MprotectFlags, ProtFlags, mmap_anonymous, mprotect, munmap},
};

/// Stores a singly linked list inside the free blocks themselves.
///
/// The list does not own block memory. Each free block stores the address of
/// the next free block in its first pointer-sized bytes. The list must not
/// outlive the memory containing its blocks.
struct IntrusiveFreeList {
    /// First free block, or `None` when the list is empty.
    head: Option<NonNull<u8>>,
}

impl IntrusiveFreeList {
    /// Creates an empty free list.
    fn new() -> Self {
        Self { head: None }
    }

    /// Removes and returns the first free block.
    fn take(&mut self) -> Option<NonNull<u8>> {
        let block = self.head?;

        // SAFETY: A block in `head` contains a link written by this list.
        self.head = unsafe { Self::read_next(block) };
        Some(block)
    }

    /// Adds a block to the front of the free list.
    ///
    /// # Safety
    ///
    /// `block` must point to live, writable, pointer-aligned storage containing
    /// at least `size_of::<*mut u8>()` bytes. It must remain live while linked
    /// and must not already belong to a free list.
    unsafe fn return_block(&mut self, block: NonNull<u8>) {
        // SAFETY: The caller guarantees that `block` is available for reuse.
        unsafe { Self::write_next(block, self.head) };
        self.head = Some(block);
    }

    /// Reads the next-free link stored at the start of a free block.
    ///
    /// # Safety
    ///
    /// `block` must point to a live, readable, pointer-aligned link previously
    /// initialized by `write_next`.
    unsafe fn read_next(block: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: The caller guarantees that `block` contains an initialized link.
        let next = unsafe { block.cast::<*mut u8>().as_ptr().read() };
        NonNull::new(next)
    }

    /// Writes a next-free link into the start of a free block.
    ///
    /// # Safety
    ///
    /// `block` must point to live, writable, pointer-aligned storage containing
    /// at least `size_of::<*mut u8>()` bytes.
    unsafe fn write_next(block: NonNull<u8>, next: Option<NonNull<u8>>) {
        let next_pointer = next.map_or(std::ptr::null_mut(), NonNull::as_ptr);

        // SAFETY: The caller guarantees that the block is writable and free.
        unsafe { block.cast::<*mut u8>().as_ptr().write(next_pointer) };
    }
}

/// Owns one contiguous allocator page divided into equal-size blocks.
///
/// The page obtains its memory directly from the operating system. A block is
/// one allocation slot within that mapping. Dropping the page releases the
/// complete mapping.
///
/// A valid page guarantees:
///
/// - the page and block sizes are nonzero;
/// - the page contains only complete blocks;
/// - every valid block pointer is within the page.
struct Page {
    /// Starting address of this page's memory.
    start: NonNull<u8>,
    /// Total number of mapped bytes owned by the page.
    size: usize,
    /// Number of bytes in each block.
    block_size: usize,
    /// Number of complete blocks that fit in the page.
    block_count: usize,
    /// Blocks currently available for allocation.
    free_list: IntrusiveFreeList,
}

impl Page {
    /// Maps a page when its size divides evenly into blocks.
    fn new(page_size: usize, block_size: usize) -> rustix::io::Result<Self> {
        let free_link_size = size_of::<*mut u8>();
        let free_link_alignment = align_of::<*mut u8>();
        if page_size == 0
            || block_size < free_link_size
            || !block_size.is_multiple_of(free_link_alignment)
            || !page_size.is_multiple_of(block_size)
        {
            return Err(Errno::INVAL);
        }

        // Reserve the address range before making it accessible.
        let mapping = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                page_size,
                ProtFlags::empty(),
                MapFlags::PRIVATE,
            )
        }?;
        debug_assert!(!mapping.is_null(), "successful mmap returned null");

        // SAFETY: `mapping` identifies the complete live mapping.
        if let Err(error) = unsafe {
            mprotect(
                mapping,
                page_size,
                MprotectFlags::READ | MprotectFlags::WRITE,
            )
        } {
            // SAFETY: The failed protection change does not invalidate the mapping.
            unsafe { munmap(mapping, page_size) }
                .expect("the reserved mapping should still be releasable");
            return Err(error);
        }

        // SAFETY: A successful Linux `mmap` without `MAP_FIXED` returns a
        // nonzero page-aligned address.
        let start = unsafe { NonNull::new_unchecked(mapping.cast()) };

        let mut page = Self {
            start,
            size: page_size,
            block_size,
            block_count: page_size / block_size,
            free_list: IntrusiveFreeList::new(),
        };

        for block_index in (0..page.block_count).rev() {
            let block = page
                .block_ptr(block_index)
                .expect("the block index should be within the page");

            // SAFETY: The new page owns every suitably sized and aligned block.
            unsafe { page.free_list.return_block(block) };
        }

        Ok(page)
    }

    /// Returns the total number of mapped bytes.
    fn size(&self) -> usize {
        self.size
    }

    /// Returns the number of bytes in each block.
    fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the number of blocks in the page.
    fn block_count(&self) -> usize {
        self.block_count
    }

    /// Returns a block pointer, or `None` when the index is outside the page.
    fn block_ptr(&self, block_index: usize) -> Option<NonNull<u8>> {
        if block_index >= self.block_count {
            return None;
        }

        let offset = block_index.checked_mul(self.block_size)?;

        // SAFETY: The validated block offset is within the live page mapping.
        Some(unsafe { NonNull::new_unchecked(self.start.as_ptr().add(offset)) })
    }

    /// Removes and returns the first free block.
    fn take_free_block(&mut self) -> Option<NonNull<u8>> {
        self.free_list.take()
    }

    /// Returns an allocated block to the front of the free list.
    ///
    /// # Safety
    ///
    /// `block` must have been taken from this page, must no longer be accessed
    /// by the caller, and must not already be free.
    unsafe fn return_free_block(&mut self, block: NonNull<u8>) {
        debug_assert!(self.contains_block(block));

        // SAFETY: The caller guarantees that `block` is available for reuse.
        unsafe { self.free_list.return_block(block) };
    }

    /// Returns whether the pointer identifies a block boundary in this page.
    fn contains_block(&self, block: NonNull<u8>) -> bool {
        let start_address = self.start.as_ptr() as usize;
        let block_address = block.as_ptr() as usize;
        let Some(offset) = block_address.checked_sub(start_address) else {
            return false;
        };

        offset < self.size && offset.is_multiple_of(self.block_size)
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: `start` and `size` identify the live mapping owned by this page.
        unsafe { munmap(self.start.as_ptr().cast(), self.size) }
            .expect("the page mapping should be releasable");
    }
}

#[cfg(test)]
mod tests {
    use super::Page;

    const PAGE_SIZE: usize = 4096;
    const BLOCK_SIZE: usize = 64;

    /// Checks that one page is divided into 64 aligned, non-overlapping blocks.
    ///
    /// Each block should keep its own data, and the final block should end
    /// exactly at the end of the page.
    #[test]
    fn divide_an_os_page_into_fixed_size_blocks() {
        let mut page = Page::new(PAGE_SIZE, BLOCK_SIZE)
            .expect("the operating system should provide one allocator page");
        assert_eq!(page.block_count(), 64);
        assert_eq!(page.block_ptr(page.block_count()), None);

        // SAFETY:
        // - Every block is removed from the free list before it is written.
        // - Every byte offset stays within its selected block.
        // - Each byte is initialized before it is read.
        unsafe {
            for block_index in 0..page.block_count() {
                let expected_block = page
                    .block_ptr(block_index)
                    .expect("the block index should be within the page");
                let block = page
                    .take_free_block()
                    .expect("the page should still contain a free block");
                let marker = (block_index + 1) as u8;

                assert_eq!(block, expected_block);
                assert_eq!((block.as_ptr() as usize) % page.block_size(), 0);
                for byte_offset in 0..page.block_size() {
                    block.as_ptr().add(byte_offset).write(marker);
                }
            }
            assert_eq!(page.take_free_block(), None);

            for block_index in 0..page.block_count() {
                let block = page
                    .block_ptr(block_index)
                    .expect("the block index should be within the page");
                let expected_marker = (block_index + 1) as u8;

                for byte_offset in 0..page.block_size() {
                    assert_eq!(block.as_ptr().add(byte_offset).read(), expected_marker);
                }
            }

            let first_block = page
                .block_ptr(0)
                .expect("the first block should be within the page");
            let last_block = page
                .block_ptr(page.block_count() - 1)
                .expect("the last block should be within the page");
            assert_eq!(
                last_block.as_ptr().add(page.block_size()),
                first_block.as_ptr().add(page.size())
            );
        }
    }

    /// Checks that every free block can be taken and returned.
    ///
    /// The list should become empty after all blocks are taken. Returned blocks
    /// should then be available again in last-returned, first-taken order.
    #[test]
    fn take_and_return_blocks_through_the_intrusive_free_list() {
        let mut page = Page::new(PAGE_SIZE, BLOCK_SIZE)
            .expect("the operating system should provide one allocator page");

        for block_index in 0..page.block_count() {
            assert_eq!(page.take_free_block(), page.block_ptr(block_index));
        }
        assert_eq!(page.take_free_block(), None);

        for block_index in 0..page.block_count() {
            let block = page
                .block_ptr(block_index)
                .expect("the block index should be within the page");

            // SAFETY: Every block was taken exactly once and is not currently free.
            unsafe { page.return_free_block(block) };
        }

        for block_index in (0..page.block_count()).rev() {
            assert_eq!(page.take_free_block(), page.block_ptr(block_index));
        }
        assert_eq!(page.take_free_block(), None);
    }
}

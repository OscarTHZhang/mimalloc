//! A bottom-up Rust rewrite of mimalloc v3.
//!
//! Allocator behavior will be added one small, reviewed concept at a time.
//!
//! Phase 1 divides one contiguous page into equal-size allocation blocks.

// TODO: remove this once they are used.
#![allow(dead_code)]

use std::ptr::NonNull;

use rustix::{
    io::Errno,
    mm::{MapFlags, MprotectFlags, ProtFlags, mmap_anonymous, mprotect, munmap},
};

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
}

impl Page {
    /// Maps a page when its size divides evenly into blocks.
    fn new(page_size: usize, block_size: usize) -> rustix::io::Result<Self> {
        if page_size == 0 || block_size == 0 || !page_size.is_multiple_of(block_size) {
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

        Ok(Self {
            start,
            size: page_size,
            block_size,
            block_count: page_size / block_size,
        })
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

    #[test]
    fn divide_an_os_page_into_fixed_size_blocks() {
        let page = Page::new(PAGE_SIZE, BLOCK_SIZE)
            .expect("the operating system should provide one allocator page");
        assert_eq!(page.block_count(), 64);
        assert_eq!(page.block_ptr(page.block_count()), None);

        // SAFETY:
        // - Every block pointer comes from the live `page`.
        // - Every byte offset stays within its selected block.
        // - Each byte is initialized before it is read.
        unsafe {
            for block_index in 0..page.block_count() {
                let block = page
                    .block_ptr(block_index)
                    .expect("the block index should be within the page");
                let marker = (block_index + 1) as u8;

                assert_eq!((block.as_ptr() as usize) % page.block_size(), 0);
                for byte_offset in 0..page.block_size() {
                    block.as_ptr().add(byte_offset).write(marker);
                }
            }

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
}

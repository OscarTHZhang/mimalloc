//! A bottom-up Rust rewrite of mimalloc v3.
//!
//! Allocator behavior will be added one small, reviewed concept at a time.

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap_anonymous, mprotect, munmap};

    #[test]
    fn reserve_commit_and_release_an_os_memory_page() {
        const PAGE_SIZE: usize = 4096;

        // Reserve an anonymous virtual-memory page without making it
        // accessible yet. This does not call Rust's global allocator.
        let mapping = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                PAGE_SIZE,
                ProtFlags::empty(),
                MapFlags::PRIVATE,
            )
        }
        .expect("the operating system should reserve one page");

        let block = NonNull::new(mapping.cast::<u8>())
            .expect("a successful memory mapping must not be null");

        // SAFETY:
        // - `mapping` identifies the complete live mapping.
        // - The mapping is page-aligned and `PAGE_SIZE` bytes long.
        // - No Rust reference points into the mapping.
        unsafe {
            mprotect(
                mapping,
                PAGE_SIZE,
                MprotectFlags::READ | MprotectFlags::WRITE,
            )
            .expect("the reserved page should become accessible");
        }

        // SAFETY:
        // - `block` points to the accessible `PAGE_SIZE`-byte mapping.
        // - Every pointer offset stays within that mapping.
        // - Each byte is initialized before it is read.
        unsafe {
            for offset in 0..PAGE_SIZE {
                block.as_ptr().add(offset).write(offset as u8);
            }

            for offset in 0..PAGE_SIZE {
                assert_eq!(block.as_ptr().add(offset).read(), offset as u8);
            }

            munmap(mapping, PAGE_SIZE).expect("the mapping should be released");
        }
    }
}

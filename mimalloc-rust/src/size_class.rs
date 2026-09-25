//! Chooses a block size and page kind for each allocation request.
//!
//! ```text
//! requested bytes -> class index -> block size -> page kind
//!
//! Small:      blocks through 10 KiB  -> 64 KiB page
//! Medium:     blocks through 80 KiB  -> 512 KiB page
//! Large:      blocks through 512 KiB -> 4 MiB page
//! Singleton:  larger blocks         -> one block per page
//! ```

// TODO: remove this once size classes are used by the allocator.
#![allow(dead_code)]

/// Number of bytes in a machine word on the current target.
const WORD_SIZE: usize = size_of::<usize>();
// The initial allocator supports only 64-bit targets.
const _: () = assert!(WORD_SIZE == 8);
/// Number of bytes in an operating-system page.
const OS_PAGE_SIZE: usize = 4 * 1024;
/// Number of bytes reserved for a small page.
const SMALL_PAGE_SIZE: usize = 64 * 1024;
/// Number of bytes reserved for a medium page.
const MEDIUM_PAGE_SIZE: usize = 512 * 1024;
/// Number of bytes reserved for a large page.
const LARGE_PAGE_SIZE: usize = 4 * 1024 * 1024;
/// Largest block size, in bytes, stored in a small page.
const SMALL_MAX_BLOCK_SIZE: usize = (SMALL_PAGE_SIZE - OS_PAGE_SIZE) / 6;
/// Largest block size, in bytes, stored in a medium page.
const MEDIUM_MAX_BLOCK_SIZE: usize = (MEDIUM_PAGE_SIZE - OS_PAGE_SIZE) / 6;
/// Largest block size, in bytes, stored in a large page.
const LARGE_MAX_BLOCK_SIZE: usize = LARGE_PAGE_SIZE / 8;
/// Largest regular block size measured in machine words.
const LARGE_MAX_WORD_COUNT: usize = LARGE_MAX_BLOCK_SIZE / WORD_SIZE;

/// The kind of page used to store a size class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageKind {
    /// A 64 KiB page for small blocks.
    Small,
    /// A 512 KiB page for medium blocks.
    Medium,
    /// A 4 MiB page for large blocks.
    Large,
    /// A page containing one oversized block.
    Singleton,
}

/// Describes how the allocator handles one requested size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SizeClass {
    /// Position in the regular page queue table, or `None` for a singleton.
    class_index: Option<usize>,
    /// Number of bytes reserved for each allocation.
    block_size: usize,
    /// Kind of page that stores blocks of this size.
    page_kind: PageKind,
}

impl SizeClass {
    /// Chooses an allocation class for a byte request.
    fn for_request(request_size: usize) -> Option<Self> {
        if request_size > isize::MAX as usize {
            return None;
        }

        let word_count =
            request_size / WORD_SIZE + usize::from(!request_size.is_multiple_of(WORD_SIZE));
        let class_index = class_index_for_word_count(word_count);

        let Some(class_index) = class_index else {
            let block_size = align_up(request_size, OS_PAGE_SIZE)?;
            return Some(Self {
                class_index: None,
                block_size,
                page_kind: PageKind::Singleton,
            });
        };

        let block_size = words_for_class_index(class_index) * WORD_SIZE;
        Some(Self {
            class_index: Some(class_index),
            block_size,
            page_kind: page_kind_for_block_size(block_size),
        })
    }
}

/// Returns the regular class index for a size measured in machine words.
fn class_index_for_word_count(word_count: usize) -> Option<usize> {
    if word_count <= 8 {
        return Some(if word_count <= 1 {
            1
        } else {
            (word_count + 1) & !1
        });
    }
    if word_count > LARGE_MAX_WORD_COUNT {
        return None;
    }
    // Subtract one to keep powers of two in the lower range. The highest set bit
    // selects the range; the next two bits select its quarter.
    let range_value = word_count - 1;
    let range_exponent = usize::BITS as usize - 1 - range_value.leading_zeros() as usize;
    let quarter = (range_value >> (range_exponent - 2)) & 0x03;
    Some((range_exponent << 2) + quarter - 3)
}

/// Decodes a class index into its rounded block size in machine words.
///
/// Indices above eight encode a power-of-two range and one of four quarters.
fn words_for_class_index(class_index: usize) -> usize {
    if class_index <= 8 {
        return class_index.max(1);
    }
    // Undo the index offset; the high bits hold the range and the low two its quarter.
    let encoded_range = class_index + 3;
    let range_exponent = encoded_range >> 2;
    let quarter = encoded_range & 0x03;
    // Quarter endpoints are 5/4, 6/4, 7/4, and 8/4 of the range's starting size.
    (5 + quarter) << (range_exponent - 2)
}

/// Selects the page kind from the rounded block size.
fn page_kind_for_block_size(block_size: usize) -> PageKind {
    if block_size <= SMALL_MAX_BLOCK_SIZE {
        PageKind::Small
    } else if block_size <= MEDIUM_MAX_BLOCK_SIZE {
        PageKind::Medium
    } else if block_size <= LARGE_MAX_BLOCK_SIZE {
        PageKind::Large
    } else {
        PageKind::Singleton
    }
}

/// Rounds a size upward to an alignment without overflowing.
fn align_up(size: usize, alignment: usize) -> Option<usize> {
    size.checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::{PageKind, SizeClass};

    /// Checks representative size classes on Linux x86-64.
    ///
    /// Each request should round up to the expected aligned block size.
    #[test]
    fn maps_requests_to_expected_block_sizes() {
        let examples = [
            (0, 1, 8),
            (8, 1, 8),
            (9, 2, 16),
            (17, 4, 32),
            (50, 8, 64),
            (65, 9, 80),
            (129, 13, 160),
            (1025, 25, 1280),
        ];

        for (request_size, expected_class_index, expected_block_size) in examples {
            let size_class =
                SizeClass::for_request(request_size).expect("the request should be supported");
            assert_eq!(size_class.class_index, Some(expected_class_index));
            assert_eq!(size_class.block_size, expected_block_size);
        }
    }

    /// Checks where the allocator changes between page kinds.
    ///
    /// Requests should use small, medium, and large pages until they exceed
    /// 512 KiB, where they should use a page containing one block.
    #[test]
    fn selects_page_kinds_at_block_size_boundaries() {
        let examples = [
            (10 * 1024, 10 * 1024, PageKind::Small),
            (10 * 1024 + 1, 12 * 1024, PageKind::Medium),
            (80 * 1024, 80 * 1024, PageKind::Medium),
            (80 * 1024 + 1, 96 * 1024, PageKind::Large),
            (512 * 1024, 512 * 1024, PageKind::Large),
            (512 * 1024 + 1, 516 * 1024, PageKind::Singleton),
        ];

        for (request_size, expected_block_size, expected_page_kind) in examples {
            let size_class =
                SizeClass::for_request(request_size).expect("the request should be supported");
            assert_eq!(size_class.block_size, expected_block_size);
            assert_eq!(size_class.page_kind, expected_page_kind);
        }

        let singleton =
            SizeClass::for_request(512 * 1024 + 1).expect("the request should be supported");
        assert_eq!(singleton.class_index, None);
        assert_eq!(SizeClass::for_request(usize::MAX), None);
    }
}

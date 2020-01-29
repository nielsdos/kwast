use core::mem::size_of;
use crate::arch::x86_64::paging::{ActiveMapping, EntryFlags};
use crate::mm::mapper::MemoryMapper;
use crate::arch::x86_64::address::VirtAddr;
use core::cmp;

/// Amount of top nodes.
const MAX_LEVEL: usize = 15;

/// Amount of nodes.
const NODE_COUNT: usize = 1 << MAX_LEVEL - 1;

// Amount of bytes needed.
const NODE_BYTES_NEEDED: usize = NODE_COUNT;

/// The buddy tree.
struct Tree {
    /// Entries in the tree.
    nodes: [u8; NODE_BYTES_NEEDED],
}

impl Tree {
    /// Initializes the tree.
    pub fn init(&mut self) {
        // Is power of 2? We don't care about the case of x == 0 here.
        fn is_pow2(x: usize) -> bool {
            x & (x - 1) == 0
        }

        let mut size = (MAX_LEVEL + 1) as u8;
        for i in 0..NODE_COUNT {
            if is_pow2(i + 1) {
                size -= 1;
            }

            self.nodes[i] = size;
        }
    }

    /// Left index of a node.
    #[inline]
    fn left_index(&self, index: usize) -> usize {
        (index << 1) | 1
    }

    /// Right index of a node.
    #[inline]
    fn right_index(&self, index: usize) -> usize {
        self.left_index(index) + 1
    }

    /// Parent index of a node.
    #[inline]
    fn parent_index(&self, index: usize) -> usize {
        ((index + 1) >> 1) - 1
    }

    /// Allocate in tree.
    pub fn alloc(&mut self, size: usize) -> Option<usize> {
        if unlikely!(self.nodes[0] < size as u8) {
            return None;
        }

        // Find node with smallest size large enough to hold the requested size
        let wanted_level = MAX_LEVEL - size as usize;
        let mut index = 0;
        for level in 0..wanted_level {
            let left_index = self.left_index(index);
            let right_index = self.right_index(index);

            // Because of the check at the beginning, we know one of these two is big enough
            index = if self.nodes[left_index] >= size as u8 {
                left_index
            } else {
                right_index
            };
        }

        // Calculate offset from the index
        let first_index_in_this_level = (1 << wanted_level) - 1;
        let index_in_this_level = index - first_index_in_this_level;
        let offset = index_in_this_level << size;

        // Update the values in the tree so that each node still contains the largest available
        // power of two size in their subtree.
        self.nodes[index] = 0;
        while index > 0 {
            index = self.parent_index(index);
            let left_index = self.left_index(index);
            let right_index = self.right_index(index);
            let max = cmp::max(self.nodes[left_index], self.nodes[right_index]);
            self.nodes[index] = max;
        }

        Some(offset)
    }
}

#[allow(improper_ctypes)]
extern "C" {
    #[link_name = "llvm.x86.rdtscp"]
    fn rdtscp(aux: *mut u8) -> u64;
}

// TODO
pub fn test() {
    unsafe {
        let mut xx: u8 = 0;

        println!("size: {:?}", size_of::<Tree>());

        let mut mapping = ActiveMapping::get();
        mapping.map_range(VirtAddr::new(0xFC00000), size_of::<Tree>(), EntryFlags::PRESENT | EntryFlags::WRITABLE).unwrap();
        let tree = &mut *(0xFC00000 as *mut Tree);
        tree.init();

        let a = rdtscp(&mut xx);

        assert_eq!(tree.alloc(3), Some(0));
        assert_eq!(tree.alloc(2), Some(8));
        assert_eq!(tree.alloc(3), Some(16));
        assert_eq!(tree.alloc(4), Some(32));
        assert_eq!(tree.alloc(2), Some(12));
        assert_eq!(tree.alloc(3), Some(24));
        assert_eq!(tree.alloc(6), Some(64));
        assert_eq!(tree.alloc(MAX_LEVEL), None);

        let b = rdtscp(&mut xx);

        println!("{}ns", ((b - a) * 284) >> 10);
    }
}

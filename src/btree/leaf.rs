use std::mem::size_of;
use zerocopy::{AsBytes, ByteSlice, ByteSliceMut, FromBytes, LayoutVerified};
use crate::btree::Pair;
use crate::disk::PageId;

use crate::slotted::{self, Slotted};

#[derive(Debug, FromBytes, AsBytes)]
#[repr(C)]
pub struct Header {
    prev_page_id: PageId,
    next_page_id: PageId,
}

pub struct Leaf<B> {
    header: LayoutVerified<B, Header>,
    body: Slotted<B>
}

impl<B: ByteSlice> Leaf<B> {
    pub fn new(bytes: B) -> Self {
        let (header, body) =
            LayoutVerified::new_from_prefix(bytes).expect("leaf header must be aligned");
        let body = Slotted::new(body);
        Self{ header, body }
    }

    pub fn prev_page_id(&self) -> Option<PageId> { self.header.prev_page_id.valid() }

    pub fn next_page_id(&self) -> Option<PageId> { self.header.next_page_id.valid() }

    pub fn num_pairs(&self) -> usize { self.body.num_slots() }

    pub fn search_slot_id(&self, key: &[u8]) -> Result<usize, usize> {
        binary_search_by(self.num_pairs(), |slot_id| {
            self.pair_at(slot_id).key.cmp(key)
        })
    }

    #[cfg(test)]
    pub fn search_pair(&self, key: &[u8]) -> Option<Pair> {
        let slot_id = self.search_slot_id(key).ok()?;
        Some(self.pair_at(slot_id))
    }

    pub fn pair_at(&self, slot_id: usize) -> Pair { Pair::from_bytes(&self.body[slot_id]) }

    pub fn max_pair_size(&self) -> usize {
        self.body.capacity() / 2 - size_of::<slotted::Pointer>()
    }
}

impl<B: ByteSliceMut> Leaf<B> {
    pub fn initialize(&mut self) {
        self.header.prev_page_id = PageId::INVALID_PAGE_ID;
        self.header.next_page_id = PageId::INVALID_PAGE_ID;
        self.body.initialize();
    }

    pub fn set_prev_page_id(&mut self, prev_page_id: Option<PageId>) {
        self.header.prev_page_id = prev_page_id.into()
    }

    pub fn set_next_page_id(&mut self, next_page_id: Option<PageId>) {
        self.header.next_page_id = next_page_id.into()
    }

    #[must_use = "insertion may fail"]
    pub fn insert(&mut self, slot_id: usize, key: &[u8], value: &[u8]) -> Option<()> {
        let pair = Pair { key, value };
        let pair_bytes = pair.to_bytes();
        assert!(pair_bytes.len() <= self.max_pair_size());
        self.body.insert(slot_id, pair_bytes.len())?;
        self.body[slot_id].copy_from_slice(&pair_bytes);
        Some(())
    }

    fn is_half_full(&self) -> bool { 2 * self.body.free_space() < self.body.capacity() }

    pub fn split_insert(
        &mut self,
        new_leaf: &mut Leaf<impl ByteSliceMut>,
        new_key: &[u8],
        new_value: &[u8],
    ) -> Vec<u8> {
        new_leaf.initialize();
        loop {
            if new_leaf.is_half_full() {
                let index = self
                    .search_slot_id(new_key)
                    .expect_err("key must be unique");
                self.insert(index, new_key, new_value)
                    .expect("old leaf must have space");
                break;
            }
            if self.pair_at(0).key < new_key {
                self.transfer(new_leaf);
            } else {
                new_leaf
                    .insert(new_leaf.num_pairs(), new_key, new_value)
                    .expect("new leaf must have space");
                while !new_leaf.is_half_full() {
                    self.transfer(new_leaf);
                }
                break;
            }
        }
        self.pair_at(0).key.to_vec()
    }

    pub fn transfer(&mut self, dest: &mut Leaf<impl ByteSliceMut>) {
        let next_index = dest.num_pairs();
        assert!(dest.body.insert(next_index, self.body[0].len()).is_some());
        dest.body[next_index].copy_from_slice(&self.body[0]);
        self.body.remove(0);
    }
}
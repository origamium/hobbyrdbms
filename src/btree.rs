mod leaf;
mod node;
mod branch;
mod meta;

use std::cell::{Ref, RefMut};
use std::convert::identity;
use std::rc::Rc;
use bincode::Options;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zerocopy::{AsBytes, ByteSlice};
use crate::buffer;
use crate::buffer::{Buffer, BufferPoolManager};
use crate::disk::PageId;

#[derive(Serialize, Deserialize)]
pub struct Pair<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

impl<'a> Pair<'a> {
    fn to_bytes(&self) -> Vec<u8> { bincode::options().serialize(self).unwrap()}
    fn from_bytes(bytes: &'a [u8]) -> Self {bincode::options().deserialize(bytes).unwrap()}
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("duplicate key")]
    DuplicateKey,
    #[error(transparent)]
    Buffer(#[from] buffer::Error),
}

#[derive(Debug, Clone)]
pub enum SearchMode {
    Start,
    Key(Vec<u8>),
}

impl SearchMode {
    fn child_page_id(&self, branch: &branch::Branch<impl ByteSlice>) -> PageId {
        match self {
            SearchMode::Start => branch.child_at(0),
            SearchMode::Key(key) => branch.search_child(key),
        }
    }

    fn tuple_slot_id(&self, leaf: &leaf::Leaf<impl ByteSlice>) -> Result<usize, usize> {
        match self {
            SearchMode::Start => Err(0),
            SearchMode::Key(key) => leaf.search_slot_id(key),
        }
    }
}

pub struct BTree {
    pub meta_page_id: PageId,
}

impl BTree {
    pub fn create(bufmgr: &mut BufferPoolManager) -> Result<Self, Error> {
        let meta_buffer = bufmgr.create_page()?;
        let mut meta = meta::Meta::new(meta_buffer.page.borrow_mut() as RefMut<_>);
        let root_buffer = bufmgr.create_page()?;
        let mut root = node::Node::new(root_buffer.page.borrow_mut() as RefMut<_>);
        root.initialize_as_leaf();
        let mut leaf = leaf::Leaf::new(root.body);
        leaf.initialize();
        meta.header.root_page_id = root_buffer.page_id;
        Ok(Self::new(meta_buffer.page_id))
    }

    pub fn new(meta_page_id: PageId) -> Self { Self { meta_page_id }}

    fn fetch_root_page(&self, bufmgr: &mut BufferPoolManager) -> Result<Rc<Buffer>, Error> {
        let root_page_id = {
            let meta_buffer = bufmgr.fetch_page(self.meta_page_id)?;
            let meta = meta::Meta::new(meta_buffer.page.borrow() as Ref<[_]>);
            meta.header.root_page_id
        };
        Ok(bufmgr.fetch_page(root_page_id)?)
    }

    fn search_internal(&self, bufmgr: &mut BufferPoolManager, node_buffer: Rc<Buffer>, search_mode: SearchMode) -> Result<Iter, Error> {
        let node = node::Node::new(node_buffer.page.borrow() as Ref<[_]>);
        match node::Body::new(node.header.node_type, node.body.as_bytes()) {
            node::Body::Leaf(leaf) => {
                let slot_id = search_mode.tuple_slot_id(&leaf).unwrap_or_else(identity);
                let is_right_most = leaf.num_pairs() == slot_id;
                drop(node);

                let mut iter = Iter {
                    buffer: node_buffer,
                    slot_id
                };
                if is_right_most {
                    iter.advance(bufmgr)?;
                }
                Ok(iter)
            }
            node::Body::Branch(branch) => {
                let child_page_id = search_mode.child_page_id(&branch);
                drop(node);
                drop(node_buffer);
                let child_node_page = bufmgr.fetch_page(child_page_id)?;
                self.search_internal(bufmgr, child_node_page, search_mode)
            }
        }
    }

    fn insert_internal(
        &self,
        bufmgr: &mut BufferPoolManager,
        buffer: Rc<Buffer>,
        key: &[u8],
        value: &[u8]
    ) -> Result<Option<(Vec<u8>, PageId)>, Error> {
        let node = node::Node::new(buffer.page.borrow_mut() as RefMut<[_]>);
        match node::Body::new(node.header.node_type, node.body) {
            node::Body::Leaf(mut leaf) => {
                let slot_id = match leaf.search_slot_id(key) {
                    Ok(_) => return Err(Error::DuplicateKey),
                    Err(slot_id) => slot_id,
                };
                if leaf.insert(slot_id, key, value).is_some() {
                    buffer.is_dirty.set(true);
                    Ok(None)
                } else {
                    let prev_leaf_page_id = leaf.prev_page_id();
                    let prev_leaf_buffer = prev_leaf_page_id
                        .map(|next_leaf_page_id| bufmgr.fetch_page(next_leaf_page_id))
                        .transpose()?;

                    let new_leaf_buffer = bufmgr.create_page()?;

                    if let Some(prev_leaf_buffer) = prev_leaf_buffer {
                        let node = node::Node::new(prev_leaf_buffer.page.borrow_mut() as RefMut<[_]>);
                        let mut prev_leaf = leaf::Leaf::new(node.body);
                        prev_leaf.set_next_page_id(Some(new_leaf_buffer.page_id));
                        prev_leaf_buffer.is_dirty.set(true);
                    }
                    leaf.set_prev_page_id(Some(new_leaf_buffer.page_id));

                    let mut new_leaf_node = node::Node::new(new_leaf_buffer.page.borrow_mut() as RefMut<[_]>);
                    new_leaf_node.initialize_as_leaf();
                    let mut new_leaf = leaf::Leaf::new(new_leaf_node.body);
                    new_leaf.initialize();
                    let overflow_key = leaf.split_insert(&mut new_leaf, key, value);
                    new_leaf.set_next_page_id(Some(buffer.page_id));
                    new_leaf.set_prev_page_id(prev_leaf_page_id);
                    buffer.is_dirty.set(true);
                    Ok(Some((overflow_key, new_leaf_buffer.page_id)))
                }
            }
            node::Body::Branch(mut branch) => {
                let child_idx = branch.search_child_idx(key);
                let child_page_id = branch.child_at(child_idx);
                let child_node_buffer = bufmgr.fetch_page(child_page_id)?;
                if let Some((overflow_key_from_child, overflow_child_page_id)) =
                    self.insert_internal(bufmgr, child_node_buffer, key, value)?
                {
                    if branch
                        .insert(child_idx, &overflow_key_from_child, overflow_child_page_id)
                        .is_some() {
                        buffer.is_dirty.set(true);
                        Ok(None)
                    } else {
                        let new_branch_buffer = bufmgr.create_page()?;
                        let mut new_branch_node =
                            node::Node::new(new_branch_buffer.page.borrow_mut() as RefMut<[_]>);
                        new_branch_node.initialize_as_branch();
                        let mut new_branch = branch::Branch::new(new_branch_node.body);
                        let overflow_key = branch.split_insert(
                            &mut new_branch,
                            &overflow_key_from_child,
                            overflow_child_page_id,
                        );
                        buffer.is_dirty.set(true);
                        new_branch_buffer.is_dirty.set(true);
                        Ok(Some((overflow_key, new_branch_buffer.page_id)))
                    }
                } else {
                    Ok(None)
                }
            }
        }
    }
}

pub struct Iter {
    buffer: Rc<Buffer>,
    slot_id: usize,
}

impl Iter {
    fn get(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let leaf_node = node::Node::new(self.buffer.page.borrow() as Ref<[_]>);
        let leaf = leaf::Leaf::new(leaf_node.body);
        if self.slot_id < leaf.num_pairs() {
            let pair = leaf.pair_at(self.slot_id);
            Some((pair.key.to_vec(), pair.value.to_vec()))
        } else {
            None
        }
    }

    fn advance(&mut self, bufmgr: &mut BufferPoolManager) -> Result<(), Error> {
        self.slot_id += 1;
        let next_page_id = {
            let leaf_node = node::Node::new(self.buffer.page.borrow() as Ref<[_]>);
            let leaf = leaf::Leaf::new(leaf_node.body);
            if self.slot_id < leaf.num_pairs() {
                return Ok(())
            }
            leaf.next_page_id()
        };
        if let Some(next_page_id) = next_page_id {
            self.buffer = bufmgr.fetch_page(next_page_id)?;
            self.slot_id = 0;
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn next(&mut self, bufmgr: &mut BufferPoolManager) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        let value = self.get();
        self.advance(bufmgr);
        Ok(value)
    }
}
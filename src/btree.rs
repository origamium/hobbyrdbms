mod leaf;

use bincode::Options;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::buffer;

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
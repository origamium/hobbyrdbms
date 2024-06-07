use crate::disk::PageId;

#[derive(Debug)]
pub struct Header {
    prev_page_id: PageId,
    next_page_id: PageId,
}
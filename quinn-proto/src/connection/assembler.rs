use std::{
    cmp::Ordering,
    collections::{binary_heap::PeekMut, BinaryHeap},
    mem,
};

use bytes::{Buf, Bytes, BytesMut};

use crate::range_set::RangeSet;

/// Helper to assemble unordered stream frames into an ordered stream
#[derive(Debug, Default)]
pub(crate) struct Assembler {
    state: State,
    data: BinaryHeap<Chunk>,
    defragmented: usize,
    /// Number of bytes read by the application. When only ordered reads have been used, this is the
    /// length of the contiguous prefix of the stream which has been consumed by the application,
    /// aka the stream offset.
    bytes_read: u64,
    /// Whether to discard data
    stopped: bool,
    /// First offset we haven't received any data at or after
    end: u64,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read_unordered(&mut self) -> Option<(u64, Bytes)> {
        if let State::Ordered = self.state {
            // Enter unordered mode
            let mut recvd = RangeSet::new();
            recvd.insert(0..self.bytes_read);
            for chunk in &self.data {
                recvd.insert(chunk.offset..chunk.offset + chunk.bytes.len() as u64);
            }
            self.state = State::Unordered { recvd };
        }
        let (n, data) = self.pop()?;
        self.bytes_read += data.len() as u64;
        Some((n, data))
    }

    // Get the the next ordered chunk
    pub(crate) fn read(&mut self, max_length: usize) -> Result<Option<Bytes>, IllegalOrderedRead> {
        if let State::Unordered { .. } = self.state {
            return Err(IllegalOrderedRead);
        }

        loop {
            let mut chunk = match self.data.peek_mut() {
                Some(chunk) => chunk,
                None => return Ok(None),
            };

            if chunk.offset > self.bytes_read {
                // Next chunk is after current read index
                return Ok(None);
            } else if (chunk.offset + chunk.bytes.len() as u64) <= self.bytes_read {
                // Next chunk is useless as the read index is beyond its end
                PeekMut::pop(chunk);
                continue;
            }

            // Determine `start` and `len` of the slice of useful data in chunk
            let start = (self.bytes_read - chunk.offset) as usize;
            if start > 0 {
                chunk.bytes.advance(start);
                chunk.offset += start as u64;
            }

            return Ok(Some(if max_length < chunk.bytes.len() {
                self.bytes_read += max_length as u64;
                chunk.offset += max_length as u64;
                chunk.bytes.split_to(max_length)
            } else {
                self.bytes_read += chunk.bytes.len() as u64;
                self.defragmented = self.defragmented.saturating_sub(1);
                PeekMut::pop(chunk).bytes
            }));
        }
    }

    // Copy the buffered chunk data to new chunks backed by a single buffer to
    // make sure we're not unnecessarily holding on to many larger allocations.
    // Merge contiguous chunks in the process of doing so. Reset the `defragmented`
    // counter to the new number of chunks left in the heap so that we can decide
    // when to defragment the queue again if necessary.
    fn defragment(&mut self) {
        let buffered = self.data.iter().map(|c| c.bytes.len()).sum::<usize>();
        let mut buffer = BytesMut::with_capacity(buffered);
        let mut offset = self
            .data
            .peek()
            .as_ref()
            .expect("defragment is only called when data is buffered")
            .offset;

        let new = BinaryHeap::with_capacity(self.data.len());
        let old = mem::replace(&mut self.data, new);
        for chunk in old.into_sorted_vec().into_iter().rev() {
            let end = offset + (buffer.len() as u64);
            if let Some(overlap) = end.checked_sub(chunk.offset) {
                if let Some(bytes) = chunk.bytes.get(overlap as usize..) {
                    buffer.extend_from_slice(bytes);
                }
            } else {
                let bytes = buffer.split().freeze();
                self.data.push(Chunk { offset, bytes });
                offset = chunk.offset;
                buffer.extend_from_slice(&chunk.bytes);
            }
        }

        let bytes = buffer.split().freeze();
        self.data.push(Chunk { offset, bytes });
        self.defragmented = self.data.len();
    }

    fn pop(&mut self) -> Option<(u64, Bytes)> {
        self.defragmented = self.defragmented.saturating_sub(1);
        self.data.pop().map(|x| (x.offset, x.bytes))
    }

    pub(crate) fn insert(&mut self, mut offset: u64, mut bytes: Bytes) {
        self.end = self.end.max(offset + bytes.len() as u64);

        if let State::Unordered { ref mut recvd } = self.state {
            // Discard duplicate data
            for duplicate in recvd.replace(offset..offset + bytes.len() as u64) {
                if duplicate.start > offset {
                    self.data.push(Chunk {
                        offset,
                        bytes: bytes.split_to((duplicate.start - offset) as usize),
                    });
                    offset = duplicate.start;
                }
                bytes.advance((duplicate.end - offset) as usize);
                offset = duplicate.end;
            }
        }
        if bytes.is_empty() || self.stopped {
            return;
        }
        self.data.push(Chunk { offset, bytes });
        // Why 32: on the one hand, we want to defragment rarely, ideally never
        // in non-pathological scenarios. However, a pathological or malicious
        // peer could send us one-byte frames, and since we use reference-counted
        // buffers in order to prevent copying, this could result in keeping a lot
        // of memory allocated. In the worst case scenario of 32 1-byte chunks,
        // each one from a ~1000-byte datagram, using 32 limits us to having a
        // maximum pathological over-allocation of about 32k bytes.
        if self.data.len() - self.defragmented > 32 {
            self.defragment()
        }
    }

    /// Number of bytes consumed by the application
    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Offset after the largest byte received
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Whether all data prior to `self.end()` has been read
    pub(crate) fn is_fully_read(&self) -> bool {
        self.bytes_read == self.end
    }

    /// Discard all buffered data
    pub(crate) fn clear(&mut self) {
        self.data.clear();
        self.defragmented = 0;
    }

    /// Discard buffered data and do not buffer future data, but continue tracking offsets.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        self.data.clear();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped
    }
}

#[derive(Debug, Eq)]
struct Chunk {
    offset: u64,
    bytes: Bytes,
}

impl Ord for Chunk {
    // Invert ordering based on offset (max-heap, min offset first),
    // prioritize longer chunks at the same offset.
    fn cmp(&self, other: &Chunk) -> Ordering {
        self.offset
            .cmp(&other.offset)
            .reverse()
            .then(self.bytes.len().cmp(&other.bytes.len()))
    }
}

impl PartialOrd for Chunk {
    fn partial_cmp(&self, other: &Chunk) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Chunk {
    fn eq(&self, other: &Chunk) -> bool {
        (self.offset, self.bytes.len()) == (other.offset, other.bytes.len())
    }
}

#[derive(Debug)]
enum State {
    Ordered,
    Unordered {
        /// The set of offsets that have been received from the peer, including portions not yet
        /// read by the application.
        recvd: RangeSet,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Ordered
    }
}

/// Error indicating that an ordered read was performed on a stream after an unordered read
#[derive(Debug, Copy, Clone)]
pub struct IllegalOrderedRead;

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn assemble_ordered() {
        let mut x = Assembler::new();
        assert_matches!(next(&mut x, 32), None);
        x.insert(0, Bytes::from_static(b"123"));
        assert_matches!(next(&mut x, 1), Some(ref y) if &y[..] == b"1");
        assert_matches!(next(&mut x, 3), Some(ref y) if &y[..] == b"23");
        x.insert(3, Bytes::from_static(b"456"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"456");
        x.insert(6, Bytes::from_static(b"789"));
        x.insert(9, Bytes::from_static(b"10"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"789");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"10");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_unordered() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"456"));
        assert_matches!(next(&mut x, 32), None);
        x.insert(0, Bytes::from_static(b"123"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_duplicate() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"));
        x.insert(0, Bytes::from_static(b"123"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_duplicate_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"));
        x.insert(0, Bytes::from_static(b"123"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contained() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"12345"));
        x.insert(1, Bytes::from_static(b"234"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contained_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"12345"));
        x.insert(1, Bytes::from_static(b"234"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contains() {
        let mut x = Assembler::new();
        x.insert(1, Bytes::from_static(b"234"));
        x.insert(0, Bytes::from_static(b"12345"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contains_compact() {
        let mut x = Assembler::new();
        x.insert(1, Bytes::from_static(b"234"));
        x.insert(0, Bytes::from_static(b"12345"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_overlapping() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"));
        x.insert(1, Bytes::from_static(b"234"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"4");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_overlapping_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"));
        x.insert(1, Bytes::from_static(b"234"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"1234");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_complex() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1"));
        x.insert(2, Bytes::from_static(b"3"));
        x.insert(4, Bytes::from_static(b"5"));
        x.insert(0, Bytes::from_static(b"123456"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_complex_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1"));
        x.insert(2, Bytes::from_static(b"3"));
        x.insert(4, Bytes::from_static(b"5"));
        x.insert(0, Bytes::from_static(b"123456"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_old() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1234"));
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"1234");
        x.insert(0, Bytes::from_static(b"1234"));
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_old_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1234"));
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"1234");
        x.insert(0, Bytes::from_static(b"1234"));
        x.defragment();
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"abc"));
        x.insert(3, Bytes::from_static(b"def"));
        x.insert(9, Bytes::from_static(b"jkl"));
        x.insert(12, Bytes::from_static(b"mno"));
        x.defragment();
        assert_eq!(x.pop(), Some((0, Bytes::from_static(b"abcdef"))));
        assert_eq!(x.pop(), Some((9, Bytes::from_static(b"jklmno"))));
    }

    #[test]
    fn defrag_with_missing_prefix() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"));
        x.defragment();
        assert_eq!(x.pop(), Some((3, Bytes::from_static(b"def"))));
    }

    #[test]
    fn defrag_read_chunk() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"));
        x.insert(0, Bytes::from_static(b"abc"));
        x.insert(7, Bytes::from_static(b"hij"));
        x.insert(11, Bytes::from_static(b"lmn"));
        x.defragment();
        assert_matches!(x.read(usize::MAX), Ok(Some(ref y)) if &y[..] == b"abcdef");
        x.insert(5, Bytes::from_static(b"fghijklmn"));
        assert_matches!(x.read(usize::MAX), Ok(Some(ref y)) if &y[..] == b"ghijklmn");
        x.insert(13, Bytes::from_static(b"nopq"));
        assert_matches!(x.read(usize::MAX), Ok(Some(ref y)) if &y[..] == b"opq");
        x.insert(15, Bytes::from_static(b"pqrs"));
        assert_matches!(x.read(usize::MAX), Ok(Some(ref y)) if &y[..] == b"rs");
        assert_matches!(x.read(usize::MAX), Ok(None));
    }

    #[test]
    fn unordered_happy_path() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"abc"));
        assert_eq!(x.read_unordered(), Some((0, Bytes::from_static(b"abc"))));
        assert_eq!(x.read_unordered(), None);
        x.insert(3, Bytes::from_static(b"def"));
        assert_eq!(x.read_unordered(), Some((3, Bytes::from_static(b"def"))));
        assert_eq!(x.read_unordered(), None);
    }

    #[test]
    fn unordered_dedup() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"));
        assert_eq!(x.read_unordered(), Some((3, Bytes::from_static(b"def"))));
        assert_eq!(x.read_unordered(), None);
        x.insert(0, Bytes::from_static(b"a"));
        x.insert(0, Bytes::from_static(b"abcdefghi"));
        x.insert(0, Bytes::from_static(b"abcd"));
        assert_eq!(x.read_unordered(), Some((0, Bytes::from_static(b"a"))));
        assert_eq!(x.read_unordered(), Some((1, Bytes::from_static(b"bc"))));
        assert_eq!(x.read_unordered(), Some((6, Bytes::from_static(b"ghi"))));
        assert_eq!(x.read_unordered(), None);
        x.insert(8, Bytes::from_static(b"ijkl"));
        assert_eq!(x.read_unordered(), Some((9, Bytes::from_static(b"jkl"))));
        assert_eq!(x.read_unordered(), None);
        x.insert(12, Bytes::from_static(b"mno"));
        assert_eq!(x.read_unordered(), Some((12, Bytes::from_static(b"mno"))));
        assert_eq!(x.read_unordered(), None);
        x.insert(2, Bytes::from_static(b"cde"));
        assert_eq!(x.read_unordered(), None);
    }

    #[test]
    fn chunks_dedup() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"));
        assert_eq!(x.read(usize::MAX).unwrap(), None);
        x.insert(0, Bytes::from_static(b"a"));
        x.insert(1, Bytes::from_static(b"bcdefghi"));
        x.insert(0, Bytes::from_static(b"abcd"));
        assert_eq!(
            x.read(usize::MAX).unwrap(),
            Some(Bytes::from_static(b"abcd"))
        );
        assert_eq!(
            x.read(usize::MAX).unwrap(),
            Some(Bytes::from_static(b"efghi"))
        );
        assert_eq!(x.read(usize::MAX).unwrap(), None);
        x.insert(8, Bytes::from_static(b"ijkl"));
        assert_eq!(
            x.read(usize::MAX).unwrap(),
            Some(Bytes::from_static(b"jkl"))
        );
        assert_eq!(x.read(usize::MAX).unwrap(), None);
        x.insert(12, Bytes::from_static(b"mno"));
        assert_eq!(
            x.read(usize::MAX).unwrap(),
            Some(Bytes::from_static(b"mno"))
        );
        assert_eq!(x.read(usize::MAX).unwrap(), None);
        x.insert(2, Bytes::from_static(b"cde"));
        assert_eq!(x.read(usize::MAX).unwrap(), None);
    }

    fn next(x: &mut Assembler, size: usize) -> Option<Bytes> {
        x.read(size).unwrap()
    }
}
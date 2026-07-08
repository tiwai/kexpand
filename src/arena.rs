/// Arena that keeps byte buffers alive for its lifetime.
/// Slices into these buffers are valid as long as the arena is alive.
///
/// The pattern (borrowed from rapidquilt's FileArena): store data in heap-pinned
/// `Box<[u8]>` inside a `Mutex<Vec<...>>`. The Vec never removes elements, and
/// Box contents never move on Vec realloc, so raw slice pointers remain valid.
/// An `unsafe transmute` extends the lifetime of returned slices to `'a`.
use std::marker::PhantomData;
use std::sync::Mutex;

pub struct ByteArena<'a> {
    storage: Mutex<Vec<Box<[u8]>>>,
    _phantom: PhantomData<&'a [u8]>,
}

unsafe impl Sync for ByteArena<'_> {}

impl<'a> ByteArena<'a> {
    pub fn new() -> Self {
        ByteArena {
            storage: Mutex::new(Vec::new()),
            _phantom: PhantomData,
        }
    }

    /// Store `data` in the arena and return a slice with lifetime `'a`.
    /// Safety: the arena must outlive all returned slices.
    pub fn alloc(&self, data: Vec<u8>) -> &'a [u8] {
        let boxed = data.into_boxed_slice();
        // Safety: boxed is pinned on the heap, storage never removes elements,
        // and the arena's lifetime 'a >= lifetime of &self.
        let slice: &'a [u8] =
            unsafe { std::mem::transmute::<&[u8], &'a [u8]>(&boxed) };
        self.storage.lock().unwrap().push(boxed);
        slice
    }

    /// Load a file from disk and return its bytes with lifetime `'a`.
    pub fn load_file(&self, path: &std::path::Path) -> std::io::Result<&'a [u8]> {
        let bytes = std::fs::read(path)?;
        Ok(self.alloc(bytes))
    }
}

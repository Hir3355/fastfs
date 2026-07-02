use std::ffi::c_void;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::EventCallback;

const ENTRY_BATCH: u8 = 1;
const TEXT_BATCH: u8 = 2;
const ERROR: u8 = 3;
const INITIAL_BATCH_ITEMS: u32 = 16;
const MAX_ENTRY_BATCH_ITEMS: u32 = 256;
const MAX_TEXT_BATCH_ITEMS: u32 = 1024;
const MAX_BATCH_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum EntryKind {
    Other = 0,
    File = 1,
    Directory = 2,
    Symlink = 3,
}

pub(crate) struct EncodedTextBatch {
    data: Vec<u8>,
    count: u32,
}

impl EncodedTextBatch {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::new(),
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, value: &str) -> bool {
        if self.count == u32::MAX || u32::try_from(value.len()).is_err() {
            return false;
        }
        if self.count == 0 {
            self.data.push(TEXT_BATCH);
            self.data.extend_from_slice(&0_u32.to_le_bytes());
        }
        let appended = append_string(&mut self.data, value);
        debug_assert!(appended, "text length was validated above");
        self.count += 1;
        self.data[1..5].copy_from_slice(&self.count.to_le_bytes());
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }
}

impl Default for EncodedTextBatch {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct Emitter {
    callback: EventCallback,
    context: *mut c_void,
    entry_buffer: Vec<u8>,
    entry_count: u32,
    text_buffer: Vec<u8>,
    text_count: u32,
    entry_batch_limit: u32,
    text_batch_limit: u32,
    cancellation: Option<Arc<AtomicBool>>,
    pub(crate) stopped: bool,
    pub(crate) error_count: usize,
}

impl Emitter {
    pub(crate) fn new(
        callback: EventCallback,
        context: *mut c_void,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            callback,
            context,
            entry_buffer: Vec::new(),
            entry_count: 0,
            text_buffer: Vec::new(),
            text_count: 0,
            entry_batch_limit: INITIAL_BATCH_ITEMS,
            text_batch_limit: INITIAL_BATCH_ITEMS,
            cancellation,
            stopped: false,
            error_count: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn entry(
        &mut self,
        path: &str,
        kind: EntryKind,
        length: Option<u64>,
        modified_unix_milliseconds: Option<u64>,
        readonly: bool,
    ) -> bool {
        if self.is_stopped() || !self.flush_text() {
            return false;
        }
        if self.entry_count == 0 {
            self.entry_buffer.push(ENTRY_BATCH);
            self.entry_buffer.extend_from_slice(&0_u32.to_le_bytes());
        }

        let mut flags = 0_u8;
        if length.is_some() {
            flags |= 1;
        }
        if modified_unix_milliseconds.is_some() {
            flags |= 2;
        }
        if readonly {
            flags |= 4;
        }
        self.entry_buffer.push(kind as u8);
        self.entry_buffer.push(flags);
        self.entry_buffer.extend_from_slice(&0_u16.to_le_bytes());
        self.entry_buffer
            .extend_from_slice(&length.unwrap_or_default().to_le_bytes());
        self.entry_buffer
            .extend_from_slice(&modified_unix_milliseconds.unwrap_or_default().to_le_bytes());
        if !append_string(&mut self.entry_buffer, path) {
            self.stopped = true;
            return false;
        }

        self.entry_count += 1;
        self.entry_buffer[1..5].copy_from_slice(&self.entry_count.to_le_bytes());
        if self.entry_count >= self.entry_batch_limit || self.entry_buffer.len() >= MAX_BATCH_BYTES
        {
            self.flush_entries()
        } else {
            true
        }
    }

    pub(crate) fn text(&mut self, value: &str) -> bool {
        if self.is_stopped() || !self.flush_entries() {
            return false;
        }
        if self.text_count == 0 {
            self.text_buffer.push(TEXT_BATCH);
            self.text_buffer.extend_from_slice(&0_u32.to_le_bytes());
        }
        if !append_string(&mut self.text_buffer, value) {
            self.stopped = true;
            return false;
        }

        self.text_count += 1;
        self.text_buffer[1..5].copy_from_slice(&self.text_count.to_le_bytes());
        if self.text_count >= self.text_batch_limit || self.text_buffer.len() >= MAX_BATCH_BYTES {
            self.flush_text()
        } else {
            true
        }
    }

    pub(crate) fn text_batch(&mut self, batch: EncodedTextBatch) -> bool {
        if batch.is_empty() {
            return !self.is_stopped();
        }
        if self.is_stopped() || !self.flush_pending() {
            return false;
        }
        self.emit(&batch.data)
    }

    pub(crate) fn error(
        &mut self,
        code: impl Into<String>,
        category: impl Into<String>,
        message: impl Into<String>,
        path: Option<String>,
    ) {
        self.error_count += 1;
        if self.is_stopped() || !self.flush_pending() {
            return;
        }

        let code = code.into();
        let category = category.into();
        let message = message.into();
        let mut data = Vec::with_capacity(
            code.len() + category.len() + message.len() + path.as_ref().map_or(0, String::len) + 21,
        );
        data.push(ERROR);
        if !append_string(&mut data, &code)
            || !append_string(&mut data, &category)
            || !append_string(&mut data, &message)
        {
            self.stopped = true;
            return;
        }
        match path {
            Some(path) => {
                if !append_string(&mut data, &path) {
                    self.stopped = true;
                    return;
                }
            }
            None => data.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
        self.emit(&data);
    }

    pub(crate) fn finish(&mut self) {
        self.flush_pending();
    }

    pub(crate) fn cancellation(&self) -> Option<Arc<AtomicBool>> {
        self.cancellation.as_ref().map(Arc::clone)
    }

    pub(crate) fn is_stopped(&mut self) -> bool {
        if !self.stopped
            && self
                .cancellation
                .as_ref()
                .is_some_and(|state| state.load(Ordering::Relaxed))
        {
            self.stopped = true;
        }
        self.stopped
    }

    fn flush_pending(&mut self) -> bool {
        self.flush_entries() && self.flush_text()
    }

    fn flush_entries(&mut self) -> bool {
        if self.is_stopped() {
            return false;
        }
        if self.entry_count == 0 {
            return true;
        }
        let callback = self.callback;
        let context = self.context;
        let emitted_count = self.entry_count;
        let result = callback(self.entry_buffer.as_ptr(), self.entry_buffer.len(), context);
        self.entry_buffer.clear();
        self.entry_count = 0;
        if result != 0 {
            self.stopped = true;
        } else if emitted_count >= self.entry_batch_limit {
            self.entry_batch_limit = (self.entry_batch_limit * 2).min(MAX_ENTRY_BATCH_ITEMS);
        }
        !self.stopped
    }

    fn flush_text(&mut self) -> bool {
        if self.is_stopped() {
            return false;
        }
        if self.text_count == 0 {
            return true;
        }
        let callback = self.callback;
        let context = self.context;
        let emitted_count = self.text_count;
        let result = callback(self.text_buffer.as_ptr(), self.text_buffer.len(), context);
        self.text_buffer.clear();
        self.text_count = 0;
        if result != 0 {
            self.stopped = true;
        } else if emitted_count >= self.text_batch_limit {
            self.text_batch_limit = (self.text_batch_limit * 2).min(MAX_TEXT_BATCH_ITEMS);
        }
        !self.stopped
    }

    fn emit(&mut self, data: &[u8]) -> bool {
        if self.is_stopped() {
            return false;
        }
        if (self.callback)(data.as_ptr(), data.len(), self.context) != 0 {
            self.stopped = true;
        }
        !self.stopped
    }
}

fn append_string(buffer: &mut Vec<u8>, value: &str) -> bool {
    let Ok(length) = u32::try_from(value.len()) else {
        return false;
    };
    buffer.extend_from_slice(&length.to_le_bytes());
    buffer.extend_from_slice(value.as_bytes());
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::slice;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::{Emitter, EncodedTextBatch, EntryKind, TEXT_BATCH};

    extern "system" fn collect(data: *const u8, length: usize, context: *mut c_void) -> i32 {
        // SAFETY: The test passes a valid Vec pointer and the callback data is valid for this call.
        unsafe {
            let events = &mut *context.cast::<Vec<Vec<u8>>>();
            events.push(slice::from_raw_parts(data, length).to_vec());
        }
        0
    }

    #[test]
    fn batches_text_events() {
        let mut events: Vec<Vec<u8>> = Vec::new();
        let context = (&mut events as *mut Vec<Vec<u8>>).cast::<c_void>();
        let mut emitter = Emitter::new(collect, context, None);
        for _ in 0..1025 {
            assert!(emitter.text("line"));
        }
        emitter.finish();

        let counts: Vec<u32> = events
            .iter()
            .map(|event| {
                assert_eq!(event[0], 2);
                u32::from_le_bytes(event[1..5].try_into().unwrap())
            })
            .collect();
        assert_eq!(counts, [16, 32, 64, 128, 256, 512, 17]);
    }

    #[test]
    fn emits_preencoded_text_batches_directly() {
        let mut events: Vec<Vec<u8>> = Vec::new();
        let context = (&mut events as *mut Vec<Vec<u8>>).cast::<c_void>();
        let mut emitter = Emitter::new(collect, context, None);
        let mut batch = EncodedTextBatch::new();
        assert!(batch.push("first"));
        assert!(batch.push("second"));
        assert!(emitter.text_batch(batch));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0][0], 2);
        assert_eq!(u32::from_le_bytes(events[0][1..5].try_into().unwrap()), 2);
    }

    #[test]
    fn preencoded_text_batch_allocates_lazily() {
        let mut batch = EncodedTextBatch::new();
        assert_eq!(batch.data.capacity(), 0);
        assert_eq!(batch.len(), 0);

        assert!(batch.push("line"));
        assert_eq!(batch.count(), 1);
        assert_eq!(batch.data[0], TEXT_BATCH);
    }

    #[test]
    fn encodes_entry_fields_and_progressive_batches() {
        let mut events: Vec<Vec<u8>> = Vec::new();
        let context = (&mut events as *mut Vec<Vec<u8>>).cast::<c_void>();
        let mut emitter = Emitter::new(collect, context, None);
        for index in 0..497 {
            assert!(emitter.entry("資料.txt", EntryKind::File, Some(index), Some(99), true));
        }
        emitter.finish();

        let counts: Vec<u32> = events
            .iter()
            .map(|event| u32::from_le_bytes(event[1..5].try_into().unwrap()))
            .collect();
        assert_eq!(counts, [16, 32, 64, 128, 256, 1]);
        let first = &events[0];
        assert_eq!(first[0], 1);
        assert_eq!(first[5], EntryKind::File as u8);
        assert_eq!(first[6], 7);
        assert_eq!(u16::from_le_bytes(first[7..9].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(first[9..17].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(first[17..25].try_into().unwrap()), 99);
        let path_length = u32::from_le_bytes(first[25..29].try_into().unwrap()) as usize;
        assert_eq!(&first[29..29 + path_length], "資料.txt".as_bytes());
    }

    #[test]
    fn flushes_pending_text_before_error_with_optional_path() {
        let mut events: Vec<Vec<u8>> = Vec::new();
        let context = (&mut events as *mut Vec<Vec<u8>>).cast::<c_void>();
        let mut emitter = Emitter::new(collect, context, None);
        assert!(emitter.text("before"));
        emitter.error(
            "ReadFailed",
            "ReadError",
            "message",
            Some("file.txt".to_owned()),
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0][0], 2);
        assert_eq!(events[1][0], 3);
        assert!(events[1].windows(8).any(|window| window == b"file.txt"));
    }

    #[test]
    fn cancellation_discards_pending_output_without_a_callback() {
        let mut events: Vec<Vec<u8>> = Vec::new();
        let context = (&mut events as *mut Vec<Vec<u8>>).cast::<c_void>();
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut emitter = Emitter::new(collect, context, Some(Arc::clone(&cancellation)));
        assert!(emitter.text("pending"));

        cancellation.store(true, Ordering::Relaxed);
        emitter.finish();

        assert!(emitter.stopped);
        assert!(events.is_empty());
    }

    extern "system" fn stop_after_first_batch(
        _data: *const u8,
        _length: usize,
        context: *mut c_void,
    ) -> i32 {
        // SAFETY: The test passes a valid usize pointer for the callback lifetime.
        unsafe {
            *context.cast::<usize>() += 1;
        }
        1
    }

    #[test]
    fn callback_stop_prevents_further_batches() {
        let mut callback_count = 0_usize;
        let context = (&mut callback_count as *mut usize).cast::<c_void>();
        let mut emitter = Emitter::new(stop_after_first_batch, context, None);
        for index in 0..100 {
            if !emitter.text(&index.to_string()) {
                break;
            }
        }
        emitter.finish();

        assert!(emitter.stopped);
        assert_eq!(callback_count, 1);
    }

    #[test]
    fn callback_stop_is_observed_for_a_single_preencoded_batch() {
        let mut callback_count = 0_usize;
        let context = (&mut callback_count as *mut usize).cast::<c_void>();
        let mut emitter = Emitter::new(stop_after_first_batch, context, None);
        let mut batch = EncodedTextBatch::new();
        assert!(batch.push("first result"));

        assert!(!emitter.text_batch(batch));
        assert!(emitter.stopped);
        assert_eq!(callback_count, 1);
    }
}

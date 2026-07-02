//! FastFs' independent, line-oriented file scanner.
//!
//! The scanner deliberately knows nothing about PowerShell output or a
//! particular regular-expression implementation. A matcher can offer a
//! whole-buffer `find_at` fast path, while the same API remains usable for a
//! per-line matcher (which is necessary for decoded UTF-16 input).

#[cfg(test)]
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

use memchr::{memchr, memchr_iter, memrchr};

use crate::native_matcher::LineMatcher;

const READ_BUFFER_SIZE: usize = 64 * 1024;
const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_COLLECTED_BLOCK_MATCHES: usize = 64 * 1024;

/// How an encountered NUL byte is handled.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum BinaryDetection {
    /// Stop searching a file when it contains a NUL character.
    #[default]
    Quit,
    /// Treat NUL as ordinary text.
    Text,
}

/// The output-oriented behaviour requested for a scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ScanMode {
    /// Send matching lines and requested context to the sink.
    #[default]
    Standard,
    /// Count matching lines without calling the sink for each line.
    Count,
    /// Stop at the first matching line after reporting it to the sink.
    FilesWithMatches,
}

/// Search controls independent of the command-line parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScannerOptions {
    pub before_context: usize,
    pub after_context: usize,
    pub max_matches: Option<u64>,
    pub binary_detection: BinaryDetection,
    pub mode: ScanMode,
}

impl Default for ScannerOptions {
    fn default() -> Self {
        Self {
            before_context: 0,
            after_context: 0,
            max_matches: None,
            binary_detection: BinaryDetection::Quit,
            mode: ScanMode::Standard,
        }
    }
}

/// A logical line supplied to a [`LineSink`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanLine<'a> {
    pub bytes: &'a [u8],
    pub line_number: u64,
}

#[cfg(test)]
impl ScanLine<'_> {
    /// Returns the line using the same replacement behaviour as
    /// `String::from_utf8_lossy`.
    #[must_use]
    pub(crate) fn text_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.bytes)
    }
}

/// Signals whether a sink wants the scanner to continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanControl {
    Continue,
    Stop,
}

/// Receives search output without coupling the scanner to its presentation.
pub(crate) trait LineSink {
    fn matched(&mut self, line: ScanLine<'_>) -> io::Result<ScanControl>;
    fn context(&mut self, line: ScanLine<'_>) -> io::Result<ScanControl>;
    fn context_break(&mut self) -> io::Result<ScanControl>;
}

/// Statistics and non-error terminal conditions for one file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScanResult {
    /// Matching lines reported to the sink, or the count in [`ScanMode::Count`].
    pub match_count: u64,
    /// Matching logical lines found before output-mode filtering.
    pub candidate_count: u64,
    /// A NUL character ended the scan in [`BinaryDetection::Quit`] mode.
    pub binary: bool,
    /// The shared cancellation flag was set while scanning.
    pub cancelled: bool,
    /// The sink asked the scanner to stop.
    pub stopped: bool,
}

/// Reusable state for scanning many files.
pub(crate) struct NativeScanner {
    options: ScannerOptions,
    line_buffer: Vec<u8>,
    next_line_buffer: Vec<u8>,
    matcher_buffer: Vec<u8>,
    file_buffer: Vec<u8>,
}

impl Default for NativeScanner {
    fn default() -> Self {
        Self::new(ScannerOptions::default())
    }
}

impl NativeScanner {
    #[must_use]
    pub(crate) fn new(options: ScannerOptions) -> Self {
        Self {
            options,
            line_buffer: Vec::new(),
            next_line_buffer: Vec::new(),
            matcher_buffer: Vec::new(),
            file_buffer: Vec::new(),
        }
    }

    /// Scans one reader. The generic matcher and sink keep the hot path
    /// statically dispatched; no per-line trait-object call is required.
    pub(crate) fn scan_reader<R, M, S>(
        &mut self,
        reader: R,
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<ScanResult>
    where
        R: Read + Seek,
        M: LineMatcher,
        S: LineSink,
    {
        let mut result = ScanResult::default();
        if self.options.max_matches == Some(0) {
            return Ok(result);
        }
        if is_cancelled(cancelled) {
            result.cancelled = true;
            return Ok(result);
        }

        let mut source = CancelReader::new(reader, cancelled);
        if !matcher.supports_block_search() {
            return self.scan_stream_reader(source, matcher, sink, cancelled, result);
        }

        match self.read_whole_file(&mut source, cancelled)? {
            WholeFileRead::Complete => {
                let (encoding, bom_len) = detect_encoding(&self.file_buffer);
                match encoding {
                    InputEncoding::Utf8 => {
                        let replay = std::mem::take(&mut self.file_buffer);
                        let outcome = self.scan_block_bytes(
                            &replay[bom_len..],
                            matcher,
                            sink,
                            cancelled,
                            result,
                        );
                        self.file_buffer = replay;
                        outcome
                    }
                    InputEncoding::Utf16Le | InputEncoding::Utf16Be => {
                        let replay = std::mem::take(&mut self.file_buffer);
                        let mut cursor = Cursor::new(replay);
                        cursor.set_position(u64::try_from(bom_len).unwrap_or(u64::MAX));
                        let buffered = BufReader::with_capacity(READ_BUFFER_SIZE, cursor);
                        let endian = if encoding == InputEncoding::Utf16Le {
                            Utf16Endian::Little
                        } else {
                            Utf16Endian::Big
                        };
                        self.scan_utf16_stream(buffered, endian, matcher, sink, cancelled, result)
                    }
                }
            }
            WholeFileRead::Binary => Ok(ScanResult {
                binary: true,
                ..result
            }),
            WholeFileRead::Cancelled => Ok(ScanResult {
                cancelled: true,
                ..result
            }),
            WholeFileRead::TooLarge => {
                let replay = std::mem::take(&mut self.file_buffer);
                let (encoding, _) = detect_encoding(&replay);
                if encoding == InputEncoding::Utf8
                    && self.options.binary_detection == BinaryDetection::Quit
                {
                    if memchr(b'\0', &replay).is_some() {
                        return Ok(ScanResult {
                            binary: true,
                            ..result
                        });
                    }
                    let tail_has_nul = match stream_contains_nul(&mut source, cancelled) {
                        Ok(found) => found,
                        Err(_error) if is_cancelled(cancelled) => {
                            return Ok(ScanResult {
                                cancelled: true,
                                ..result
                            });
                        }
                        Err(error) => return Err(error),
                    };
                    if tail_has_nul {
                        return Ok(ScanResult {
                            binary: true,
                            ..result
                        });
                    }
                    if is_cancelled(cancelled) {
                        return Ok(ScanResult {
                            cancelled: true,
                            ..result
                        });
                    }
                    source.seek(SeekFrom::Start(0))?;
                    return self.scan_stream_reader(source, matcher, sink, cancelled, result);
                }
                let chained = Cursor::new(replay).chain(source);
                self.scan_stream_reader(chained, matcher, sink, cancelled, result)
            }
        }
    }

    fn scan_stream_reader<R, M, S>(
        &mut self,
        reader: R,
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        R: Read,
        M: LineMatcher,
        S: LineSink,
    {
        let mut source = BufReader::with_capacity(READ_BUFFER_SIZE, reader);
        let (encoding, bom_len) = match source.fill_buf() {
            Ok(bytes) => detect_encoding(bytes),
            Err(_error) if is_cancelled(cancelled) => {
                result.cancelled = true;
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        source.consume(bom_len);
        match encoding {
            InputEncoding::Utf8 => self.scan_utf8_stream(source, matcher, sink, cancelled, result),
            InputEncoding::Utf16Le => self.scan_utf16_stream(
                source,
                Utf16Endian::Little,
                matcher,
                sink,
                cancelled,
                result,
            ),
            InputEncoding::Utf16Be => {
                self.scan_utf16_stream(source, Utf16Endian::Big, matcher, sink, cancelled, result)
            }
        }
    }

    /// Scans an already resident byte buffer without copying it into the
    /// scanner's reusable file buffer. This is the mmap-friendly entry point
    /// for a small explicit file set.
    #[cfg(test)]
    pub(crate) fn scan_bytes<M, S>(
        &mut self,
        bytes: &[u8],
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<ScanResult>
    where
        M: LineMatcher,
        S: LineSink,
    {
        let mut result = ScanResult::default();
        if self.options.max_matches == Some(0) {
            return Ok(result);
        }
        if is_cancelled(cancelled) {
            result.cancelled = true;
            return Ok(result);
        }

        let (encoding, bom_len) = detect_encoding(bytes);
        if encoding != InputEncoding::Utf8 {
            // UTF-16 requires lossily decoding code units into the reusable
            // line buffer. The reader path retains the same BOM semantics.
            return self.scan_reader(Cursor::new(bytes), matcher, sink, cancelled);
        }
        let haystack = &bytes[bom_len..];
        if self.options.binary_detection == BinaryDetection::Quit
            && memchr(b'\0', haystack).is_some()
        {
            result.binary = true;
            return Ok(result);
        }
        if matcher.supports_block_search() {
            self.scan_block_bytes(haystack, matcher, sink, cancelled, result)
        } else {
            self.scan_utf8_stream(Cursor::new(haystack), matcher, sink, cancelled, result)
        }
    }

    fn read_whole_file<R>(
        &mut self,
        mut reader: R,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<WholeFileRead>
    where
        R: Read,
    {
        self.file_buffer.clear();
        let mut encoding = None;
        let mut binary_checked_until = 0;
        loop {
            if is_cancelled(cancelled) {
                return Ok(WholeFileRead::Cancelled);
            }
            let start = self.file_buffer.len();
            let remaining = MAX_BLOCK_BYTES.saturating_add(1).saturating_sub(start);
            let request = remaining.min(READ_BUFFER_SIZE);
            if request == 0 {
                return Ok(WholeFileRead::TooLarge);
            }
            self.file_buffer.resize(start + request, 0);
            let read = match reader.read(&mut self.file_buffer[start..]) {
                Ok(read) => read,
                Err(_error) if is_cancelled(cancelled) => return Ok(WholeFileRead::Cancelled),
                Err(error) => return Err(error),
            };
            if read == 0 {
                self.file_buffer.truncate(start);
                if self.options.binary_detection == BinaryDetection::Quit && encoding.is_none() {
                    let (detected, bom_len) = detect_encoding(&self.file_buffer);
                    if detected == InputEncoding::Utf8
                        && memchr(b'\0', &self.file_buffer[bom_len..]).is_some()
                    {
                        self.file_buffer.clear();
                        return Ok(WholeFileRead::Binary);
                    }
                }
                return Ok(WholeFileRead::Complete);
            }
            self.file_buffer.truncate(start + read);
            if self.options.binary_detection == BinaryDetection::Quit {
                if encoding.is_none() && self.file_buffer.len() >= 3 {
                    let (detected, bom_len) = detect_encoding(&self.file_buffer);
                    encoding = Some(detected);
                    if detected == InputEncoding::Utf8 {
                        if memchr(b'\0', &self.file_buffer[bom_len..]).is_some() {
                            self.file_buffer.clear();
                            return Ok(WholeFileRead::Binary);
                        }
                        binary_checked_until = self.file_buffer.len();
                    }
                } else if encoding == Some(InputEncoding::Utf8) {
                    if memchr(b'\0', &self.file_buffer[binary_checked_until..]).is_some() {
                        self.file_buffer.clear();
                        return Ok(WholeFileRead::Binary);
                    }
                    binary_checked_until = self.file_buffer.len();
                }
            }
            if self.file_buffer.len() > MAX_BLOCK_BYTES {
                return Ok(WholeFileRead::TooLarge);
            }
        }
    }

    fn scan_block_bytes<M, S>(
        &mut self,
        haystack: &[u8],
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        M: LineMatcher,
        S: LineSink,
    {
        if self.options.mode == ScanMode::Count {
            return self.scan_block_count(haystack, matcher, cancelled, result);
        }
        if self.options.mode == ScanMode::Standard
            && self.options.before_context == 0
            && self.options.after_context == 0
        {
            return self.scan_block_without_context(haystack, matcher, sink, cancelled, result);
        }
        let collected = self.collect_block_matches(haystack, matcher, cancelled)?;
        if collected.overflowed {
            let buffered = BufReader::with_capacity(READ_BUFFER_SIZE, Cursor::new(haystack));
            return self.scan_utf8_stream(buffered, matcher, sink, cancelled, result);
        }
        result.candidate_count = u64::try_from(collected.matches.len()).unwrap_or(u64::MAX);
        if collected.cancelled {
            result.cancelled = true;
            return Ok(result);
        }
        let matches = collected.matches;
        if matches.is_empty() {
            return Ok(result);
        }
        if is_cancelled(cancelled) {
            result.cancelled = true;
            return Ok(result);
        }

        match self.options.mode {
            ScanMode::Count => {
                result.match_count =
                    u64::try_from(self.primary_match_len(matches.len())).unwrap_or(u64::MAX);
                Ok(result)
            }
            ScanMode::FilesWithMatches => {
                let matched = matches[0];
                let bytes =
                    logical_line_bytes(&haystack[matched.line_start..matched.line_full_end]);
                result.match_count = 1;
                if sink.matched(ScanLine {
                    bytes,
                    line_number: matched.line_number,
                })? == ScanControl::Stop
                {
                    result.stopped = true;
                }
                Ok(result)
            }
            ScanMode::Standard => {
                let groups = self.output_groups(haystack, &matches);
                self.emit_block_groups(haystack, &matches, &groups, sink, cancelled, &mut result)?;
                Ok(result)
            }
        }
    }

    fn scan_block_count<M>(
        &self,
        haystack: &[u8],
        matcher: &M,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        M: LineMatcher,
    {
        let mut at = 0;
        while at < haystack.len() {
            if is_cancelled(cancelled) {
                result.cancelled = true;
                return Ok(result);
            }
            let Some(found) = matcher.find_at(haystack, at) else {
                return Ok(result);
            };
            if found.start < at || found.end < found.start || found.end > haystack.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FastFs rg matcher returned an invalid match range",
                ));
            }
            if found.start == haystack.len() {
                if haystack.last() == Some(&b'\n') {
                    return Ok(result);
                }
                let line_start = memrchr(b'\n', haystack)
                    .map(|offset| offset + 1)
                    .unwrap_or(0);
                if line_start < at {
                    return Ok(result);
                }
                result.candidate_count = result.candidate_count.saturating_add(1);
                result.match_count = result.match_count.saturating_add(1);
                return Ok(result);
            }

            let line_full_end = memchr(b'\n', &haystack[found.start..])
                .map(|offset| found.start + offset + 1)
                .unwrap_or(haystack.len());
            result.candidate_count = result.candidate_count.saturating_add(1);
            result.match_count = result.match_count.saturating_add(1);
            if reached_limit(result.match_count, self.options.max_matches)
                || line_full_end == haystack.len()
            {
                return Ok(result);
            }
            at = line_full_end;
        }
        Ok(result)
    }

    fn scan_block_without_context<M, S>(
        &self,
        haystack: &[u8],
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        M: LineMatcher,
        S: LineSink,
    {
        let mut at = 0;
        let mut next_line_number = 1_u64;
        while at < haystack.len() {
            if is_cancelled(cancelled) {
                result.cancelled = true;
                return Ok(result);
            }
            let Some(found) = matcher.find_at(haystack, at) else {
                return Ok(result);
            };
            if is_cancelled(cancelled) {
                result.cancelled = true;
                return Ok(result);
            }
            if found.start < at || found.end < found.start || found.end > haystack.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FastFs rg matcher returned an invalid match range",
                ));
            }

            let (line_start, line_full_end, line_number) = if found.start == haystack.len() {
                if haystack.last() == Some(&b'\n') {
                    return Ok(result);
                }
                let line_start = memrchr(b'\n', haystack)
                    .map(|offset| offset + 1)
                    .unwrap_or(0);
                if line_start < at {
                    return Ok(result);
                }
                let skipped = u64::try_from(memchr_iter(b'\n', &haystack[at..line_start]).count())
                    .unwrap_or(u64::MAX);
                (
                    line_start,
                    haystack.len(),
                    next_line_number.saturating_add(skipped),
                )
            } else {
                let between = &haystack[at..found.start];
                let skipped =
                    u64::try_from(memchr_iter(b'\n', between).count()).unwrap_or(u64::MAX);
                let line_start = memrchr(b'\n', between)
                    .map(|offset| at + offset + 1)
                    .unwrap_or(at);
                let line_full_end = memchr(b'\n', &haystack[found.start..])
                    .map(|offset| found.start + offset + 1)
                    .unwrap_or(haystack.len());
                (
                    line_start,
                    line_full_end,
                    next_line_number.saturating_add(skipped),
                )
            };

            result.candidate_count = result.candidate_count.saturating_add(1);
            result.match_count = result.match_count.saturating_add(1);
            if sink.matched(ScanLine {
                bytes: logical_line_bytes(&haystack[line_start..line_full_end]),
                line_number,
            })? == ScanControl::Stop
            {
                result.stopped = true;
                return Ok(result);
            }
            if reached_limit(result.match_count, self.options.max_matches)
                || line_full_end == haystack.len()
            {
                return Ok(result);
            }
            at = line_full_end;
            next_line_number = line_number.saturating_add(1);
        }
        Ok(result)
    }

    fn collect_block_matches<M>(
        &self,
        haystack: &[u8],
        matcher: &M,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<BlockMatchCollection>
    where
        M: LineMatcher,
    {
        let mut matches = Vec::new();
        let mut at = 0;
        let mut next_line_number = 1_u64;
        let mut selected_matches = 0_usize;
        let maximum = self
            .options
            .max_matches
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX));
        let mut context_end_after_limit = None;

        while at < haystack.len() {
            if context_end_after_limit.is_some_and(|end| at >= end) {
                break;
            }
            if is_cancelled(cancelled) {
                return Ok(BlockMatchCollection {
                    matches,
                    cancelled: true,
                    overflowed: false,
                });
            }
            let Some(found) = matcher.find_at(haystack, at) else {
                break;
            };
            if is_cancelled(cancelled) {
                return Ok(BlockMatchCollection {
                    matches,
                    cancelled: true,
                    overflowed: false,
                });
            }
            if found.start < at || found.end < found.start || found.end > haystack.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FastFs rg matcher returned an invalid match range",
                ));
            }
            if context_end_after_limit.is_some_and(|end| found.start >= end) {
                break;
            }
            if found.start == haystack.len() {
                // A terminal zero-width match belongs to the final logical
                // line only when that line has no terminating LF. This is
                // needed for patterns such as `$` and `\z`, while avoiding a
                // synthetic empty line after a trailing newline.
                if haystack.last() != Some(&b'\n') {
                    let line_start = memrchr(b'\n', haystack)
                        .map(|offset| offset + 1)
                        .unwrap_or(0);
                    if line_start >= at {
                        let skipped_lines =
                            u64::try_from(memchr_iter(b'\n', &haystack[at..line_start]).count())
                                .unwrap_or(u64::MAX);
                        if matches.len() == MAX_COLLECTED_BLOCK_MATCHES {
                            return Ok(BlockMatchCollection {
                                matches,
                                cancelled: false,
                                overflowed: true,
                            });
                        }
                        matches.push(BlockMatchLine {
                            line_start,
                            line_full_end: haystack.len(),
                            line_number: next_line_number.saturating_add(skipped_lines),
                        });
                    }
                }
                break;
            }

            let between = &haystack[at..found.start];
            let skipped_lines =
                u64::try_from(memchr_iter(b'\n', between).count()).unwrap_or(u64::MAX);
            let line_number = next_line_number.saturating_add(skipped_lines);
            let line_start = memrchr(b'\n', between)
                .map(|offset| at + offset + 1)
                .unwrap_or(at);
            let line_full_end = memchr(b'\n', &haystack[found.start..])
                .map(|offset| found.start + offset + 1)
                .unwrap_or(haystack.len());

            if matches.len() == MAX_COLLECTED_BLOCK_MATCHES {
                return Ok(BlockMatchCollection {
                    matches,
                    cancelled: false,
                    overflowed: true,
                });
            }
            matches.push(BlockMatchLine {
                line_start,
                line_full_end,
                line_number,
            });
            at = line_full_end;
            next_line_number = line_number.saturating_add(1);

            match self.options.mode {
                ScanMode::FilesWithMatches => break,
                ScanMode::Count => {
                    selected_matches = selected_matches.saturating_add(1);
                    if maximum.is_some_and(|maximum| selected_matches >= maximum) {
                        break;
                    }
                }
                ScanMode::Standard => {
                    if maximum.is_none_or(|maximum| selected_matches < maximum) {
                        selected_matches = selected_matches.saturating_add(1);
                        if maximum.is_some_and(|maximum| selected_matches >= maximum) {
                            let end =
                                context_end(haystack, line_full_end, self.options.after_context);
                            if end <= at {
                                break;
                            }
                            context_end_after_limit = Some(end);
                        }
                    }
                }
            }
        }
        Ok(BlockMatchCollection {
            matches,
            cancelled: false,
            overflowed: false,
        })
    }

    fn output_groups(&self, haystack: &[u8], matches: &[BlockMatchLine]) -> Vec<OutputGroup> {
        let selected = self.primary_match_len(matches.len());
        let mut groups: Vec<OutputGroup> = Vec::with_capacity(selected);
        for matched in matches.iter().take(selected) {
            let start = context_start(haystack, matched.line_start, self.options.before_context);
            let preceding =
                u64::try_from(memchr_iter(b'\n', &haystack[start..matched.line_start]).count())
                    .unwrap_or(u64::MAX);
            let start_line_number = matched.line_number.saturating_sub(preceding);
            let end = context_end(haystack, matched.line_full_end, self.options.after_context);
            if let Some(last) = groups.last_mut()
                && start <= last.end
            {
                last.end = last.end.max(end);
            } else {
                groups.push(OutputGroup {
                    start,
                    end,
                    start_line_number,
                });
            }
        }
        groups
    }

    fn emit_block_groups<S>(
        &self,
        haystack: &[u8],
        matches: &[BlockMatchLine],
        groups: &[OutputGroup],
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        result: &mut ScanResult,
    ) -> io::Result<()>
    where
        S: LineSink,
    {
        let mut match_index = 0;
        for (group_index, group) in groups.iter().enumerate() {
            if is_cancelled(cancelled) {
                result.cancelled = true;
                return Ok(());
            }
            while match_index < matches.len() && matches[match_index].line_start < group.start {
                match_index += 1;
            }
            if group_index != 0 && sink.context_break()? == ScanControl::Stop {
                result.stopped = true;
                return Ok(());
            }

            let mut offset = group.start;
            let mut line_number = group.start_line_number;
            while offset < group.end {
                if is_cancelled(cancelled) {
                    result.cancelled = true;
                    return Ok(());
                }
                let line_full_end = memchr(b'\n', &haystack[offset..group.end])
                    .map(|relative| offset + relative + 1)
                    .unwrap_or(group.end);
                let line = ScanLine {
                    bytes: logical_line_bytes(&haystack[offset..line_full_end]),
                    line_number,
                };
                let is_match =
                    match_index < matches.len() && matches[match_index].line_start == offset;
                let control = if is_match {
                    result.match_count = result.match_count.saturating_add(1);
                    sink.matched(line)?
                } else {
                    sink.context(line)?
                };
                if control == ScanControl::Stop {
                    result.stopped = true;
                    return Ok(());
                }
                if is_match {
                    match_index += 1;
                }
                offset = line_full_end;
                line_number = line_number.saturating_add(1);
            }
        }
        Ok(())
    }

    fn scan_utf8_stream<R, M, S>(
        &mut self,
        reader: R,
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        R: BufRead,
        M: LineMatcher,
        S: LineSink,
    {
        let mut reader = reader;
        let mut state = StreamState::default();
        self.line_buffer.clear();
        let (has_first, mut line_terminator) =
            match read_utf8_logical_line(&mut reader, &mut self.line_buffer) {
                Ok(line) => line,
                Err(_error) if is_cancelled(cancelled) => {
                    result.cancelled = true;
                    return Ok(result);
                }
                Err(error) => return Err(error),
            };
        if !has_first {
            return Ok(result);
        }

        let mut line_number = 1_u64;
        loop {
            self.next_line_buffer.clear();
            let (has_next, next_line_terminator) =
                match read_utf8_logical_line(&mut reader, &mut self.next_line_buffer) {
                    Ok(line) => line,
                    Err(_error) if is_cancelled(cancelled) => {
                        result.cancelled = true;
                        return Ok(result);
                    }
                    Err(error) => return Err(error),
                };
            if self.options.binary_detection == BinaryDetection::Quit
                && memchr(b'\0', &self.line_buffer).is_some()
            {
                result.binary = true;
                return Ok(result);
            }
            let (matcher_line_start, matcher_line_length) = prepare_matcher_line(
                &mut self.matcher_buffer,
                &self.line_buffer,
                line_number == 1,
                line_terminator,
            );
            match process_stream_line(
                self.options,
                &mut state,
                matcher,
                sink,
                &self.matcher_buffer,
                &self.line_buffer,
                matcher_line_start,
                matcher_line_length,
                line_number,
                cancelled,
                &mut result,
            )? {
                StreamFlow::Continue => {}
                StreamFlow::Stopped => return Ok(result),
            }
            if !has_next {
                return Ok(result);
            }
            std::mem::swap(&mut self.line_buffer, &mut self.next_line_buffer);
            line_terminator = next_line_terminator;
            line_number = line_number.saturating_add(1);
        }
    }

    fn scan_utf16_stream<R, M, S>(
        &mut self,
        reader: R,
        endian: Utf16Endian,
        matcher: &M,
        sink: &mut S,
        cancelled: Option<&AtomicBool>,
        mut result: ScanResult,
    ) -> io::Result<ScanResult>
    where
        R: Read,
        M: LineMatcher,
        S: LineSink,
    {
        let mut reader = Utf16LineReader::new(reader, endian);
        let mut state = StreamState::default();
        self.line_buffer.clear();
        let (has_first, mut line_terminator) = match reader.read_line(&mut self.line_buffer) {
            Ok(line) => line,
            Err(_error) if is_cancelled(cancelled) => {
                result.cancelled = true;
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        if !has_first {
            return Ok(result);
        }

        let mut line_number = 1_u64;
        loop {
            self.next_line_buffer.clear();
            let (has_next, next_line_terminator) =
                match reader.read_line(&mut self.next_line_buffer) {
                    Ok(line) => line,
                    Err(_error) if is_cancelled(cancelled) => {
                        result.cancelled = true;
                        return Ok(result);
                    }
                    Err(error) => return Err(error),
                };
            if self.options.binary_detection == BinaryDetection::Quit
                && memchr(b'\0', &self.line_buffer).is_some()
            {
                result.binary = true;
                return Ok(result);
            }
            let (matcher_line_start, matcher_line_length) = prepare_matcher_line(
                &mut self.matcher_buffer,
                &self.line_buffer,
                line_number == 1,
                line_terminator,
            );
            match process_stream_line(
                self.options,
                &mut state,
                matcher,
                sink,
                &self.matcher_buffer,
                &self.line_buffer,
                matcher_line_start,
                matcher_line_length,
                line_number,
                cancelled,
                &mut result,
            )? {
                StreamFlow::Continue => {}
                StreamFlow::Stopped => return Ok(result),
            }
            if !has_next {
                return Ok(result);
            }
            std::mem::swap(&mut self.line_buffer, &mut self.next_line_buffer);
            line_terminator = next_line_terminator;
            line_number = line_number.saturating_add(1);
        }
    }

    fn primary_match_len(&self, available: usize) -> usize {
        self.options
            .max_matches
            .map(|maximum| {
                usize::try_from(maximum)
                    .unwrap_or(usize::MAX)
                    .min(available)
            })
            .unwrap_or(available)
    }
}

/// Wraps any reader and turns a shared cancellation request into an interrupt.
pub(crate) struct CancelReader<'a, R> {
    inner: R,
    cancelled: Option<&'a AtomicBool>,
}

impl<'a, R> CancelReader<'a, R> {
    #[must_use]
    pub(crate) fn new(inner: R, cancelled: Option<&'a AtomicBool>) -> Self {
        Self { inner, cancelled }
    }
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if is_cancelled(self.cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FastFs rg search cancelled",
            ));
        }
        self.inner.read(buffer)
    }
}

impl<R: Seek> Seek for CancelReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if is_cancelled(self.cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FastFs rg search cancelled",
            ));
        }
        self.inner.seek(position)
    }
}

fn stream_contains_nul<R: Read>(
    reader: &mut R,
    cancelled: Option<&AtomicBool>,
) -> io::Result<bool> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if is_cancelled(cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FastFs rg search cancelled",
            ));
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(false);
        }
        if memchr(b'\0', &buffer[..read]).is_some() {
            return Ok(true);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

fn detect_encoding(bytes: &[u8]) -> (InputEncoding, usize) {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        (InputEncoding::Utf16Le, 2)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (InputEncoding::Utf16Be, 2)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (InputEncoding::Utf8, 3)
    } else {
        (InputEncoding::Utf8, 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WholeFileRead {
    Complete,
    Binary,
    Cancelled,
    TooLarge,
}

#[derive(Clone, Copy, Debug)]
struct BlockMatchLine {
    line_start: usize,
    line_full_end: usize,
    line_number: u64,
}

struct BlockMatchCollection {
    matches: Vec<BlockMatchLine>,
    cancelled: bool,
    overflowed: bool,
}

#[derive(Clone, Copy, Debug)]
struct OutputGroup {
    start: usize,
    end: usize,
    start_line_number: u64,
}

#[derive(Default)]
struct StreamState {
    before: VecDeque<StoredLine>,
    last_emitted_line: Option<u64>,
    after_remaining: usize,
    selected_matches: u64,
}

struct StoredLine {
    bytes: Vec<u8>,
    line_number: u64,
}

enum StreamFlow {
    Continue,
    Stopped,
}

#[allow(clippy::too_many_arguments)]
fn process_stream_line<M, S>(
    options: ScannerOptions,
    state: &mut StreamState,
    matcher: &M,
    sink: &mut S,
    matcher_line: &[u8],
    output_line: &[u8],
    matcher_line_start: usize,
    matcher_line_length: usize,
    line_number: u64,
    cancelled: Option<&AtomicBool>,
    result: &mut ScanResult,
) -> io::Result<StreamFlow>
where
    M: LineMatcher,
    S: LineSink,
{
    if is_cancelled(cancelled) {
        result.cancelled = true;
        return Ok(StreamFlow::Stopped);
    }

    match options.mode {
        ScanMode::Count => {
            if reached_limit(state.selected_matches, options.max_matches) {
                return Ok(StreamFlow::Stopped);
            }
            if matcher_matches_line(
                matcher,
                matcher_line,
                matcher_line_start,
                matcher_line_length,
            ) {
                state.selected_matches = state.selected_matches.saturating_add(1);
                result.candidate_count = result.candidate_count.saturating_add(1);
                result.match_count = result.match_count.saturating_add(1);
                if reached_limit(state.selected_matches, options.max_matches) {
                    return Ok(StreamFlow::Stopped);
                }
            }
            return Ok(StreamFlow::Continue);
        }
        ScanMode::FilesWithMatches => {
            if !matcher_matches_line(
                matcher,
                matcher_line,
                matcher_line_start,
                matcher_line_length,
            ) {
                return Ok(StreamFlow::Continue);
            }
            result.candidate_count = result.candidate_count.saturating_add(1);
            result.match_count = 1;
            if sink.matched(ScanLine {
                bytes: output_line,
                line_number,
            })? == ScanControl::Stop
            {
                result.stopped = true;
            }
            return Ok(StreamFlow::Stopped);
        }
        ScanMode::Standard => {}
    }

    let limit_reached = reached_limit(state.selected_matches, options.max_matches);
    if limit_reached && state.after_remaining == 0 {
        return Ok(StreamFlow::Stopped);
    }
    let matched = matcher_matches_line(
        matcher,
        matcher_line,
        matcher_line_start,
        matcher_line_length,
    );
    if matched {
        result.candidate_count = result.candidate_count.saturating_add(1);
        if !limit_reached {
            if !emit_before_context(state, sink, cancelled, result)? {
                return Ok(StreamFlow::Stopped);
            }
            if !emit_stream_line(
                state,
                sink,
                output_line,
                line_number,
                true,
                cancelled,
                result,
            )? {
                return Ok(StreamFlow::Stopped);
            }
            result.match_count = result.match_count.saturating_add(1);
            state.selected_matches = state.selected_matches.saturating_add(1);
            state.after_remaining = state.after_remaining.max(options.after_context);
        } else {
            // Matches inside the final requested after-context range retain
            // their match separator, but do not extend that range or consume
            // another --max-count slot.
            if !emit_stream_line(
                state,
                sink,
                output_line,
                line_number,
                true,
                cancelled,
                result,
            )? {
                return Ok(StreamFlow::Stopped);
            }
            result.match_count = result.match_count.saturating_add(1);
            state.after_remaining = state.after_remaining.saturating_sub(1);
        }
    } else if state.after_remaining > 0 {
        if !emit_stream_line(
            state,
            sink,
            output_line,
            line_number,
            false,
            cancelled,
            result,
        )? {
            return Ok(StreamFlow::Stopped);
        }
        state.after_remaining = state.after_remaining.saturating_sub(1);
    }

    if options.before_context > 0 {
        state.before.push_back(StoredLine {
            bytes: output_line.to_vec(),
            line_number,
        });
        if state.before.len() > options.before_context {
            state.before.pop_front();
        }
    }
    if reached_limit(state.selected_matches, options.max_matches) && state.after_remaining == 0 {
        Ok(StreamFlow::Stopped)
    } else {
        Ok(StreamFlow::Continue)
    }
}

fn emit_before_context<S>(
    state: &mut StreamState,
    sink: &mut S,
    cancelled: Option<&AtomicBool>,
    result: &mut ScanResult,
) -> io::Result<bool>
where
    S: LineSink,
{
    let already_emitted = state.last_emitted_line;
    let pending = state
        .before
        .iter()
        .filter(|stored| Some(stored.line_number) > already_emitted)
        .map(|stored| (stored.line_number, stored.bytes.clone()))
        .collect::<Vec<_>>();
    for (line_number, bytes) in pending {
        if !emit_stream_line(state, sink, &bytes, line_number, false, cancelled, result)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn emit_stream_line<S>(
    state: &mut StreamState,
    sink: &mut S,
    bytes: &[u8],
    line_number: u64,
    matched: bool,
    cancelled: Option<&AtomicBool>,
    result: &mut ScanResult,
) -> io::Result<bool>
where
    S: LineSink,
{
    if is_cancelled(cancelled) {
        result.cancelled = true;
        return Ok(false);
    }
    if state
        .last_emitted_line
        .is_some_and(|last| line_number > last.saturating_add(1))
        && sink.context_break()? == ScanControl::Stop
    {
        result.stopped = true;
        return Ok(false);
    }
    let line = ScanLine { bytes, line_number };
    let control = if matched {
        sink.matched(line)?
    } else {
        sink.context(line)?
    };
    if control == ScanControl::Stop {
        result.stopped = true;
        return Ok(false);
    }
    state.last_emitted_line = Some(line_number);
    Ok(true)
}

fn reached_limit(current: u64, maximum: Option<u64>) -> bool {
    maximum.is_some_and(|maximum| current >= maximum)
}

fn read_utf8_logical_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<(bool, LineTerminator)> {
    line.clear();
    if reader.read_until(b'\n', line)? == 0 {
        return Ok((false, LineTerminator::None));
    }
    let mut terminator = LineTerminator::None;
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
            terminator = LineTerminator::CrLf;
        } else {
            terminator = LineTerminator::Lf;
        }
    }
    Ok((true, terminator))
}

fn prepare_matcher_line(
    buffer: &mut Vec<u8>,
    line: &[u8],
    is_first: bool,
    terminator: LineTerminator,
) -> (usize, usize) {
    buffer.clear();
    buffer.reserve(line.len().saturating_add(2));
    if !is_first {
        buffer.push(b'\n');
    }
    let line_start = buffer.len();
    buffer.extend_from_slice(line);
    let matcher_line_length = line.len() + usize::from(matches!(terminator, LineTerminator::CrLf));
    match terminator {
        LineTerminator::None => {}
        LineTerminator::Lf => buffer.push(b'\n'),
        LineTerminator::CrLf => buffer.extend_from_slice(b"\r\n"),
    }
    (line_start, matcher_line_length)
}

fn matcher_matches_line<M: LineMatcher>(
    matcher: &M,
    contextual: &[u8],
    line_start: usize,
    line_length: usize,
) -> bool {
    if !matcher.supports_block_search() {
        return matcher.is_match(contextual);
    }

    let line_end = line_start.saturating_add(line_length);
    let mut at = 0;
    while at <= contextual.len() {
        let Some(found) = matcher.find_at(contextual, at) else {
            return false;
        };
        if found.start < at || found.end < found.start || found.end > contextual.len() {
            return false;
        }
        let belongs_to_line = if found.start == found.end {
            found.start >= line_start && found.start <= line_end
        } else {
            found.start < line_end && found.end > line_start
        };
        if belongs_to_line {
            return true;
        }
        if found.start == contextual.len() {
            return false;
        }
        at = found.end.max(found.start.saturating_add(1));
    }
    false
}

fn logical_line_bytes(mut bytes: &[u8]) -> &[u8] {
    if bytes.last() == Some(&b'\n') {
        bytes = &bytes[..bytes.len() - 1];
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
    }
    bytes
}

fn context_start(haystack: &[u8], line_start: usize, before: usize) -> usize {
    let mut start = line_start;
    for _ in 0..before {
        if start == 0 {
            break;
        }
        let previous_end = start - 1;
        start = memrchr(b'\n', &haystack[..previous_end])
            .map(|offset| offset + 1)
            .unwrap_or(0);
    }
    start
}

fn context_end(haystack: &[u8], line_full_end: usize, after: usize) -> usize {
    let mut end = line_full_end;
    for _ in 0..after {
        if end >= haystack.len() {
            break;
        }
        end = memchr(b'\n', &haystack[end..])
            .map(|offset| end + offset + 1)
            .unwrap_or(haystack.len());
    }
    end
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

#[derive(Clone, Copy)]
enum Utf16Endian {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineTerminator {
    None,
    Lf,
    CrLf,
}

struct Utf16LineReader<R> {
    reader: R,
    endian: Utf16Endian,
    read_buffer: Vec<u8>,
    position: usize,
    length: usize,
    decoder: Utf16Decoder,
    pending_cr: bool,
}

impl<R: Read> Utf16LineReader<R> {
    fn new(reader: R, endian: Utf16Endian) -> Self {
        Self {
            reader,
            endian,
            read_buffer: vec![0; READ_BUFFER_SIZE],
            position: 0,
            length: 0,
            decoder: Utf16Decoder::default(),
            pending_cr: false,
        }
    }

    /// Reads one logical UTF-16 line, converting it to UTF-8 lossily.
    fn read_line(&mut self, output: &mut Vec<u8>) -> io::Result<(bool, LineTerminator)> {
        let mut saw_input = false;
        loop {
            let Some(first) = self.read_byte()? else {
                if !saw_input {
                    return Ok((false, LineTerminator::None));
                }
                self.flush_pending(output);
                return Ok((true, LineTerminator::None));
            };
            saw_input = true;
            let Some(second) = self.read_byte()? else {
                self.flush_pending(output);
                append_replacement(output);
                return Ok((true, LineTerminator::None));
            };
            let unit = match self.endian {
                Utf16Endian::Little => u16::from_le_bytes([first, second]),
                Utf16Endian::Big => u16::from_be_bytes([first, second]),
            };
            if unit == 0x000A {
                let terminator = if self.pending_cr {
                    LineTerminator::CrLf
                } else {
                    LineTerminator::Lf
                };
                self.pending_cr = false;
                self.decoder.finish(output);
                return Ok((true, terminator));
            }
            if self.pending_cr {
                self.decoder.push(0x000D, output);
                self.pending_cr = false;
            }
            if unit == 0x000D {
                self.pending_cr = true;
            } else {
                self.decoder.push(unit, output);
            }
        }
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if self.position == self.length {
            self.length = self.reader.read(&mut self.read_buffer)?;
            self.position = 0;
            if self.length == 0 {
                return Ok(None);
            }
        }
        let byte = self.read_buffer[self.position];
        self.position += 1;
        Ok(Some(byte))
    }

    fn flush_pending(&mut self, output: &mut Vec<u8>) {
        if self.pending_cr {
            self.decoder.push(0x000D, output);
            self.pending_cr = false;
        }
        self.decoder.finish(output);
    }
}

#[derive(Default)]
struct Utf16Decoder {
    pending_high_surrogate: Option<u16>,
}

impl Utf16Decoder {
    fn push(&mut self, unit: u16, output: &mut Vec<u8>) {
        if let Some(high) = self.pending_high_surrogate.take() {
            if (0xDC00..=0xDFFF).contains(&unit) {
                let scalar = 0x1_0000 + (((high as u32 - 0xD800) << 10) | (unit as u32 - 0xDC00));
                append_char(
                    char::from_u32(scalar).expect("valid UTF-16 surrogate pair"),
                    output,
                );
                return;
            }
            append_replacement(output);
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            self.pending_high_surrogate = Some(unit);
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            append_replacement(output);
        } else {
            append_char(
                char::from_u32(unit as u32).expect("non-surrogate UTF-16 unit"),
                output,
            );
        }
    }

    fn finish(&mut self, output: &mut Vec<u8>) {
        if self.pending_high_surrogate.take().is_some() {
            append_replacement(output);
        }
    }
}

fn append_replacement(output: &mut Vec<u8>) {
    output.extend_from_slice("�".as_bytes());
}

fn append_char(character: char, output: &mut Vec<u8>) {
    let mut encoded = [0_u8; 4];
    output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryDetection, LineMatcher, LineSink, NativeScanner, ScanControl, ScanLine, ScanMode,
        ScannerOptions,
    };
    use crate::native_matcher::{MatchRange, MatcherOptions, NativeMatcher};
    use std::cell::Cell;
    use std::io::{self, Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingSink {
        events: Vec<Event>,
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Event {
        Match(u64, String),
        Context(u64, String),
        Break,
    }

    impl LineSink for RecordingSink {
        fn matched(&mut self, line: ScanLine<'_>) -> io::Result<ScanControl> {
            self.events.push(Event::Match(
                line.line_number,
                line.text_lossy().into_owned(),
            ));
            Ok(ScanControl::Continue)
        }

        fn context(&mut self, line: ScanLine<'_>) -> io::Result<ScanControl> {
            self.events.push(Event::Context(
                line.line_number,
                line.text_lossy().into_owned(),
            ));
            Ok(ScanControl::Continue)
        }

        fn context_break(&mut self) -> io::Result<ScanControl> {
            self.events.push(Event::Break);
            Ok(ScanControl::Continue)
        }
    }

    #[derive(Default)]
    struct StopOnMatchSink {
        matched: usize,
    }

    impl LineSink for StopOnMatchSink {
        fn matched(&mut self, _line: ScanLine<'_>) -> io::Result<ScanControl> {
            self.matched += 1;
            Ok(ScanControl::Stop)
        }

        fn context(&mut self, _line: ScanLine<'_>) -> io::Result<ScanControl> {
            Ok(ScanControl::Continue)
        }

        fn context_break(&mut self) -> io::Result<ScanControl> {
            Ok(ScanControl::Continue)
        }
    }

    struct GeneratedLargeReader {
        position: usize,
        length: usize,
        bytes_read: Arc<AtomicUsize>,
        first_request: Arc<AtomicUsize>,
    }

    impl Read for GeneratedLargeReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.first_request
                .compare_exchange(0, buffer.len(), Ordering::Relaxed, Ordering::Relaxed)
                .ok();
            let read = buffer.len().min(self.length.saturating_sub(self.position));
            if read == 0 {
                return Ok(0);
            }
            const PREFIX: &[u8] = b"hit\nnext\n";
            for (offset, byte) in buffer[..read].iter_mut().enumerate() {
                let position = self.position + offset;
                *byte = PREFIX.get(position).copied().unwrap_or(b'x');
            }
            self.position += read;
            self.bytes_read.fetch_add(read, Ordering::Relaxed);
            Ok(read)
        }
    }

    impl Seek for GeneratedLargeReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            let next = match position {
                SeekFrom::Start(offset) => i128::from(offset),
                SeekFrom::Current(offset) => self.position as i128 + i128::from(offset),
                SeekFrom::End(offset) => self.length as i128 + i128::from(offset),
            };
            if next < 0 || next > self.length as i128 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "seek outside generated reader",
                ));
            }
            self.position = usize::try_from(next).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "seek position is too large")
            })?;
            Ok(self.position as u64)
        }
    }

    struct NeedleSet {
        needles: Vec<Vec<u8>>,
    }

    impl NeedleSet {
        fn new(values: &[&str]) -> Self {
            Self {
                needles: values
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect(),
            }
        }
    }

    impl LineMatcher for NeedleSet {
        fn is_match(&self, line: &[u8]) -> bool {
            self.needles
                .iter()
                .any(|needle| line.windows(needle.len()).any(|window| window == needle))
        }

        fn supports_block_search(&self) -> bool {
            true
        }

        fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
            let mut best: Option<MatchRange> = None;
            for needle in &self.needles {
                if needle.is_empty() {
                    continue;
                }
                if let Some(offset) = haystack[at..]
                    .windows(needle.len())
                    .position(|window| window == needle)
                {
                    let start = at + offset;
                    let candidate = MatchRange {
                        start,
                        end: start + needle.len(),
                    };
                    if best.is_none_or(|current| candidate.start < current.start) {
                        best = Some(candidate);
                    }
                }
            }
            best
        }
    }

    struct LinePredicate<F>(F);

    impl<F> LineMatcher for LinePredicate<F>
    where
        F: Fn(&[u8]) -> bool,
    {
        fn supports_block_search(&self) -> bool {
            false
        }

        fn find_at(&self, _haystack: &[u8], _at: usize) -> Option<MatchRange> {
            None
        }

        fn is_match(&self, line: &[u8]) -> bool {
            self.0(line)
        }
    }

    struct CountingNeedle {
        needle: Vec<u8>,
        calls: Cell<usize>,
    }

    impl CountingNeedle {
        fn new(needle: &str) -> Self {
            Self {
                needle: needle.as_bytes().to_vec(),
                calls: Cell::new(0),
            }
        }
    }

    impl LineMatcher for CountingNeedle {
        fn supports_block_search(&self) -> bool {
            true
        }

        fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
            self.calls.set(self.calls.get().saturating_add(1));
            haystack[at..]
                .windows(self.needle.len())
                .position(|window| window == self.needle)
                .map(|relative| MatchRange {
                    start: at + relative,
                    end: at + relative + self.needle.len(),
                })
        }

        fn is_match(&self, line: &[u8]) -> bool {
            line.windows(self.needle.len())
                .any(|window| window == self.needle)
        }
    }

    struct EofMatcher;

    impl LineMatcher for EofMatcher {
        fn supports_block_search(&self) -> bool {
            true
        }

        fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
            (at <= haystack.len()).then_some(MatchRange {
                start: haystack.len(),
                end: haystack.len(),
            })
        }

        fn is_match(&self, _line: &[u8]) -> bool {
            false
        }
    }

    struct CancellingMatcher<'a> {
        flag: &'a AtomicBool,
        calls: Cell<usize>,
    }

    impl LineMatcher for CancellingMatcher<'_> {
        fn supports_block_search(&self) -> bool {
            true
        }

        fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
            self.calls.set(self.calls.get().saturating_add(1));
            self.flag.store(true, Ordering::Relaxed);
            haystack[at..]
                .iter()
                .position(|byte| *byte == b'h')
                .map(|relative| MatchRange {
                    start: at + relative,
                    end: at + relative + 1,
                })
        }

        fn is_match(&self, _line: &[u8]) -> bool {
            false
        }
    }

    fn scan(
        options: ScannerOptions,
        input: &[u8],
        matcher: &impl LineMatcher,
    ) -> (super::ScanResult, RecordingSink) {
        let mut scanner = NativeScanner::new(options);
        let mut sink = RecordingSink::default();
        let result = scanner
            .scan_reader(Cursor::new(input), matcher, &mut sink, None)
            .expect("scan should succeed");
        (result, sink)
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn block_path_emits_context_and_separator() {
        let matcher = NeedleSet::new(&["two", "six"]);
        let (result, sink) = scan(
            ScannerOptions {
                before_context: 1,
                after_context: 1,
                ..ScannerOptions::default()
            },
            b"zero\none\ntwo\none\nfour\nfive\nsix\nseven\n",
            &matcher,
        );
        assert_eq!(result.match_count, 2);
        assert_eq!(
            sink.events,
            vec![
                Event::Context(2, "one".to_owned()),
                Event::Match(3, "two".to_owned()),
                Event::Context(4, "one".to_owned()),
                Event::Break,
                Event::Context(6, "five".to_owned()),
                Event::Match(7, "six".to_owned()),
                Event::Context(8, "seven".to_owned()),
            ],
        );
    }

    #[test]
    fn dense_context_matches_fall_back_to_bounded_streaming() {
        let input = b"hit\n".repeat(super::MAX_COLLECTED_BLOCK_MATCHES + 1);
        let matcher = NeedleSet::new(&["hit"]);
        let mut scanner = NativeScanner::new(ScannerOptions {
            before_context: 1,
            after_context: 1,
            ..ScannerOptions::default()
        });
        let mut sink = StopOnMatchSink::default();
        let result = scanner
            .scan_reader(Cursor::new(input), &matcher, &mut sink, None)
            .expect("dense context scan should succeed");
        assert!(result.stopped);
        assert_eq!(sink.matched, 1);
    }

    #[test]
    fn max_count_keeps_matching_after_context_as_a_match() {
        let matcher = NeedleSet::new(&["two", "four"]);
        let (result, sink) = scan(
            ScannerOptions {
                after_context: 2,
                max_matches: Some(1),
                ..ScannerOptions::default()
            },
            b"zero\none\ntwo\none\nfour\nfive\n",
            &matcher,
        );
        assert_eq!(result.match_count, 2);
        assert_eq!(
            sink.events,
            vec![
                Event::Match(3, "two".to_owned()),
                Event::Context(4, "one".to_owned()),
                Event::Match(5, "four".to_owned()),
            ],
        );
    }

    #[test]
    fn block_collection_stops_when_mode_and_context_allow_it() {
        let input = b"hit\nhit\nhit\nhit\nhit\n";

        let files_matcher = CountingNeedle::new("hit");
        let (files, _) = scan(
            ScannerOptions {
                mode: ScanMode::FilesWithMatches,
                ..ScannerOptions::default()
            },
            input,
            &files_matcher,
        );
        assert_eq!(files.match_count, 1);
        assert_eq!(files_matcher.calls.get(), 1);

        let count_matcher = CountingNeedle::new("hit");
        let (count, _) = scan(
            ScannerOptions {
                mode: ScanMode::Count,
                max_matches: Some(1),
                ..ScannerOptions::default()
            },
            input,
            &count_matcher,
        );
        assert_eq!(count.match_count, 1);
        assert_eq!(count_matcher.calls.get(), 1);

        let standard_matcher = CountingNeedle::new("hit");
        let (standard, sink) = scan(
            ScannerOptions {
                max_matches: Some(1),
                after_context: 2,
                ..ScannerOptions::default()
            },
            input,
            &standard_matcher,
        );
        assert_eq!(standard.candidate_count, 3);
        assert_eq!(standard.match_count, 3);
        assert_eq!(standard_matcher.calls.get(), 3);
        assert_eq!(sink.events.len(), 3);
    }

    #[test]
    fn eof_zero_width_match_maps_to_an_unterminated_final_line_once() {
        let matcher = EofMatcher;
        let (result, sink) = scan(ScannerOptions::default(), b"final", &matcher);
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(1, "final".to_owned())]);

        let (result, sink) = scan(ScannerOptions::default(), b"final\n", &matcher);
        assert_eq!(result.match_count, 0);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn cancellation_between_block_candidates_suppresses_late_output() {
        let cancelled = AtomicBool::new(false);
        let matcher = CancellingMatcher {
            flag: &cancelled,
            calls: Cell::new(0),
        };
        let mut scanner = NativeScanner::default();
        let mut sink = RecordingSink::default();
        let result = scanner
            .scan_reader(
                Cursor::new(b"hello\nhello\n"),
                &matcher,
                &mut sink,
                Some(&cancelled),
            )
            .expect("cancellation should not be an error");
        assert!(result.cancelled);
        assert_eq!(matcher.calls.get(), 1);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn count_and_files_with_matches_short_circuit_output_correctly() {
        let input = b"one\ntwo\nthree\ntwo\n";
        let count_matcher = NeedleSet::new(&["two"]);
        let (count, count_sink) = scan(
            ScannerOptions {
                mode: ScanMode::Count,
                ..ScannerOptions::default()
            },
            input,
            &count_matcher,
        );
        assert_eq!(count.match_count, 2);
        assert!(count_sink.events.is_empty());

        let files_matcher = NeedleSet::new(&["two"]);
        let (files, files_sink) = scan(
            ScannerOptions {
                mode: ScanMode::FilesWithMatches,
                ..ScannerOptions::default()
            },
            input,
            &files_matcher,
        );
        assert_eq!(files.match_count, 1);
        assert_eq!(files_sink.events, vec![Event::Match(2, "two".to_owned())]);
    }

    #[test]
    fn quit_binary_mode_discards_block_path_output() {
        let matcher = NeedleSet::new(&["hello"]);
        let (result, sink) = scan(
            ScannerOptions::default(),
            b"hello\n\0later\nhello\n",
            &matcher,
        );
        assert!(result.binary);
        assert!(sink.events.is_empty());

        let matcher = NeedleSet::new(&["hello"]);
        let (result, sink) = scan(
            ScannerOptions {
                binary_detection: BinaryDetection::Text,
                ..ScannerOptions::default()
            },
            b"hello\n\0later\nhello\n",
            &matcher,
        );
        assert!(!result.binary);
        assert_eq!(result.match_count, 2);
        assert_eq!(
            sink.events,
            vec![
                Event::Match(1, "hello".to_owned()),
                Event::Match(3, "hello".to_owned()),
            ],
        );
    }

    #[test]
    fn quit_binary_mode_discards_large_streaming_output() {
        let mut input = vec![b'x'; super::MAX_BLOCK_BYTES + 1024];
        input[..4].copy_from_slice(b"hit\n");
        input[super::MAX_BLOCK_BYTES + 512] = 0;
        let matcher = NeedleSet::new(&["hit"]);
        let (result, sink) = scan(ScannerOptions::default(), &input, &matcher);
        assert!(result.binary);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn byte_slice_path_keeps_utf8_bom_and_binary_semantics_without_a_copy() {
        let matcher = NeedleSet::new(&["hello"]);
        let mut scanner = NativeScanner::default();
        let mut sink = RecordingSink::default();
        let result = scanner
            .scan_bytes(b"\xEF\xBB\xBFskip\nhello\n", &matcher, &mut sink, None)
            .expect("scan should succeed");
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(2, "hello".to_owned())]);
        assert!(scanner.file_buffer.is_empty());

        let mut sink = RecordingSink::default();
        let result = scanner
            .scan_bytes(b"hello\n\0", &matcher, &mut sink, None)
            .expect("scan should succeed");
        assert!(result.binary);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn utf16_bom_is_decoded_for_line_matchers() {
        let le = NeedleSet::new(&["hello"]);
        let (le_result, le_sink) = scan(
            ScannerOptions::default(),
            &[
                0xFF, 0xFE, b'h', 0, b'i', 0, b'\n', 0, b'h', 0, b'e', 0, b'l', 0, b'l', 0, b'o', 0,
            ],
            &le,
        );
        assert_eq!(le_result.match_count, 1);
        assert_eq!(le_sink.events, vec![Event::Match(2, "hello".to_owned())]);

        let be = NeedleSet::new(&["hello"]);
        let (be_result, be_sink) = scan(
            ScannerOptions::default(),
            &[
                0xFE, 0xFF, 0, b'h', 0, b'i', 0, b'\n', 0, b'h', 0, b'e', 0, b'l', 0, b'l', 0, b'o',
            ],
            &be,
        );
        assert_eq!(be_result.match_count, 1);
        assert_eq!(be_sink.events, vec![Event::Match(2, "hello".to_owned())]);
    }

    #[test]
    fn invalid_utf8_is_available_lossily() {
        let matcher = LinePredicate(|line: &[u8]| line.starts_with(b"hi"));
        let (result, sink) = scan(ScannerOptions::default(), b"hi \xFF\nother\n", &matcher);
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(1, "hi �".to_owned())]);
    }

    #[test]
    fn buffered_probe_and_size_cap_allow_early_sink_stop() {
        let total = super::MAX_BLOCK_BYTES * 2;
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let first_request = Arc::new(AtomicUsize::new(0));
        let reader = GeneratedLargeReader {
            position: 0,
            length: total,
            bytes_read: Arc::clone(&bytes_read),
            first_request: Arc::clone(&first_request),
        };
        let matcher = NeedleSet::new(&["hit"]);
        let mut scanner = NativeScanner::new(ScannerOptions {
            binary_detection: BinaryDetection::Text,
            ..ScannerOptions::default()
        });
        let mut sink = StopOnMatchSink::default();
        let result = scanner
            .scan_reader(reader, &matcher, &mut sink, None)
            .expect("large streaming fallback should succeed");

        assert!(result.stopped);
        assert_eq!(sink.matched, 1);
        assert_eq!(
            first_request.load(Ordering::Relaxed),
            super::READ_BUFFER_SIZE
        );
        assert!(bytes_read.load(Ordering::Relaxed) < total);
        assert!(scanner.file_buffer.is_empty());
        assert_eq!(scanner.file_buffer.capacity(), 0);
    }

    #[test]
    fn utf16_stream_preserves_absolute_file_anchors() {
        let starts_at_file = NativeMatcher::build("\\Aneedle", MatcherOptions::default())
            .expect("pattern should compile");
        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("first\nneedle\n"),
            &starts_at_file,
        );
        assert_eq!(result.match_count, 0);
        assert!(sink.events.is_empty());

        let ends_at_file = NativeMatcher::build("needle\\z", MatcherOptions::default())
            .expect("pattern should compile");
        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("needle\nlast"),
            &ends_at_file,
        );
        assert_eq!(result.match_count, 0);
        assert!(sink.events.is_empty());

        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("needle\n"),
            &ends_at_file,
        );
        assert_eq!(result.match_count, 0);
        assert!(sink.events.is_empty());

        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("first\nneedle"),
            &ends_at_file,
        );
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(2, "needle".to_owned())]);

        let before_crlf = NativeMatcher::build("needle\\r$", MatcherOptions::default())
            .expect("pattern should compile");
        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("needle\r\n"),
            &before_crlf,
        );
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(1, "needle".to_owned())]);

        let empty_line =
            NativeMatcher::build("^$", MatcherOptions::default()).expect("pattern should compile");
        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("first\nsecond"),
            &empty_line,
        );
        assert_eq!(result.match_count, 0);
        assert!(sink.events.is_empty());

        let (result, sink) = scan(
            ScannerOptions::default(),
            &utf16le("first\n\nsecond"),
            &empty_line,
        );
        assert_eq!(result.match_count, 1);
        assert_eq!(sink.events, vec![Event::Match(2, String::new())]);
    }

    #[test]
    fn cancellation_is_a_non_error_terminal_condition() {
        let cancelled = AtomicBool::new(true);
        let matcher = NeedleSet::new(&["hello"]);
        let mut scanner = NativeScanner::default();
        let mut sink = RecordingSink::default();
        let result = scanner
            .scan_reader(
                Cursor::new(b"hello\n"),
                &matcher,
                &mut sink,
                Some(&cancelled),
            )
            .expect("cancellation should not be reported as a read error");
        assert!(result.cancelled);
        assert!(sink.events.is_empty());
    }
}

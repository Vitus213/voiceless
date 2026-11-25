//! GGUF container reader for import checks: the fixed header, the
//! metadata keys that name a model and the tensor table, read without an
//! inference engine. `import.rs` uses it to refuse files that are not GGUF
//! or are cut short before the engine loads them; the engine then proves
//! that the file is a speech recognition model (`gguf_asr::GgufDecoder`).
//!
//! Layout (little-endian, GGUF v2 and v3; the `gguf.md` spec in ggml):
//! magic, version, tensor count, metadata count; then per metadata entry a
//! string key, a `u32` value type and the value; then per tensor a string
//! name, a `u32` dimension count, its `u64` dimensions, a `u32` ggml type
//! and the `u64` offset of its data; then padding up to
//! `general.alignment` (32 by default) and the tensor data itself. Only
//! the keys that name the model are kept; everything else is skipped, by
//! file offset when it does not fit the read buffer.
//!
//! Failure behaviour: every structural problem is `ModelError::Invalid`
//! with the cause (magic, version, an unknown type, a header or tensor
//! data that runs past the end of the file, no
//! `general.architecture`, no tensor); a file that cannot be read is
//! `ModelError::Io`. Nothing is allocated in proportion to a count the
//! file claims: every count is bounded by the bytes left in the file
//! before it is used.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use super::ModelError;

/// `"GGUF"` as a little-endian `u32`.
const MAGIC: u32 = 0x4655_4747;
/// `general.alignment` of a file that does not set it.
const DEFAULT_ALIGNMENT: u64 = 32;
/// Metadata value type of a string.
const STRING: u32 = 8;
/// Metadata value type of an array.
const ARRAY: u32 = 9;
/// Metadata value type of a `u32` (`general.alignment`).
const U32: u32 = 4;
/// Longest metadata string that is kept; longer ones are skipped.
const MAX_STRING: u64 = 4096;
/// Nesting depth of metadata arrays that is skipped.
const MAX_ARRAY_DEPTH: u8 = 4;
/// Bytes read at a time. The tokenizer of a large vocabulary is a few MB
/// of short strings, so a header takes a handful of reads.
const CHUNK: usize = 256 * 1024;

const NOT_GGUF: &str = "not a GGUF file";

/// What the GGUF header says about the file that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GgufHeader {
    /// `general.architecture`: the family the engine dispatches on
    /// (`qwen3_asr`).
    pub architecture: String,
    /// `general.name`; empty when the file does not carry it.
    pub name: String,
}

/// Read the header of the GGUF file at `path`. `Invalid` names what is
/// wrong (see the module doc).
pub(super) fn inspect(path: &Path) -> Result<GgufHeader, ModelError> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut scanner = Scanner::new(file, length);
    let magic = scanner
        .u32()
        .map_err(|_| ModelError::Invalid(NOT_GGUF.to_owned()))?;
    if magic != MAGIC {
        return Err(ModelError::Invalid(NOT_GGUF.to_owned()));
    }
    let version = scanner.u32()?;
    if !(2..=3).contains(&version) {
        return Err(ModelError::Invalid(format!(
            "unsupported GGUF version {version}"
        )));
    }
    let tensor_count = count(&mut scanner)?;
    let metadata_count = count(&mut scanner)?;

    let mut header = GgufHeader {
        architecture: String::new(),
        name: String::new(),
    };
    let mut alignment = DEFAULT_ALIGNMENT;
    for _ in 0..metadata_count {
        let key = scanner.string()?;
        let code = scanner.u32()?;
        match key.as_deref() {
            Some("general.architecture") if code == STRING => {
                header.architecture = scanner.string()?.unwrap_or_default()
            }
            Some("general.name") if code == STRING => {
                header.name = scanner.string()?.unwrap_or_default()
            }
            Some("general.alignment") if code == U32 => alignment = u64::from(scanner.u32()?),
            _ => skip_value(&mut scanner, code, 0)?,
        }
    }

    // The tensor table: only the largest data offset matters, as the last
    // tensor must still have data after it.
    let mut last_offset = 0_u64;
    for _ in 0..tensor_count {
        scanner.string()?;
        let dimensions = scanner.u32()?;
        if dimensions == 0 {
            return Err(malformed("a tensor has no dimensions"));
        }
        scanner.skip(u64::from(dimensions) * 8)?;
        scanner.skip(4)?;
        last_offset = last_offset.max(scanner.u64()?);
    }
    if header.architecture.is_empty() {
        return Err(ModelError::Invalid(
            "GGUF file has no general.architecture".to_owned(),
        ));
    }
    if tensor_count == 0 {
        return Err(ModelError::Invalid(
            "GGUF file contains no tensors".to_owned(),
        ));
    }
    if alignment == 0 {
        return Err(malformed("general.alignment is zero"));
    }
    let data_offset = scanner.position.div_ceil(alignment) * alignment;
    let minimum_length = data_offset
        .saturating_add(last_offset)
        .saturating_add(u64::from(tensor_count > 0));
    if length < minimum_length {
        return Err(ModelError::Invalid(format!(
            "truncated GGUF file: tensor data runs past byte {last_offset} of the data section but the file has {length} bytes"
        )));
    }
    Ok(header)
}

/// Bytes of a scalar metadata value; `None` for strings, arrays and
/// unknown types.
fn scalar_size(code: u32) -> Option<u64> {
    match code {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Step over one metadata value of type `code`.
fn skip_value<R: Read + Seek>(
    scanner: &mut Scanner<R>,
    code: u32,
    depth: u8,
) -> Result<(), ModelError> {
    match code {
        STRING => scanner.string().map(|_| ()),
        ARRAY => {
            if depth >= MAX_ARRAY_DEPTH {
                return Err(malformed("metadata arrays are nested too deeply"));
            }
            let element = scanner.u32()?;
            for _ in 0..count(scanner)? {
                skip_value(scanner, element, depth + 1)?;
            }
            Ok(())
        }
        _ => match scalar_size(code) {
            Some(size) => scanner.skip(size),
            None => Err(malformed(format!("unknown metadata type {code}"))),
        },
    }
}

/// A count read from the header. Nothing in the file has more elements
/// than the file has bytes left (an element is at least one byte), so a
/// larger count is a file cut short or not a model; no loop then runs in
/// proportion to what the header claims.
fn count<R: Read + Seek>(scanner: &mut Scanner<R>) -> Result<u64, ModelError> {
    let count = scanner.u64()?;
    scanner.within(count)?;
    Ok(count)
}

fn malformed(detail: impl std::fmt::Display) -> ModelError {
    ModelError::Invalid(format!("malformed GGUF header: {detail}"))
}

/// Sequential reader over the start of a file that keeps one chunk in
/// memory, so stepping over the short strings of a tokenizer costs no
/// system call each.
struct Scanner<R> {
    inner: R,
    buffer: Vec<u8>,
    /// Consumed prefix and filled end of `buffer`.
    start: usize,
    end: usize,
    /// File offset of `buffer[start]`, and the length of the file.
    position: u64,
    length: u64,
}

impl<R: Read + Seek> Scanner<R> {
    fn new(inner: R, length: u64) -> Self {
        Self {
            inner,
            buffer: vec![0; CHUNK],
            start: 0,
            end: 0,
            position: 0,
            length,
        }
    }

    fn truncated(&self, needed: u64) -> ModelError {
        ModelError::Invalid(format!(
            "truncated GGUF file: the header runs to byte {needed} but the file has {} bytes",
            self.length
        ))
    }

    /// Whether the file has `count` more bytes from here.
    fn within(&self, count: u64) -> Result<(), ModelError> {
        if self.length - self.position < count {
            Err(self.truncated(self.position.saturating_add(count)))
        } else {
            Ok(())
        }
    }

    /// Buffer at least `n` unread bytes (`n <= CHUNK`).
    fn fill(&mut self, n: usize) -> io::Result<()> {
        if self.end - self.start >= n {
            return Ok(());
        }
        self.buffer.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < n {
            match self.inner.read(&mut self.buffer[self.end..]) {
                Ok(0) => break,
                Ok(read) => self.end += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// The next `n` bytes (`n <= CHUNK`).
    fn take(&mut self, n: usize) -> Result<&[u8], ModelError> {
        self.within(u64::try_from(n).expect("a slice length"))?;
        self.fill(n)?;
        let bytes = &self.buffer[self.start..self.start + n];
        self.start += n;
        self.position += n as u64;
        Ok(bytes)
    }

    /// Step over `count` bytes.
    fn skip(&mut self, count: u64) -> Result<(), ModelError> {
        self.within(count)?;
        let buffered = (self.end - self.start) as u64;
        if count <= buffered {
            self.start += count as usize;
        } else {
            // Bounded by `within`, so the offset fits an `i64`.
            self.inner
                .seek(SeekFrom::Current((count - buffered) as i64))?;
            self.start = 0;
            self.end = 0;
        }
        self.position += count;
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, ModelError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, ModelError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    /// A GGUF string; `None` (skipped) when longer than `MAX_STRING`.
    fn string(&mut self) -> Result<Option<String>, ModelError> {
        let length = self.u64()?;
        if length > MAX_STRING {
            return self.skip(length).map(|()| None);
        }
        let bytes = self.take(length as usize)?.to_vec();
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn string(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value);
    }

    /// One metadata entry: key, type code, already-encoded value.
    fn entry(out: &mut Vec<u8>, key: &str, code: u32, value: &[u8]) {
        string(out, key.as_bytes());
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(value);
    }

    /// One tensor info with the given dimensions and data offset.
    fn tensor(out: &mut Vec<u8>, dimensions: &[u64], offset: u64) {
        string(out, b"weight");
        out.extend_from_slice(&(dimensions.len() as u32).to_le_bytes());
        for dimension in dimensions {
            out.extend_from_slice(&dimension.to_le_bytes());
        }
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
    }

    /// A GGUF v3 file with `metadata` and `tensors`, padded to the default
    /// alignment and followed by enough data bytes for every tensor.
    fn gguf(metadata: &[(&str, u32, &[u8])], tensors: &[(&[u64], u64)]) -> Vec<u8> {
        let mut out = MAGIC.to_le_bytes().to_vec();
        out.extend_from_slice(&3_u32.to_le_bytes());
        out.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        out.extend_from_slice(&(metadata.len() as u64).to_le_bytes());
        for (key, code, value) in metadata {
            entry(&mut out, key, *code, value);
        }
        for (dimensions, offset) in tensors {
            tensor(&mut out, dimensions, *offset);
        }
        out.resize(out.len().div_ceil(32) * 32, 0);
        let data = tensors.iter().map(|(_, offset)| *offset).max().unwrap_or(0);
        out.resize(out.len() + data as usize + 16, 0);
        out
    }

    /// One tensor of two elements whose data starts at `offset`.
    fn tensor_at(offset: u64) -> (&'static [u64], u64) {
        (&[2], offset)
    }

    fn inspect_bytes(bytes: &[u8]) -> Result<GgufHeader, ModelError> {
        let mut file = NamedTempFile::new().expect("temporary file");
        file.write_all(bytes).expect("write fixture");
        inspect(file.path())
    }

    fn invalid(bytes: &[u8]) -> String {
        match inspect_bytes(bytes) {
            Err(ModelError::Invalid(message)) => message,
            other => panic!("expected ModelError::Invalid, got {other:?}"),
        }
    }

    fn architecture() -> Vec<u8> {
        let mut value = Vec::new();
        string(&mut value, b"qwen3_asr");
        value
    }

    /// Architecture and name come from their keys, whatever their order.
    #[test]
    fn reads_the_name_and_architecture_of_a_well_formed_header() {
        let mut name = Vec::new();
        string(&mut name, b"Qwen3-ASR 0.6B");
        let bytes = gguf(
            &[
                ("general.name", STRING, &name),
                ("general.architecture", STRING, &architecture()),
            ],
            &[(&[2, 16], 0), (&[4], 32)],
        );
        assert_eq!(
            inspect_bytes(&bytes).expect("valid header"),
            GgufHeader {
                architecture: "qwen3_asr".to_owned(),
                name: "Qwen3-ASR 0.6B".to_owned(),
            }
        );
    }

    /// Everything but the three keys it keeps is stepped over whatever its
    /// type, including the long strings of a tokenizer, nested arrays and
    /// a `general.languages` array, and keys that follow are still read.
    #[test]
    fn skips_metadata_it_does_not_keep() {
        let long_key = vec![b'k'; 9_000];
        let mut long_string = Vec::new();
        string(&mut long_string, &vec![b'v'; 70_000]);
        let mut tokens = Vec::new();
        tokens.extend_from_slice(&STRING.to_le_bytes());
        tokens.extend_from_slice(&(30_000_u64).to_le_bytes());
        for _ in 0..30_000 {
            string(&mut tokens, b"token");
        }
        let mut languages = Vec::new();
        languages.extend_from_slice(&STRING.to_le_bytes());
        languages.extend_from_slice(&2_u64.to_le_bytes());
        string(&mut languages, b"zh");
        string(&mut languages, b"en");
        let mut nested = Vec::new();
        nested.extend_from_slice(&ARRAY.to_le_bytes());
        nested.extend_from_slice(&2_u64.to_le_bytes());
        for _ in 0..2 {
            nested.extend_from_slice(&STRING.to_le_bytes());
            nested.extend_from_slice(&1_u64.to_le_bytes());
            string(&mut nested, b"x");
        }
        let mut newline = Vec::new();
        string(&mut newline, b"x\nx");
        let mut name = Vec::new();
        string(&mut name, b"after the skipped keys");
        let bytes = gguf(
            &[
                ("tokenizer.ggml.tokens", ARRAY, &tokens),
                ("general.languages", ARRAY, &languages),
                ("split.strings", ARRAY, &nested),
                ("split.newline", STRING, &newline),
                ("general.quantization_version", 4, &2_u32.to_le_bytes()),
                ("offset_time", 10, &1_700_000_000_u64.to_le_bytes()),
                ("temperature", 6, &0.5_f32.to_le_bytes()),
                ("temperature_high", 12, &0.5_f64.to_le_bytes()),
                ("flags", 0, &[7_u8]),
                ("flags_signed", 1, &(-7_i8).to_le_bytes()),
                ("short", 2, &513_u16.to_le_bytes()),
                ("short_signed", 3, &(-2_i16).to_le_bytes()),
                ("signed", 5, &(-3_i32).to_le_bytes()),
                ("signed_long", 11, &(-4_i64).to_le_bytes()),
                ("enabled", 7, &[1_u8]),
                (
                    String::from_utf8(long_key).expect("key").as_str(),
                    STRING,
                    &long_string,
                ),
                ("general.architecture", STRING, &architecture()),
                ("general.name", STRING, &name),
            ],
            &[(&[2, 16], 0)],
        );
        let header = inspect_bytes(&bytes).expect("valid header");
        assert_eq!(header.architecture, "qwen3_asr");
        assert_eq!(header.name, "after the skipped keys");
    }

    #[test]
    fn rejects_files_that_are_not_gguf() {
        assert_eq!(invalid(b""), NOT_GGUF);
        assert_eq!(invalid(b"GUGF"), NOT_GGUF);
        assert_eq!(invalid(b"PK\x03\x04 not a model at all"), NOT_GGUF);
        assert_eq!(invalid(&[0_u8; 4096]), NOT_GGUF);
    }

    /// The magic alone, or a file of zeros behind it, names no version.
    #[test]
    fn rejects_a_magic_without_a_usable_version() {
        assert!(invalid(b"GGUF").starts_with("truncated GGUF file"));
        let mut zeros = b"GGUF".to_vec();
        zeros.extend_from_slice(&[0_u8; 4096]);
        assert!(invalid(&zeros).contains("unsupported GGUF version 0"));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let mut bytes = gguf(
            &[("general.architecture", STRING, &architecture())],
            &[tensor_at(0)],
        );
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert!(invalid(&bytes).contains("unsupported GGUF version 1"));
        bytes[4..8].copy_from_slice(&4_u32.to_le_bytes());
        assert!(invalid(&bytes).contains("unsupported GGUF version 4"));
    }

    /// Every cut inside the header, the tensor table or before the first
    /// byte of data is a truncation, never a read past the file.
    #[test]
    fn rejects_a_file_cut_short() {
        let mut languages = Vec::new();
        languages.extend_from_slice(&STRING.to_le_bytes());
        languages.extend_from_slice(&1_u64.to_le_bytes());
        string(&mut languages, b"zh");
        let bytes = gguf(
            &[
                ("general.architecture", STRING, &architecture()),
                ("general.languages", ARRAY, &languages),
            ],
            &[(&[2, 16], 0)],
        );
        let data = bytes.len() - 16;
        for cut in [4, 6, 9, 12, 17, 20, 23, 30, 41, 50, data - 1, data] {
            assert!(
                invalid(&bytes[..cut]).starts_with("truncated GGUF file"),
                "cut at {cut}"
            );
        }
        inspect_bytes(&bytes[..data + 1]).expect("one data byte is enough");
    }

    /// A header whose last tensor starts after the end of the file is a
    /// download that was cut short.
    #[test]
    fn rejects_tensor_data_past_the_end_of_the_file() {
        let mut bytes = gguf(
            &[("general.architecture", STRING, &architecture())],
            &[tensor_at(0), tensor_at(1 << 20)],
        );
        // Keep eight bytes of the data section: the second tensor starts a mebibyte in.
        let data = bytes.len() - (1 << 20) - 16;
        bytes.truncate(data + 8);
        assert!(invalid(&bytes).contains("truncated GGUF file"));
    }

    #[test]
    fn requires_an_architecture_and_tensors() {
        let architecture = architecture();
        let header = &[("general.architecture", STRING, architecture.as_slice())];
        assert!(invalid(&gguf(&[], &[tensor_at(0)])).contains("general.architecture"));
        let wrong_type = gguf(
            &[("general.architecture", 5, &7_i32.to_le_bytes())],
            &[tensor_at(0)],
        );
        assert!(invalid(&wrong_type).contains("general.architecture"));
        assert!(invalid(&gguf(header, &[])).contains("no tensors"));
        let no_dimensions: (&[u64], u64) = (&[], 0);
        assert!(invalid(&gguf(header, &[no_dimensions])).contains("no dimensions"));
    }

    /// Counts, types and nesting that no model has are refused from the
    /// header alone: a count is bounded by the bytes left in the file,
    /// so nothing is read in proportion to what a corrupt header claims.
    #[test]
    fn rejects_counts_the_file_cannot_hold_types_and_nesting() {
        let mut huge = MAGIC.to_le_bytes().to_vec();
        huge.extend_from_slice(&3_u32.to_le_bytes());
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        huge.extend_from_slice(&1_u64.to_le_bytes());
        assert!(invalid(&huge).starts_with("truncated GGUF file"));

        let mut tensor_count = MAGIC.to_le_bytes().to_vec();
        tensor_count.extend_from_slice(&3_u32.to_le_bytes());
        tensor_count.extend_from_slice(&100_001_u64.to_le_bytes());
        tensor_count.extend_from_slice(&0_u64.to_le_bytes());
        assert!(invalid(&tensor_count).starts_with("truncated GGUF file"));

        let mut elements = Vec::new();
        elements.extend_from_slice(&0_u32.to_le_bytes());
        elements.extend_from_slice(&1_000_u64.to_le_bytes());
        let padded = gguf(&[("flags", ARRAY, &elements)], &[tensor_at(0)]);
        assert!(invalid(&padded).starts_with("truncated GGUF file"));

        let unknown_type = gguf(&[("general.architecture", 13, b"")], &[tensor_at(0)]);
        assert!(invalid(&unknown_type).contains("unknown metadata type 13"));

        // Ten arrays, each holding the next; the innermost holds a `u32`.
        let mut deep = Vec::new();
        deep.extend_from_slice(&4_u32.to_le_bytes());
        deep.extend_from_slice(&0_u64.to_le_bytes());
        for _ in 0..10 {
            let mut wrapped = Vec::new();
            wrapped.extend_from_slice(&ARRAY.to_le_bytes());
            wrapped.extend_from_slice(&1_u64.to_le_bytes());
            wrapped.extend_from_slice(&deep);
            deep = wrapped;
        }
        let nested = gguf(&[("deep", ARRAY, &deep)], &[tensor_at(0)]);
        assert!(invalid(&nested).contains("nested too deeply"));
    }

    /// The tokenizer of a large vocabulary (Qwen3 has 151,936 tokens) is
    /// stepped over like any other array: the bound is what the file can
    /// hold, not a round number below real vocabularies.
    #[test]
    fn skips_a_vocabulary_of_over_a_hundred_thousand_tokens() {
        let mut tokens = Vec::new();
        tokens.extend_from_slice(&STRING.to_le_bytes());
        tokens.extend_from_slice(&151_936_u64.to_le_bytes());
        for index in 0..151_936 {
            string(&mut tokens, format!("token{index}").as_bytes());
        }
        let bytes = gguf(
            &[
                ("tokenizer.ggml.tokens", ARRAY, &tokens),
                ("general.architecture", STRING, &architecture()),
            ],
            &[tensor_at(0)],
        );
        assert_eq!(
            inspect_bytes(&bytes).expect("valid header").architecture,
            "qwen3_asr"
        );
    }

    /// An alignment of zero, or one that places the data past the end of
    /// the file, names no data section.
    #[test]
    fn rejects_an_alignment_that_cannot_place_the_data() {
        let architecture = architecture();
        let zero = gguf(
            &[
                ("general.architecture", STRING, &architecture),
                ("general.alignment", U32, &0_u32.to_le_bytes()),
            ],
            &[tensor_at(0)],
        );
        assert!(invalid(&zero).contains("alignment is zero"));
        let huge = gguf(
            &[
                ("general.architecture", STRING, &architecture),
                ("general.alignment", U32, &u32::MAX.to_le_bytes()),
            ],
            &[tensor_at(0)],
        );
        assert!(invalid(&huge).contains("truncated GGUF file"));
    }
}

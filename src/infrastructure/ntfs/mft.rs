//! NTFS Master File Table reader. Parses the boot sector, follows the `$MFT`'s
//! own data runs, and streams every in-use file record as a [`RawRecord`].
//!
//! Layout references are to the standard on-disk NTFS structures: the BPB in
//! the boot sector, the `FILE` record header + update-sequence fixups, and the
//! `$STANDARD_INFORMATION` (0x10), `$FILE_NAME` (0x30) and `$DATA` (0x80)
//! attributes.

use crate::application::{IndexError, IndexResult, RawRecord, VolumeEnumerator};

use super::volume::Volume;

/// One extent of the `$MFT` data stream. `start_lcn == None` marks a sparse run.
#[derive(Debug, Clone, Copy)]
struct Run {
    start_lcn: Option<u64>,
    clusters: u64,
}

/// The `$STANDARD_INFORMATION` timestamps and DOS attributes of one record.
/// All times are Windows FILETIMEs (`0` when unknown).
#[derive(Debug, Clone, Copy, Default)]
struct StdInfo {
    created_ft: u64,
    modified_ft: u64,
    accessed_ft: u64,
    attributes: u32,
}

/// Full per-record metadata read on demand for a single MFT record (used by the
/// USN watcher to enrich live changes, which the journal alone does not carry).
/// The directory state is carried in `attributes` (the `0x10` bit); the watcher
/// keeps the journal's own dir flag for the changed link.
#[derive(Debug, Clone, Copy)]
pub struct RecordMeta {
    /// Size in bytes; `None` for directories or when unknown.
    pub size: Option<u64>,
    pub modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub attributes: u32,
}

/// Reads and enumerates the MFT of a single NTFS volume.
pub struct MftReader {
    volume: Volume,
    drive: char,
    bytes_per_sector: u16,
    bytes_per_cluster: u32,
    record_size: u32,
    record_count: u64,
    runs: Vec<Run>,
}

impl MftReader {
    /// Open `drive` and read enough of the MFT metadata to enumerate it.
    pub fn open(drive: char) -> IndexResult<Self> {
        let volume = Volume::open(drive)?;
        let volume_label = format!("{drive}:");

        let mut boot = [0u8; 512];
        volume.read_at(0, &mut boot)?;
        if &boot[3..11] != b"NTFS    " {
            return Err(IndexError::NotNtfs(volume_label));
        }

        let bytes_per_sector = read_u16(&boot, 0x0B);
        let sectors_per_cluster = boot[0x0D];
        let bytes_per_cluster = bytes_per_sector as u32 * sectors_per_cluster as u32;
        let mft_lcn = read_u64(&boot, 0x30);
        let clusters_per_record = boot[0x40] as i8;
        let record_size = if clusters_per_record >= 0 {
            clusters_per_record as u32 * bytes_per_cluster
        } else {
            1u32 << (-clusters_per_record as u32)
        };

        if record_size == 0 || bytes_per_cluster == 0 {
            return Err(IndexError::Malformed {
                volume: volume_label,
                detail: "zero record or cluster size".into(),
            });
        }

        // Read $MFT's own record (number 0) to learn where the rest of the
        // table lives on disk.
        let mft_offset = mft_lcn * bytes_per_cluster as u64;
        let mut rec0 = vec![0u8; record_size as usize];
        volume.read_at(mft_offset, &mut rec0)?;
        apply_fixup(&mut rec0, bytes_per_sector);

        let (runs, data_size) = parse_mft_data_runs(&rec0, &volume_label)?;
        let record_count = data_size / record_size as u64;

        Ok(Self {
            volume,
            drive,
            bytes_per_sector,
            bytes_per_cluster,
            record_size,
            record_count,
            runs,
        })
    }

    /// The underlying volume handle, shared so the USN watcher can issue journal
    /// control codes without opening a second handle.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Absolute byte offset of MFT record `record_no`, or `None` if it lies past
    /// the table or inside a sparse run (no data on disk).
    fn record_offset(&self, record_no: u64) -> Option<u64> {
        if record_no >= self.record_count {
            return None;
        }
        let rec = self.record_size as u64;
        let cluster = self.bytes_per_cluster as u64;
        let mut acc: u64 = 0;
        for run in &self.runs {
            // Run lengths come straight off disk; a corrupt run could overflow
            // the offset arithmetic. Skip such a run rather than wrapping to a
            // bogus on-disk position.
            let Some(run_bytes) = run.clusters.checked_mul(cluster) else {
                continue;
            };
            let recs_in_run = run_bytes / rec;
            if record_no < acc + recs_in_run {
                let lcn = run.start_lcn?;
                return lcn
                    .checked_mul(cluster)
                    .and_then(|base| base.checked_add((record_no - acc).checked_mul(rec)?));
            }
            acc = acc.saturating_add(recs_in_run);
        }
        None
    }

    /// Read and parse a single in-use record's timestamps, attributes and size.
    /// `expected_seq` is the sequence number from the USN file reference (its high
    /// 16 bits); the read is rejected when the record has since been reused for a
    /// different file, so stale metadata never attaches to the changed entry.
    /// Returns `None` if the record is unreadable, free, reused, or not a `FILE`.
    pub fn read_meta(&self, record_no: u64, expected_seq: u16) -> Option<RecordMeta> {
        let offset = self.record_offset(record_no)?;
        let rec_size = self.record_size as usize;
        // Raw volume reads must be aligned to the logical sector size in both
        // offset and length, so read the sector window that contains the record
        // and slice it back out — an MFT record can be smaller than a 4Kn sector.
        let sector = self.bytes_per_sector as u64;
        let aligned = offset & !(sector - 1);
        let pad = (offset - aligned) as usize;
        let len = (pad + rec_size).next_multiple_of(sector as usize);
        let mut buf = vec![0u8; len];
        self.volume.read_at(aligned, &mut buf).ok()?;

        let record = &mut buf[pad..pad + rec_size];
        apply_fixup(record, self.bytes_per_sector);
        // Sequence number lives in the FILE header at +0x10; a mismatch means the
        // record was recycled between the journal event and this read.
        if read_u16(record, 0x10) != expected_seq {
            return None;
        }
        parse_record_meta(record)
    }
}

impl VolumeEnumerator for MftReader {
    fn drive(&self) -> char {
        self.drive
    }

    fn enumerate(&mut self, sink: &mut dyn FnMut(RawRecord)) -> IndexResult<()> {
        let rec_size = self.record_size as usize;
        let cluster = self.bytes_per_cluster as u64;
        const CHUNK_RECORDS: usize = 1024;
        let chunk_bytes = rec_size * CHUNK_RECORDS;

        let mut buf = vec![0u8; chunk_bytes];
        let mut record_no: u64 = 0;
        // Reused per record so hardlink name collection allocates nothing.
        let mut names: Vec<(u64, String)> = Vec::new();

        for run in &self.runs {
            if record_no >= self.record_count {
                break;
            }
            // Disk-controlled run length; skip a run whose size overflows rather
            // than wrapping to a bogus read position.
            let Some(run_bytes) = run.clusters.checked_mul(cluster) else {
                continue;
            };

            let Some(lcn) = run.start_lcn else {
                // Sparse run: no data on disk, just advance the numbering.
                record_no += run_bytes / rec_size as u64;
                continue;
            };

            let Some(base) = lcn.checked_mul(cluster) else {
                continue;
            };
            let mut pos = 0u64;
            while pos < run_bytes {
                let mut want = (run_bytes - pos).min(chunk_bytes as u64) as usize;
                want -= want % rec_size;
                if want == 0 {
                    break;
                }
                self.volume.read_at(base + pos, &mut buf[..want])?;

                for k in 0..(want / rec_size) {
                    if record_no >= self.record_count {
                        return Ok(());
                    }
                    let rec = &mut buf[k * rec_size..(k + 1) * rec_size];
                    parse_file_record(rec, record_no, self.bytes_per_sector, &mut names, sink);
                    record_no += 1;
                }
                pos += want as u64;
            }
        }
        Ok(())
    }
}

/// Apply the update-sequence fixups: each sector's final two bytes are restored
/// from the update-sequence array referenced in the record header.
fn apply_fixup(rec: &mut [u8], bytes_per_sector: u16) {
    if rec.len() < 8 {
        return;
    }
    let usa_offset = read_u16(rec, 0x04) as usize;
    let usa_count = read_u16(rec, 0x06) as usize;
    let sector = bytes_per_sector as usize;
    if usa_count == 0 || sector == 0 {
        return;
    }
    for i in 1..usa_count {
        let pos = i * sector - 2;
        let fx = usa_offset + i * 2;
        if pos + 2 > rec.len() || fx + 2 > rec.len() {
            break;
        }
        rec[pos] = rec[fx];
        rec[pos + 1] = rec[fx + 1];
    }
}

/// Locate the non-resident, unnamed `$DATA` attribute of the `$MFT` record and
/// decode its run list plus its real (logical) size.
fn parse_mft_data_runs(rec0: &[u8], volume: &str) -> IndexResult<(Vec<Run>, u64)> {
    let malformed = |detail: &str| IndexError::Malformed {
        volume: volume.to_string(),
        detail: detail.to_string(),
    };

    if rec0.len() < 0x18 || &rec0[0..4] != b"FILE" {
        return Err(malformed("$MFT record is not a FILE record"));
    }

    let first_attr = read_u16(rec0, 0x14) as usize;
    let used = (read_u32(rec0, 0x18) as usize).min(rec0.len());
    let mut off = first_attr;

    while off + 8 <= used {
        let atype = read_u32(rec0, off);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let len = read_u32(rec0, off + 4) as usize;
        // See parse_file_record: require a full 16-byte header so the byte reads
        // below cannot index past a record-boundary-straddling attribute.
        if len < 16 || off + len > rec0.len() {
            break;
        }
        let non_resident = rec0[off + 8];
        let name_len = rec0[off + 9];

        if atype == 0x80 && name_len == 0 && non_resident == 1 {
            let runs_off = off + read_u16(rec0, off + 0x20) as usize;
            let real_size = read_u64(rec0, off + 0x30);
            let attr_end = (off + len).min(rec0.len());
            if runs_off < attr_end {
                let runs = decode_runs(&rec0[runs_off..attr_end]);
                if !runs.is_empty() {
                    return Ok((runs, real_size));
                }
            }
        }
        off += len;
    }
    Err(malformed("no non-resident $DATA attribute on $MFT"))
}

/// Decode an NTFS data-run list into absolute runs.
fn decode_runs(bytes: &[u8]) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    let mut prev_lcn: i64 = 0;

    while i < bytes.len() {
        let header = bytes[i];
        if header == 0 {
            break;
        }
        i += 1;
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        if len_bytes == 0 || i + len_bytes + off_bytes > bytes.len() {
            break;
        }

        let mut length: u64 = 0;
        for j in 0..len_bytes {
            length |= (bytes[i + j] as u64) << (8 * j);
        }
        i += len_bytes;

        if off_bytes == 0 {
            runs.push(Run {
                start_lcn: None,
                clusters: length,
            });
        } else {
            let mut offset: i64 = 0;
            for j in 0..off_bytes {
                offset |= (bytes[i + j] as i64) << (8 * j);
            }
            // Sign-extend the variable-width signed offset.
            let shift = 64 - 8 * off_bytes as u32;
            offset = (offset << shift) >> shift;
            prev_lcn += offset;
            runs.push(Run {
                start_lcn: Some(prev_lcn as u64),
                clusters: length,
            });
            i += off_bytes;
            continue;
        }
        i += off_bytes;
    }
    runs
}

/// Parse one MFT record and emit one [`RawRecord`] per real name. A hardlinked
/// file has several `$FILE_NAME` attributes (one per directory it lives in);
/// each is a distinct path that Everything lists separately. DOS 8.3 aliases
/// (namespace 2) are suppressed. `names` is a caller-owned scratch buffer to
/// avoid per-record allocation.
fn parse_file_record(
    rec: &mut [u8],
    record_no: u64,
    bytes_per_sector: u16,
    names: &mut Vec<(u64, String)>,
    sink: &mut dyn FnMut(RawRecord),
) {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return;
    }
    let flags = read_u16(rec, 0x16);
    if flags & 0x01 == 0 {
        return; // not in use
    }
    let is_dir = flags & 0x02 != 0;
    let used = (read_u32(rec, 0x18) as usize).min(rec.len());
    let first_attr = read_u16(rec, 0x14) as usize;

    apply_fixup(rec, bytes_per_sector);

    names.clear();
    let mut size_from_data: Option<u64> = None;
    let mut size_from_name: Option<u64> = None;
    let mut std = StdInfo::default();

    let mut off = first_attr;
    while off + 8 <= used {
        let atype = read_u32(rec, off);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let len = read_u32(rec, off + 4) as usize;
        // Require the full 16-byte resident attribute header before indexing
        // off+8/off+9 below. `len < 16` (not `< 8`) also rejects a header that
        // straddles the record end: a crafted len==8 at off==rec.len()-8 would
        // otherwise pass `off + len > rec.len()` and index one byte past the end.
        if len < 16 || off + len > rec.len() {
            break;
        }
        let non_resident = rec[off + 8];
        let name_len = rec[off + 9];

        match atype {
            0x10 => {
                // $STANDARD_INFORMATION — resident; times + DOS attributes.
                std = read_std_info(rec, off);
            }
            0x30 => {
                // $FILE_NAME — resident. One per hardlink path.
                let content = off + read_u16(rec, off + 0x14) as usize;
                if content + 0x42 <= rec.len() {
                    let parent = read_u64(rec, content) & 0x0000_FFFF_FFFF_FFFF;
                    let real_size = read_u64(rec, content + 0x30);
                    let name_chars = rec[content + 0x40] as usize;
                    let namespace = rec[content + 0x41];
                    let name_start = content + 0x42;
                    // namespace 2 == DOS 8.3 alias; skip it.
                    if namespace != 2 && name_start + name_chars * 2 <= rec.len() {
                        let units: Vec<u16> = (0..name_chars)
                            .map(|j| read_u16(rec, name_start + j * 2))
                            .collect();
                        names.push((parent, String::from_utf16_lossy(&units)));
                        size_from_name = Some(real_size);
                    }
                }
            }
            // $DATA — the unnamed stream gives the file's logical size.
            0x80 if name_len == 0 => {
                size_from_data = Some(data_attr_size(rec, off, non_resident));
            }
            _ => {}
        }
        off += len;
    }

    if names.is_empty() {
        return;
    }
    let size = resolve_size(is_dir, size_from_data, size_from_name);
    for (parent_no, name) in names.drain(..) {
        sink(RawRecord {
            record_no,
            parent_no,
            name,
            is_dir,
            size,
            modified_ft: std.modified_ft,
            created_ft: std.created_ft,
            accessed_ft: std.accessed_ft,
            attributes: normalize_attributes(std.attributes, is_dir),
        });
    }
}

/// Logical size from a `$DATA` (0x80) attribute header: the resident content
/// length at +0x10, or the non-resident real size at +0x30.
fn data_attr_size(rec: &[u8], attr_off: usize, non_resident: u8) -> u64 {
    if non_resident == 0 {
        read_u32(rec, attr_off + 0x10) as u64
    } else {
        read_u64(rec, attr_off + 0x30)
    }
}

/// Resolve a record's file size: directories have none; files prefer the `$DATA`
/// stream size and fall back to the size recorded in `$FILE_NAME`.
fn resolve_size(is_dir: bool, from_data: Option<u64>, from_name: Option<u64>) -> Option<u64> {
    if is_dir {
        None
    } else {
        from_data.or(from_name)
    }
}

/// Reconcile the DOS-attributes directory bit with the authoritative header
/// flag: `$STANDARD_INFORMATION` does not reliably carry `FILE_ATTRIBUTE_
/// DIRECTORY` (0x10), so set it from `is_dir` to match the Win32 view.
fn normalize_attributes(attrs: u32, is_dir: bool) -> u32 {
    const DIRECTORY: u32 = 0x10;
    if is_dir {
        attrs | DIRECTORY
    } else {
        attrs & !DIRECTORY
    }
}

/// Decode the `$STANDARD_INFORMATION` (0x10) attribute at `attr_off`: creation
/// (+0x00), modified (+0x08) and access (+0x18) FILETIMEs, plus DOS attributes
/// (+0x20). The attribute is always resident; its content offset is at +0x14.
fn read_std_info(rec: &[u8], attr_off: usize) -> StdInfo {
    let content = attr_off + read_u16(rec, attr_off + 0x14) as usize;
    StdInfo {
        created_ft: read_u64(rec, content),
        modified_ft: read_u64(rec, content + 0x08),
        accessed_ft: read_u64(rec, content + 0x18),
        attributes: read_u32(rec, content + 0x20),
    }
}

/// Parse a fixed-up single record into [`RecordMeta`], or `None` if it is not an
/// in-use `FILE` record. Mirrors the metadata collection in `parse_file_record`
/// but skips name extraction, which the USN watcher already has.
fn parse_record_meta(rec: &[u8]) -> Option<RecordMeta> {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return None;
    }
    let flags = read_u16(rec, 0x16);
    if flags & 0x01 == 0 {
        return None; // not in use
    }
    let is_dir = flags & 0x02 != 0;
    let used = (read_u32(rec, 0x18) as usize).min(rec.len());
    let first_attr = read_u16(rec, 0x14) as usize;

    let mut std = StdInfo::default();
    let mut size_from_data: Option<u64> = None;
    let mut size_from_name: Option<u64> = None;

    let mut off = first_attr;
    while off + 8 <= used {
        let atype = read_u32(rec, off);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let len = read_u32(rec, off + 4) as usize;
        // Require the full 16-byte resident attribute header before indexing
        // off+8/off+9 below. `len < 16` (not `< 8`) also rejects a header that
        // straddles the record end: a crafted len==8 at off==rec.len()-8 would
        // otherwise pass `off + len > rec.len()` and index one byte past the end.
        if len < 16 || off + len > rec.len() {
            break;
        }
        let non_resident = rec[off + 8];
        let name_len = rec[off + 9];

        match atype {
            0x10 => std = read_std_info(rec, off),
            0x30 => {
                let content = off + read_u16(rec, off + 0x14) as usize;
                if content + 0x38 <= rec.len() {
                    size_from_name = Some(read_u64(rec, content + 0x30));
                }
            }
            0x80 if name_len == 0 => {
                size_from_data = Some(data_attr_size(rec, off, non_resident));
            }
            _ => {}
        }
        off += len;
    }

    let size = resolve_size(is_dir, size_from_data, size_from_name);
    Some(RecordMeta {
        size,
        modified_ft: std.modified_ft,
        created_ft: std.created_ft,
        accessed_ft: std.accessed_ft,
        attributes: normalize_attributes(std.attributes, is_dir),
    })
}

#[inline]
fn read_u16(b: &[u8], off: usize) -> u16 {
    if off + 2 > b.len() {
        return 0;
    }
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn read_u32(b: &[u8], off: usize) -> u32 {
    if off + 4 > b.len() {
        return 0;
    }
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn read_u64(b: &[u8], off: usize) -> u64 {
    if off + 8 > b.len() {
        return 0;
    }
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

#![allow(
    unreachable_pub,
    clippy::unwrap_used
)]

mod ffi;

use crate::ffi::{ReaderRequest, ReaderResponse, RustReaderContext};
use std::ffi::{c_void, CStr};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task;

// SAFETY: SevenZArchive is owned and transferred between threads safely across await points.
// SevenZArchive is NOT Sync because concurrent operations on the stateful C++ archive
// and shared stream reader context would cause data races.
unsafe impl Send for ffi::SevenZArchive {}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub index: u32,
}

struct NativeArchive {
    archive: *mut ffi::SevenZArchive,
    // Context must be kept alive as long as archive uses it
    _context: Box<RustReaderContext>,
}

// SAFETY: NativeArchive uniquely owns the archive handle and context and can be safely transferred between threads.
unsafe impl Send for NativeArchive {}

impl Drop for NativeArchive {
    fn drop(&mut self) {
        if !self.archive.is_null() {
            // SAFETY: archive is non-null, valid, and dropped only when all references have finished.
            unsafe {
                ffi::CloseArchive(self.archive);
            }
        }
    }
}

pub struct SevenZ {
    inner: Arc<Mutex<NativeArchive>>,
}

// SAFETY: SevenZ wraps an Arc<Mutex<NativeArchive>> and can be moved across threads.
// SevenZ does NOT implement Sync because concurrent calls to C++ 7-Zip FFI would cause data races.
unsafe impl Send for SevenZ {}

impl SevenZ {
    pub async fn open<R>(mut reader: R) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
    {
        let (tx, mut rx) =
            mpsc::unbounded_channel::<(ReaderRequest, std::sync::mpsc::Sender<ReaderResponse>)>();

        // Spawn the driver task
        task::spawn(async move {
            while let Some((req, resp_tx)) = rx.recv().await {
                match req {
                    ReaderRequest::Read(size) => {
                        let mut buf = vec![0u8; size as usize];
                        let result = reader.read(&mut buf).await;
                        match result {
                            Ok(n) => {
                                buf.truncate(n);
                                let _ = resp_tx.send(ReaderResponse::Read(buf));
                            }
                            Err(_) => {
                                let _ = resp_tx.send(ReaderResponse::Error);
                            }
                        }
                    }
                    ReaderRequest::Seek(offset, origin) => {
                        let seek_from = match origin {
                            0 => std::io::SeekFrom::Start(offset as u64),
                            1 => std::io::SeekFrom::Current(offset),
                            2 => std::io::SeekFrom::End(offset),
                            _ => {
                                let _ = resp_tx.send(ReaderResponse::Error);
                                continue;
                            }
                        };
                        match reader.seek(seek_from).await {
                            Ok(pos) => {
                                let _ = resp_tx.send(ReaderResponse::Seek(pos));
                            }
                            Err(_) => {
                                let _ = resp_tx.send(ReaderResponse::Error);
                            }
                        }
                    }
                }
            }
        });

        // Create context
        let mut context = Box::new(RustReaderContext { tx });
        let ctx_ptr = (&raw mut *context).cast::<c_void>();
        // Cast to autocxx::c_void pointer type (which wraps system c_void)
        // Since we can't easily construct autocxx::c_void, we assume the FFI function takes *mut autocxx::c_void
        // and we can cast our *mut c_void to it if the layout is compatible.
        // autocxx::c_void is transparently a c_void.
        let ffi_ctx_addr = ctx_ptr as usize;

        // Call OpenArchive in blocking task
        // We cast the result to usize to make it Send-safe to pass back from spawn_blocking
        let archive_addr = task::spawn_blocking(move || {
            // SAFETY: ffi_ctx_addr points to the live Box<RustReaderContext> allocated above.
            unsafe {
                let ptr = ffi::OpenArchive(ffi_ctx_addr as *mut autocxx::c_void);
                ptr as usize
            }
        })
        .await?;

        let archive = archive_addr as *mut ffi::SevenZArchive;

        if archive.is_null() {
            return Err("Failed to open archive".into());
        }

        let native = NativeArchive {
            archive,
            _context: context,
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(native)),
        })
    }

    pub async fn entries(
        &mut self,
    ) -> Result<Vec<ArchiveEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let inner = Arc::clone(&self.inner);

        task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|e| format!("Archive mutex poisoned: {e}"))?;
            let archive = guard.archive;

            // SAFETY: guard holds mutual exclusion on the valid SevenZArchive pointer.
            unsafe {
                let count = ffi::GetArchiveItemCount(archive).0;
                let mut entries = Vec::with_capacity(count as usize);

                for i in 0..count {
                    let name_ptr = ffi::GetArchiveItemName(archive, autocxx::c_uint(i));
                    let name = if name_ptr.is_null() {
                        format!("{i}")
                    } else {
                        let c_str = CStr::from_ptr(name_ptr);
                        let s = c_str.to_string_lossy().to_string();
                        ffi::FreeString(name_ptr);
                        s
                    };

                    let is_dir =
                        ffi::IsArchiveItemFolder(archive, autocxx::c_uint(i)) != autocxx::c_int(0);
                    let size = ffi::GetArchiveItemSize(archive, autocxx::c_uint(i)).0;

                    entries.push(ArchiveEntry {
                        name,
                        size,
                        is_dir,
                        index: i,
                    });
                }
                Ok(entries)
            }
        })
        .await?
    }

    pub async fn get_entry_index(
        &mut self,
        name: &str,
    ) -> Result<Option<u32>, Box<dyn std::error::Error + Send + Sync>> {
        let inner = Arc::clone(&self.inner);
        let name_c = std::ffi::CString::new(name)?;

        task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|e| format!("Archive mutex poisoned: {e}"))?;
            let archive = guard.archive;

            // SAFETY: archive is valid and name_c is a valid null-terminated C string.
            unsafe {
                let idx = ffi::GetArchiveItemIndex(archive, name_c.as_ptr());
                let idx_val = idx.0;
                if idx_val >= 0 {
                    Ok(Some(idx_val as u32))
                } else {
                    Ok(None)
                }
            }
        })
        .await?
    }

    /// Get the uncompressed size of an entry by its index
    pub async fn get_entry_size(
        &mut self,
        index: u32,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let inner = Arc::clone(&self.inner);

        task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|e| format!("Archive mutex poisoned: {e}"))?;
            let archive = guard.archive;

            // SAFETY: archive is valid under mutex guard.
            unsafe {
                let size = ffi::GetArchiveItemSize(archive, autocxx::c_uint(index));
                Ok(size.0)
            }
        })
        .await?
    }
}

// Structs must be outside impl blocks
struct RustWriterContext {
    writer: Box<dyn std::io::Write + Send>,
}

#[unsafe(no_mangle)]
/// Writes a buffer supplied by the native 7-Zip callback into the Rust writer.
///
/// # Safety
///
/// `ctx` must point to a live [`RustWriterContext`], `buf` must be readable for
/// `size` bytes, and a non-null `processed` pointer must be writable.
pub unsafe extern "C" fn rust_write_cb(
    ctx: *mut c_void,
    buf: *const c_void,
    size: autocxx::c_uint,
    processed: *mut autocxx::c_uint,
) -> autocxx::c_int {
    // SAFETY: Caller contract requires ctx points to a live RustWriterContext and buf has size bytes.
    unsafe {
        let ctx = &mut *ctx.cast::<RustWriterContext>();
        let buf_slice = std::slice::from_raw_parts(buf.cast::<u8>(), size.0 as usize);
        match ctx.writer.write_all(buf_slice) {
            Ok(()) => {
                if !processed.is_null() {
                    *processed = size;
                }
                autocxx::c_int(0)
            }
            Err(_) => autocxx::c_int(1),
        }
    }
}

impl SevenZ {
    pub async fn extract<W>(
        &mut self,
        index: u32,
        writer: W,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: std::io::Write + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);

        task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|e| format!("Archive mutex poisoned: {e}"))?;
            let archive = guard.archive;

            let mut context = RustWriterContext {
                writer: Box::new(writer),
            };
            let callback_ctx = (&raw mut context).cast::<autocxx::c_void>();

            // SAFETY: archive is valid and callback_ctx points to stack-allocated RustWriterContext during call.
            let res = unsafe {
                ffi::ExtractEntry(
                    archive,
                    autocxx::c_uint(index),
                    callback_ctx,
                )
            };

            if res == autocxx::c_int(0) {
                Ok(())
            } else {
                Err(format!("Extraction failed with code {res:?}").into())
            }
        })
        .await?
    }
}

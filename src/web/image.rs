//! The two halves every image upload shares, whatever the picture hangs on
//! (exam question, bank template, pool question, solution, answer sheet):
//! reading the `file` part under the school's size limit and the raster
//! allowlist, and the blob-first write dance — fresh blob to disk, then the
//! row, then the replaced blob off disk, with any row failure taking the fresh
//! blob back out. A stored row therefore always points at a real blob, and a
//! failed write strands nothing.
//!
//! Gates stay at the call sites (the exam freeze, the pool's approval, owner
//! and author rules), as do the row types: this module owns only the
//! format/limit rule and the rollback, once each.

use std::future::Future;

use axum::extract::Multipart;

use crate::domain::note_file::FileContentType;
use crate::domain::settings::Settings;
use crate::error::AppError;
use crate::state::AppState;

use super::{blob_path, image_content_type, read_upload, remove_blob};

/// A validated image upload: an allowlisted raster content type and the bytes.
pub(crate) struct ImageUpload {
    pub content_type: FileContentType,
    pub data: Vec<u8>,
}

impl ImageUpload {
    pub(crate) fn size(&self) -> i64 {
        self.data.len() as i64
    }
}

/// The multipart `file` part, held to the school's `max_file_bytes` and the
/// raster allowlist — the one place either rule is spelled, so a format or
/// limit change lands on every image endpoint at once.
pub(crate) async fn read_image_upload(
    st: &AppState,
    multipart: &mut Multipart,
) -> Result<ImageUpload, AppError> {
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    Ok(ImageUpload {
        content_type,
        data: upload.data,
    })
}

/// Blob first, row second: `data` lands on disk under `file`, then `write_row`
/// runs. It returns what it stored plus the blob name it replaced (if any),
/// which comes off disk only once the row is safely written; any error takes
/// the fresh blob back out instead.
pub(crate) async fn store_blob<T, Fut>(
    st: &AppState,
    file: &str,
    data: &[u8],
    write_row: impl FnOnce() -> Fut,
) -> Result<T, AppError>
where
    Fut: Future<Output = Result<(T, Option<String>), AppError>>,
{
    crate::web::ensure_files_dir(&st.files_path).await?;
    let path = blob_path(&st.files_path, file);
    tokio::fs::write(&path, data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the image blob: {err}")))?;
    match write_row().await {
        Ok((stored, replaced)) => {
            if let Some(replaced) = replaced {
                remove_blob(&st.files_path, &replaced).await;
            }
            Ok(stored)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

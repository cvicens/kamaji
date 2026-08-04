use std::time::Duration;

use kamaji_core::attachment::ResolvedAttachment;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::OwnedMxcUri;
use matrix_sdk::Client as MatrixClient;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::FileId;

use crate::error::AttachmentError;

/// Resolves a Telegram `file_id` to a download path via `getFile`, then
/// downloads the bytes. Bounded by both `timeout` (per Telegram API call --
/// the "timeout every external call" convention) and `max_bytes` (a
/// defensive cap independent of Telegram's own ~20MB bot-download limit).
pub async fn download_attachment(
    bot: &Bot,
    file_id: &str,
    file_name: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<ResolvedAttachment, AttachmentError> {
    let file = tokio::time::timeout(timeout, bot.get_file(FileId(file_id.to_string())))
        .await
        .map_err(|_| AttachmentError::Timeout)?
        .map_err(AttachmentError::GetFile)?;

    if file.size as usize > max_bytes {
        return Err(AttachmentError::TooLarge {
            size: file.size as usize,
            max: max_bytes,
        });
    }

    let mut bytes = Vec::new();
    tokio::time::timeout(timeout, bot.download_file(&file.path, &mut bytes))
        .await
        .map_err(|_| AttachmentError::Timeout)?
        .map_err(AttachmentError::Download)?;

    Ok(ResolvedAttachment {
        file_name: file_name.to_string(),
        bytes,
    })
}

/// Matrix's media API is a single content-fetch given an `mxc://` URI --
/// unlike Telegram's `getFile`-then-download, there's no separate metadata
/// call that returns a size up front, so the `max_bytes` cap is enforced
/// after the download completes rather than before. Still a defensive cap,
/// not a hard protocol limit, so checking it late is acceptable here.
pub async fn download_matrix_attachment(
    client: &MatrixClient,
    mxc_uri: &str,
    file_name: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<ResolvedAttachment, AttachmentError> {
    let source = MediaSource::Plain(OwnedMxcUri::from(mxc_uri.to_string()));
    let request = MediaRequestParameters {
        source,
        format: MediaFormat::File,
    };

    let bytes = tokio::time::timeout(timeout, client.media().get_media_content(&request, false))
        .await
        .map_err(|_| AttachmentError::Timeout)?
        .map_err(AttachmentError::MatrixMedia)?;

    if bytes.len() > max_bytes {
        return Err(AttachmentError::TooLarge {
            size: bytes.len(),
            max: max_bytes,
        });
    }

    Ok(ResolvedAttachment {
        file_name: file_name.to_string(),
        bytes,
    })
}

/// Bytes for a `/fact` attachment that have already been resolved by the
/// caller -- a platform-specific download in `kamajid` (Telegram's
/// `file_id`/Matrix's `mxc://` URI), or bytes read directly from a local
/// file by the `kamaji` CLI. Core has no platform client to do this
/// resolution itself, so `worker::process_fact_command` only ever sees the
/// outcome, never a `file_id` reference.
pub struct ResolvedAttachment {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

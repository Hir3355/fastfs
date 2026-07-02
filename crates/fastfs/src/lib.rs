mod cancellation;
mod commands;
mod native_matcher;
mod native_scanner;
mod native_walker;
mod search;
mod search_cache;
mod search_platform;
mod wire;

use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;
use std::str;

use cancellation::{CancellationState, FastFsCancellationToken};
use wire::Emitter;

pub type EventCallback = extern "system" fn(*const u8, usize, *mut c_void) -> i32;

struct Request {
    command: String,
    args: Vec<String>,
}

fn execute(
    request_bytes: &[u8],
    callback: EventCallback,
    context: *mut c_void,
    cancellation: Option<CancellationState>,
) -> i32 {
    let mut emitter = Emitter::new(callback, context, cancellation);
    if emitter.is_stopped() {
        return 130;
    }
    let request = match parse_request(request_bytes) {
        Ok(request) => request,
        Err(error) => {
            emitter.error(
                "InvalidRequest",
                "InvalidArgument",
                format!("要求の解析に失敗しました: {error}"),
                None,
            );
            emitter.finish();
            return 2;
        }
    };

    match request.command.as_str() {
        "ls" => commands::ls(&request.args, &mut emitter),
        "touch" => commands::touch(&request.args, &mut emitter),
        "find" => commands::find(&request.args, &mut emitter),
        "cat" => commands::cat(&request.args, &mut emitter),
        "sed" => commands::sed(&request.args, &mut emitter),
        "rg" => search::rg(&request.args, &mut emitter),
        command => emitter.error(
            "UnknownCommand",
            "InvalidArgument",
            format!("未対応のコマンドです: {command}"),
            None,
        ),
    }

    emitter.finish();

    if emitter.stopped {
        130
    } else if emitter.error_count > 0 {
        1
    } else {
        0
    }
}

fn parse_request(bytes: &[u8]) -> Result<Request, String> {
    if bytes.len() < 8 || &bytes[..4] != b"FFS1" {
        return Err("バイナリ要求ヘッダーが不正です".to_owned());
    }
    let mut offset = 4;
    let field_count = read_u32(bytes, &mut offset)? as usize;
    let maximum_fields = bytes.len().saturating_sub(8) / 4;
    if field_count == 0 || field_count > 1_000_000 || field_count > maximum_fields {
        return Err("要求のフィールド数が不正です".to_owned());
    }

    let command = read_string(bytes, &mut offset)?;
    let mut args = Vec::with_capacity(field_count - 1);
    for _ in 1..field_count {
        args.push(read_string(bytes, &mut offset)?);
    }
    if offset != bytes.len() {
        return Err("要求の末尾に余分なデータがあります".to_owned());
    }

    Ok(Request { command, args })
}

fn read_string(bytes: &[u8], offset: &mut usize) -> Result<String, String> {
    let length = read_u32(bytes, offset)? as usize;
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "要求の文字列長が不正です".to_owned())?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| "要求が途中で切れています".to_owned())?;
    *offset = end;
    Ok(str::from_utf8(value)
        .map_err(|error| format!("要求が UTF-8 ではありません: {error}"))?
        .to_owned())
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "要求位置が不正です".to_owned())?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| "要求が途中で切れています".to_owned())?;
    *offset = end;
    Ok(u32::from_le_bytes(
        value.try_into().expect("slice length was validated"),
    ))
}

/// バイナリ形式の要求を実行し、結果をコールバックへ一括送信します。
///
/// # Safety
///
/// `request` は `request_len` バイト読み取り可能でなければなりません。
/// `callback` はこの関数の実行中、有効でなければなりません。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn fastfs_execute_v2(
    request: *const u8,
    request_len: usize,
    callback: Option<EventCallback>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: This forwards the caller's pointer guarantees unchanged.
    unsafe { execute_ffi(request, request_len, callback, context, None) }
}

/// Executes a request with cooperative cancellation support.
///
/// # Safety
///
/// `request` must be readable for `request_len` bytes. `callback` and a
/// non-null `cancellation` token must remain valid until this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn fastfs_execute_v3(
    request: *const u8,
    request_len: usize,
    callback: Option<EventCallback>,
    context: *mut c_void,
    cancellation: *const FastFsCancellationToken,
) -> i32 {
    let cancellation = if cancellation.is_null() {
        None
    } else {
        // SAFETY: The caller keeps the opaque token alive for the execution.
        Some(unsafe { &*cancellation }.state())
    };
    // SAFETY: This forwards the caller's pointer guarantees unchanged.
    unsafe { execute_ffi(request, request_len, callback, context, cancellation) }
}

unsafe fn execute_ffi(
    request: *const u8,
    request_len: usize,
    callback: Option<EventCallback>,
    context: *mut c_void,
    cancellation: Option<CancellationState>,
) -> i32 {
    let Some(callback) = callback else {
        return 2;
    };
    if request.is_null() && request_len != 0 {
        return 2;
    }

    let panic_cancellation = cancellation.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let bytes = if request_len == 0 {
            &[]
        } else {
            // SAFETY: The caller guarantees that the pointer is readable for request_len bytes.
            unsafe { slice::from_raw_parts(request, request_len) }
        };
        execute(bytes, callback, context, cancellation)
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            let mut emitter = Emitter::new(callback, context, panic_cancellation);
            emitter.error(
                "NativePanic",
                "NotSpecified",
                "Rust ライブラリ内で予期しないエラーが発生しました",
                None,
            );
            emitter.finish();
            3
        }
    }
}

static VERSION: &[u8] = b"0.6.2\0";

#[unsafe(no_mangle)]
pub extern "system" fn fastfs_abi_version() -> u32 {
    3
}

#[unsafe(no_mangle)]
pub extern "system" fn fastfs_version() -> *const c_char {
    VERSION.as_ptr().cast()
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use crate::cancellation::{
        fastfs_cancellation_cancel, fastfs_cancellation_create, fastfs_cancellation_destroy,
    };

    use super::{fastfs_execute_v3, parse_request};

    extern "system" fn count_callback(
        _data: *const u8,
        _length: usize,
        context: *mut c_void,
    ) -> i32 {
        // SAFETY: The test supplies a live usize for the duration of the call.
        unsafe { *context.cast::<usize>() += 1 };
        0
    }

    #[test]
    fn parses_binary_request() {
        let mut bytes = b"FFS1".to_vec();
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        for value in ["find", ".", "*.txt"] {
            bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }

        let request = parse_request(&bytes).expect("request should parse");
        assert_eq!(request.command, "find");
        assert_eq!(request.args, [".", "*.txt"]);
    }

    #[test]
    fn rejects_truncated_binary_request() {
        let mut bytes = b"FFS1".to_vec();
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&10_u32.to_le_bytes());
        bytes.extend_from_slice(b"cat");
        assert!(parse_request(&bytes).is_err());
    }

    #[test]
    fn v3_execution_observes_a_pre_cancelled_token() {
        let token = fastfs_cancellation_create();
        let mut callback_count = 0_usize;
        let context = (&mut callback_count as *mut usize).cast::<c_void>();

        // SAFETY: The token remains live through execution.
        unsafe { fastfs_cancellation_cancel(token) };
        // SAFETY: A zero-length request may use a null pointer, and all other pointers are live.
        let result =
            unsafe { fastfs_execute_v3(std::ptr::null(), 0, Some(count_callback), context, token) };
        assert_eq!(result, 130);
        assert_eq!(callback_count, 0);

        // SAFETY: This is the token's only destruction.
        unsafe { fastfs_cancellation_destroy(token) };
    }
}

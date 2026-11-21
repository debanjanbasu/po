use js_sys::{Array, Function, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, BlobPropertyBag, File, FilePropertyBag, HtmlDocument, HtmlElement, HtmlTextAreaElement,
    ShareData, Url, Window, window,
};

use crate::storage::background_errors::report_nonfatal;

use super::{DownloadResult, ShareError, ShareResult};

fn browser_error(context: &'static str, error: &JsValue) -> ShareError {
    let name = Reflect::get(error, &JsValue::from_str("name"))
        .ok()
        .and_then(|name| name.as_string());
    match name.as_deref() {
        Some("AbortError") => ShareError::Cancelled,
        Some("NotAllowedError" | "SecurityError") => ShareError::Denied,
        Some("NotSupportedError") => ShareError::Unsupported,
        _ => {
            report_nonfatal("platform.share", context, format!("{error:?}"));
            ShareError::Failed
        }
    }
}

fn optional_method(receiver: &JsValue, name: &str) -> Result<Option<Function>, ShareError> {
    let method = Reflect::get(receiver, &JsValue::from_str(name))
        .map_err(|error| browser_error("Browser capability lookup failed", &error))?;
    Ok(method.dyn_into::<Function>().ok())
}

async fn call_promise(
    method: &Function,
    receiver: &JsValue,
    argument: &JsValue,
) -> Result<(), ShareError> {
    let promise = method
        .call1(receiver, argument)
        .map_err(|error| browser_error("Browser action failed to start", &error))?
        .dyn_into::<Promise>()
        .map_err(|error| browser_error("Browser action did not return a promise", &error))?;
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|error| browser_error("Browser action was rejected", &error))
}

/// Copies text only after an API confirms success. Missing Clipboard API
/// support permits the legacy fallback; denial or cancellation does not.
pub async fn copy_to_clipboard(text: &str) -> Result<(), ShareError> {
    let window = window().ok_or(ShareError::Unsupported)?;
    if window.is_secure_context() {
        let clipboard = Reflect::get(&window.navigator(), &JsValue::from_str("clipboard"))
            .map_err(|error| browser_error("Clipboard lookup failed", &error))?;
        if !clipboard.is_null()
            && !clipboard.is_undefined()
            && let Some(write_text) = optional_method(&clipboard, "writeText")?
        {
            match call_promise(&write_text, &clipboard, &JsValue::from_str(text)).await {
                Err(ShareError::Unsupported) => {}
                result => return result,
            }
        }
    }
    copy_legacy(text, &window)
}

fn copy_legacy(text: &str, window: &Window) -> Result<(), ShareError> {
    let document = window.document().ok_or(ShareError::Unsupported)?;
    let html_document = document
        .dyn_ref::<HtmlDocument>()
        .ok_or(ShareError::Unsupported)?;
    if optional_method(&document, "execCommand")?.is_none() {
        return Err(ShareError::Unsupported);
    }
    let body = document.body().ok_or(ShareError::Unsupported)?;
    let focused = document.active_element();
    let textarea = document
        .create_element("textarea")
        .map_err(|error| browser_error("Clipboard input creation failed", &error))?
        .dyn_into::<HtmlTextAreaElement>()
        .map_err(|error| browser_error("Clipboard input is not a textarea", &error))?;
    textarea.set_value(text);
    textarea
        .set_attribute("style", "position:fixed;opacity:0")
        .map_err(|error| browser_error("Clipboard input setup failed", &error))?;
    body.append_child(&textarea)
        .map_err(|error| browser_error("Clipboard input insertion failed", &error))?;
    textarea.select();
    let result = html_document
        .exec_command("copy")
        .map_err(|error| browser_error("Legacy clipboard copy failed", &error))
        .and_then(|copied| {
            if copied {
                Ok(())
            } else {
                report_nonfatal(
                    "platform.share",
                    "Legacy clipboard copy failed",
                    "execCommand returned false",
                );
                Err(ShareError::Failed)
            }
        });
    textarea.remove();
    if let Some(focused) = focused.and_then(|element| element.dyn_into::<HtmlElement>().ok())
        && let Err(error) = focused.focus()
    {
        report_nonfatal(
            "platform.share",
            "Clipboard focus restoration failed",
            format!("{error:?}"),
        );
    }
    result
}

fn byte_blob(bytes: &[u8], mime: &str) -> Result<Blob, ShareError> {
    let len = u32::try_from(bytes.len()).map_err(|error| {
        report_nonfatal(
            "platform.share",
            "Byte payload exceeds browser limits",
            error,
        );
        ShareError::Failed
    })?;
    let array = Uint8Array::new_with_length(len);
    array.copy_from(bytes);
    let parts = Array::of1(&array.buffer());
    let opts = BlobPropertyBag::new();
    opts.set_type(mime);
    Blob::new_with_buffer_source_sequence_and_options(&parts, &opts)
        .map_err(|error| browser_error("Blob creation failed", &error))
}

fn revoke_url(url: &str) {
    if let Err(error) = Url::revoke_object_url(url) {
        report_nonfatal(
            "platform.share",
            "Download URL cleanup failed",
            format!("{error:?}"),
        );
    }
}

fn download_blob(blob: &Blob, filename: &str, window: &Window) -> Result<(), ShareError> {
    let document = window.document().ok_or(ShareError::Unsupported)?;
    let body = document.body().ok_or(ShareError::Unsupported)?;
    let anchor = document
        .create_element("a")
        .map_err(|error| browser_error("Download link creation failed", &error))?
        .dyn_into::<HtmlElement>()
        .map_err(|error| browser_error("Download link is not an HTML element", &error))?;
    anchor
        .set_attribute("download", filename)
        .map_err(|error| browser_error("Download filename setup failed", &error))?;
    let url = Url::create_object_url_with_blob(blob)
        .map_err(|error| browser_error("Download URL creation failed", &error))?;
    let setup = anchor
        .set_attribute("href", &url)
        .and_then(|()| body.append_child(&anchor).map(|_| ()));
    if let Err(error) = setup {
        revoke_url(&url);
        return Err(browser_error("Download link setup failed", &error));
    }
    anchor.click();
    anchor.remove();
    // No completion signal exists for a blob download, so the revoke is a
    // timeout — and a timeout is a *minimum*: a main thread busy with the inline
    // CPU work this app loads it with (see `docs/concurrency.md`) delays the
    // revoke rather than pulling it ahead of the download the click started, so
    // the wait can only err toward holding the blob longer, never toward
    // revoking before the browser has read it.
    //
    // `once_into_js` hands the closure to JS and frees it when the timeout
    // fires. The previous `Closure::once(..).forget()` freed nothing, on any
    // path: every export leaked a JS function plus the URL string it captured.
    let cleanup_url = url.clone();
    let callback: Function =
        Closure::once_into_js(move || revoke_url(&cleanup_url)).unchecked_into();
    if let Err(error) = window.set_timeout_with_callback_and_timeout_and_arguments_0(&callback, 100)
    {
        revoke_url(&url);
        report_nonfatal(
            "platform.share",
            "Download cleanup timer failed",
            format!("{error:?}"),
        );
    }
    Ok(())
}

/// Starts a browser download; completion or cancellation is not observable.
pub fn download_bytes(
    bytes: &[u8],
    filename: &str,
    mime: &str,
) -> Result<DownloadResult, ShareError> {
    let window = window().ok_or(ShareError::Unsupported)?;
    download_blob(&byte_blob(bytes, mime)?, filename, &window)?;
    Ok(DownloadResult::Triggered)
}

async fn share_data(data: &ShareData, has_files: bool) -> Result<(), ShareError> {
    let window = window().ok_or(ShareError::Unsupported)?;
    if !window.is_secure_context() {
        return Err(ShareError::Unsupported);
    }
    let navigator = window.navigator();
    let share = optional_method(&navigator, "share")?.ok_or(ShareError::Unsupported)?;
    match optional_method(&navigator, "canShare")? {
        Some(can_share) => {
            let supported = can_share
                .call1(&navigator, data.as_ref())
                .map_err(|error| browser_error("Share capability check failed", &error))?
                .as_bool()
                .ok_or_else(|| {
                    report_nonfatal(
                        "platform.share",
                        "Share capability check failed",
                        "canShare did not return a boolean",
                    );
                    ShareError::Failed
                })?;
            if !supported {
                return Err(ShareError::Unsupported);
            }
        }
        None if has_files => return Err(ShareError::Unsupported),
        None => {}
    }
    call_promise(&share, &navigator, data).await
}

async fn share_text(text: &str) -> Result<(), ShareError> {
    let data = ShareData::new();
    data.set_text(text);
    data.set_title("po");
    share_data(&data, false).await
}

/// Falls back only for unavailable capabilities. Rejection, denial, and
/// cancellation never silently start another share or copy operation.
pub async fn share_text_and_image(
    text: &str,
    image_bytes: &[u8],
) -> Result<ShareResult, ShareError> {
    let blob = byte_blob(image_bytes, "image/webp")?;
    let file_parts = Array::of1(&blob);
    let file_opts = FilePropertyBag::new();
    file_opts.set_type("image/webp");
    let file = File::new_with_blob_sequence_and_options(&file_parts, "oo-qr.webp", &file_opts)
        .map_err(|error| browser_error("Share image creation failed", &error))?;
    let data = ShareData::new();
    data.set_text(text);
    data.set_title("po");
    data.set_files(&Array::of1(&file));
    match share_data(&data, true).await {
        Ok(()) => return Ok(ShareResult::Shared),
        Err(ShareError::Unsupported) => {}
        Err(error) => return Err(error),
    }
    match share_text(text).await {
        Ok(()) => Ok(ShareResult::TextOnly),
        Err(ShareError::Unsupported) => copy_to_clipboard(text)
            .await
            .map(|()| ShareResult::ClipboardFallback),
        Err(error) => Err(error),
    }
}

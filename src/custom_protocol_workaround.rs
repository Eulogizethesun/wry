//! - WebView2 supports non-standard protocols only on Windows 10+, so we have to use a workaround.
//!   See <https://github.com/MicrosoftEdge/WebView2Feedback/issues/73>
//! - On Android, there's no API for registering custom protocols, so this workaround is also used.
//!
//! The process looks like this:
//!
//! 1. Use [`apply_uri_work_around`] to convert the URI we want to navigate to
//! 2. Intercept http(s) requests, test the request URI against [`is_work_around_uri`],
//!    if it matches, we apply [`revert_uri_work_around`] to the URI and feed it to the custom protocol handler

/// If the URI is a work around URI for this protocol which starts with
/// `{http_or_https}://{protocol}.localhost` (the host is always `localhost`, as produced by
/// [`apply_uri_work_around`]). External URLs like `https://tauri.com/page` must NOT match.
pub fn is_work_around_uri(uri: &str, http_or_https: &str, protocol: &str) -> bool {
  uri
    .strip_prefix(http_or_https)
    .and_then(|rest| rest.strip_prefix("://"))
    .and_then(|rest| rest.strip_prefix(protocol))
    .and_then(|rest| rest.strip_prefix("."))
    .and_then(|rest| rest.strip_prefix("localhost"))
    .is_some()
}

/// Conveting `{protocol}://localhost/abc` to `{http_or_https}://{protocol}.localhost/abc`
pub fn apply_uri_work_around(uri: &str, http_or_https: &str, protocol: &str) -> String {
  uri.replace(
    &original_uri_prefix(protocol),
    &work_around_uri_prefix(http_or_https, protocol),
  )
}

/// Conveting `{http_or_https}://{protocol}.localhost/abc` back to `{protocol}://localhost/abc`
pub fn revert_uri_work_around(uri: &str, http_or_https: &str, protocol: &str) -> String {
  uri.replace(
    &work_around_uri_prefix(http_or_https, protocol),
    &original_uri_prefix(protocol),
  )
}

pub fn original_uri_prefix(protocol: &str) -> String {
  format!("{protocol}://")
}

pub fn work_around_uri_prefix(http_or_https: &str, protocol: &str) -> String {
  format!("{http_or_https}://{protocol}.")
}

#[cfg(test)]
mod tests {
  use super::{apply_uri_work_around, is_work_around_uri, revert_uri_work_around};

  #[test]
  fn checks_if_custom_protocol_uri() {
    let scheme = "http";
    let uri = "http://wry.localhost/path/to/page";
    assert!(is_work_around_uri(uri, scheme, "wry"));
    assert!(!is_work_around_uri(uri, scheme, "asset"));
  }

  #[test]
  fn https_work_around_roundtrip() {
    // OHOS uses https scheme for secure-context support
    let original = "tauri://localhost/index.html";
    let worked = apply_uri_work_around(original, "https", "tauri");
    assert_eq!(worked, "https://tauri.localhost/index.html");
    assert!(is_work_around_uri(&worked, "https", "tauri"));
    let reverted = revert_uri_work_around(&worked, "https", "tauri");
    assert_eq!(reverted, original);
  }

  #[test]
  fn https_work_around_does_not_match_external() {
    // External https URLs must NOT be matched (so onInterceptRequest returns null)
    assert!(!is_work_around_uri("https://example.com/page", "https", "tauri"));
    assert!(!is_work_around_uri("https://tauri.com/page", "https", "tauri"));
    // Only https://tauri.localhost/... matches
    assert!(is_work_around_uri("https://tauri.localhost/page", "https", "tauri"));
  }
}

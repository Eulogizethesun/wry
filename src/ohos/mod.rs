use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::WebViewAttributes;
use crate::{Error, PageLoadEvent, Rect, RequestAsyncResponder, Result, RGBA};
use cookie::Cookie;
use http::{Request, Response, Uri};
pub use openharmony_ability::PdfConfig;
use openharmony_ability::{
  native_web::WebProxyBuilder, Either, WebViewBuilder, WebViewStyle, Webview,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Re-export of `openharmony_ability::Webview` for use in wry's public API.
/// Provides access to the OHOS webview handle (e.g., for `web_page_snapshot`).
///
/// Note: This intentionally exposes the full upstream API surface, consistent
/// with other platforms (macOS `WryWebView`, Windows `ICoreWebView2Controller`,
/// Android `JniHandle`). Users are responsible for correct usage of the
/// underlying OHOS webview APIs.
pub type OhosWebviewHandle = Webview;

use crate::util::Counter;

static COUNTER: Counter = Counter::new();

pub struct InnerWebView {
  id: String,
  pub(crate) webview: Webview,
  page_loaded: Arc<AtomicBool>,
  bounds_cache: Mutex<Rect>,
  is_child: bool,
  disposed: AtomicBool,
}

impl InnerWebView {
  pub(crate) fn new_as_child(
    _window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(_window, attributes, pl_attrs, true)
  }

  pub(crate) fn new(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(window, attributes, pl_attrs, false)
  }

  fn new_inner(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
    is_child: bool,
  ) -> Result<Self> {
    let WebViewAttributes {
      id,
      ref url,
      html,
      initialization_scripts,
      ipc_handler,
      #[cfg(any(debug_assertions, feature = "devtools"))]
      devtools,
      custom_protocols,
      background_color,
      transparent,
      headers,
      autoplay,
      user_agent,
      javascript_disabled,
      navigation_handler,
      document_title_changed_handler,
      on_page_load_handler,
      new_window_req_handler,
      download_started_handler,
      download_completed_handler,
      bounds,
      ..
    } = attributes;

    let initial_url = url.clone();
    let initial_headers = headers;

    let id = id
      .map(|id| id.to_string())
      .unwrap_or_else(|| COUNTER.next().to_string());

    let background_color = background_color
      .map(|c| ((c.3 as u32) << 24) | ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | (c.2 as u32));

    // Get window_id from platform-specific attributes
    let window_id = pl_attrs.window_id.unwrap_or(0);

    let merged_scripts = initialization_scripts
      .iter()
      .map(|s| s.script.clone())
      .collect::<Vec<_>>()
      .join("\n");

    let initial_bounds = bounds.unwrap_or_default();

    let (style_x, style_y, style_width, style_height) = if is_child && bounds.is_some() {
      let (x, y) = initial_bounds.position.to_logical::<f64>(1.0).into();
      let (w, h) = initial_bounds.size.to_logical::<f64>(1.0).into();
      (
        Some(Either::A(x)),
        Some(Either::A(y)),
        Some(Either::A(w)),
        Some(Either::A(h)),
      )
    } else {
      (None, None, None, None)
    };

    let mut webview_builder = WebViewBuilder::new()
      .id(id.clone())
      .style(WebViewStyle {
        x: style_x,
        y: style_y,
        width: style_width,
        height: style_height,
        visible: None,
        background_color,
      })
      .javascript_enabled(!javascript_disabled)
      .autoplay(autoplay)
      .initialization_scripts(vec![merged_scripts])
      .transparent(transparent)
      .window_id(window_id);

    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
      webview_builder = webview_builder.devtools(devtools);
    }

    if let Some(html) = html {
      webview_builder = webview_builder.html(html);
    } else if let Some(url) = initial_url {
      webview_builder = webview_builder.url(url);
      if let Some(headers) = initial_headers {
        webview_builder = webview_builder.headers(headers);
      }
    }

    if let Some(user_agent) = user_agent {
      webview_builder = webview_builder.user_agent(user_agent);
    }

    // Wire navigation handler (on_navigation → WebViewBuilder::on_navigation_request)
    if let Some(navigation_handler) = navigation_handler {
      webview_builder = webview_builder
        .on_navigation_request(move |url: String| -> bool { navigation_handler(url) });
    }

    // Wire document title changed handler (on_document_title_changed → WebViewBuilder::on_title_change)
    if let Some(document_title_changed_handler) = document_title_changed_handler {
      webview_builder =
        webview_builder.on_title_change(move |title: String| document_title_changed_handler(title));
    }

    // Wire download started handler (FnMut wrapped in Mutex to satisfy Fn bound)
    if let Some(download_started_handler) = download_started_handler {
      let handler = Mutex::new(download_started_handler);
      webview_builder =
        webview_builder.on_download_start(move |url: String, path: &mut PathBuf| -> bool {
          let mut handler = handler.lock().unwrap();
          handler(url, path)
        });
    }

    // Wire download completed handler (Rc<dyn Fn> cloned into closure)
    if let Some(download_completed_handler) = download_completed_handler {
      webview_builder = webview_builder.on_download_end(
        move |url: String, path: Option<PathBuf>, success: bool| {
          download_completed_handler(url, path, success)
        },
      );
    }

    // Track page loaded state for create_pdf readiness check
    let page_loaded = Arc::new(AtomicBool::new(false));
    let page_loaded_begin = page_loaded.clone();
    let page_loaded_end = page_loaded.clone();

    // Wire on_page_load handler (pre-build: onPageBegin/onPageEnd in ArkTS Web component)
    if let Some(on_page_load_handler) = on_page_load_handler {
      let handler = Arc::new(on_page_load_handler);
      let handler_begin = handler.clone();
      let handler_end = handler.clone();
      webview_builder = webview_builder.on_page_begin(move |url: String| {
        page_loaded_begin.store(false, Ordering::SeqCst);
        handler_begin(PageLoadEvent::Started, url);
      });
      webview_builder = webview_builder.on_page_end(move |url: String| {
        page_loaded_end.store(true, Ordering::SeqCst);
        handler_end(PageLoadEvent::Finished, url);
      });
    } else {
      webview_builder = webview_builder.on_page_begin(move |_: String| {
        page_loaded_begin.store(false, Ordering::SeqCst);
      });
      webview_builder = webview_builder.on_page_end(move |_: String| {
        page_loaded_end.store(true, Ordering::SeqCst);
      });
    }

    // Bridge new_window_req_handler to openharmony-ability's on_window_new.
    // Note: this callback runs on the ArkUI JS thread, not the Rust event loop thread.
    if let Some(new_window_req_handler) = new_window_req_handler {
      webview_builder = webview_builder.on_window_new(
        move |target_url: String, _is_alert: bool, _is_user_trigger: bool| -> bool {
          let features = crate::NewWindowFeatures {
            size: None,
            position: None,
            opener: crate::NewWindowOpener {},
          };
          match new_window_req_handler(target_url, features) {
            crate::NewWindowResponse::Allow => true,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            crate::NewWindowResponse::Create { .. } => {
              // OHOS: Create is not supported, degrade to Allow
              log::warn!("[WRY] NewWindowResponse::Create degraded to Allow on OHOS");
              true
            }
            crate::NewWindowResponse::Deny => false,
          }
        },
      );
    }

    let webview = webview_builder
      .build()
      .map_err(|e| Error::OpenHarmonyInitError(e.to_string()))?;

    let current_id = id.clone();
    let ipc_webview = webview.clone();
    webview
      .on_controller_attach(move || {
        let _builder = WebProxyBuilder::new(current_id.clone(), "ipc".to_string())
          .add_method("postMessage", |_frame, params| {
            if let Some(ipc_handler) = &ipc_handler {
              let message = params.get(0).unwrap_or(&"".to_string()).to_owned();
              // when load html, url will be base64 and uri parse will failed, so we use about:blank avoid panic
              let url = ipc_webview.url().unwrap_or("about:blank".to_string());
              let uri = url
                .parse::<Uri>()
                .unwrap_or(Uri::from_static("about:blank"));

              ipc_handler(Request::builder().uri(uri).body(message).unwrap());
            }
          })
          .build()
          .expect("Failed to build web proxy");
      })
      .map_err(|e| {
        Error::OpenHarmonyWebviewError(format!("Failed to add controller attach listener: {}", e))
      })?;

    for (protocol, callback) in custom_protocols {
      let webview_id = id.clone();
      webview
        .custom_protocol_async(protocol, move |_web, req, _is_on_main_frame, responder| {
          let responder: Box<dyn FnOnce(Response<Cow<'static, [u8]>>)> = Box::new(move |resp| {
            responder.respond(resp);
          });

          (callback)(&webview_id, req, RequestAsyncResponder { responder });
        })
        .unwrap();
    }

    Ok(Self {
      id,
      webview,
      page_loaded,
      bounds_cache: Mutex::new(initial_bounds),
      is_child,
      disposed: AtomicBool::new(false),
    })
  }

  pub fn print(&self) -> crate::Result<()> {
    Ok(())
  }

  pub fn id(&self) -> crate::WebViewId<'_> {
    &self.id
  }

  pub fn url(&self) -> crate::Result<String> {
    self
      .webview
      .url()
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to get url: {}", e)))
  }

  pub fn eval(&self, js: &str, callback: Option<impl Fn(String) + Send + 'static>) -> Result<()> {
    self
      .webview
      .evaluate_script_with_callback(
        js,
        callback.map(|c| Box::new(c) as Box<dyn Fn(String) + Send + 'static>),
      )
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to evaluate script: {}", e)))
  }

  // OHOS has no standalone DevTools window. `open_devtools`/`close_devtools`
  // toggle `WebviewController.setWebDebuggingAccess` (a process-global static
  // setter — affecting all webviews) instead. `is_devtools_open` returns the
  // ArkTS-side tracked state (OHOS has no getter). All three are cfg-gated like
  // webview2/wkwebview.
  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {
    if let Err(e) = self.webview.set_web_debugging_access(true) {
      log::warn!("[wry] open_devtools failed: {}", e);
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {
    if let Err(e) = self.webview.set_web_debugging_access(false) {
      log::warn!("[wry] close_devtools failed: {}", e);
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    self.webview.is_web_debugging_access().unwrap_or(false)
  }

  pub fn zoom(&self, scale_factor: f64) -> Result<()> {
    self
      .webview
      .set_zoom(scale_factor)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set zoom: {}", e)))
  }

  pub fn set_background_color(&self, background_color: RGBA) -> Result<()> {
    log::debug!(
      "[WRY] set_background_color called with RGBA: {:?}",
      background_color
    );
    // Convert RGBA(r,g,b,a) to 0xAARRGGBB number format for OHOS
    let color_number = ((background_color.3 as u32) << 24)  // AA (alpha)
                     | ((background_color.0 as u32) << 16)  // RR (red)
                     | ((background_color.1 as u32) << 8)   // GG (green)
                     | (background_color.2 as u32); // BB (blue)
    log::debug!("[WRY] Converted to u32: 0x{:08X}", color_number);
    self
      .webview
      .set_background_color(color_number)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set background color: {}", e)))
  }

  pub fn cookies(&self) -> Result<Vec<Cookie<'static>>> {
    // OHOS WebCookieManager has no API to enumerate all cookies across all
    // URLs. As a best-effort degradation, return cookies for the webview's
    // current URL (same as `cookies_for_url`). Returns empty when no URL.
    match self.webview.url() {
      Ok(url) if !url.is_empty() => self.cookies_for_url(&url),
      _ => Ok(vec![]),
    }
  }

  pub fn set_cookie(&self, cookie: &Cookie<'_>) -> Result<()> {
    // OHOS WebCookieManager.configCookieSync requires a URL + a Set-Cookie
    // formatted value. Derive the URL from the cookie's domain, falling back
    // to the webview's current URL.
    let domain = cookie.domain();
    let url = match domain {
      Some(d) if !d.is_empty() => format!("https://{}", d.trim_start_matches('.')),
      _ => match self.webview.url() {
        Ok(u) if !u.is_empty() => u,
        _ => {
          log::warn!(
            "[wry] set_cookie: cannot determine target URL (no domain and no current URL)"
          );
          return Ok(());
        }
      },
    };

    let value = format_set_cookie_value(cookie);

    self
      .webview
      .set_cookie(url, value)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set cookie: {}", e)))
  }

  pub fn delete_cookie(&self, _cookie: &Cookie<'_>) -> Result<()> {
    // OHOS WebCookieManager provides no single-cookie deletion (only
    // `clearAllCookies` / `clearSessionCookie`). Deleting all cookies here
    // would be semantically wrong, so this remains a no-op with a warning.
    log::warn!("[wry] delete_cookie is a no-op on OHOS: platform lacks single-cookie deletion");
    Ok(())
  }

  pub fn cookies_for_url(&self, url: &str) -> Result<Vec<Cookie<'static>>> {
    self
      .webview
      .cookies_with_url(url)
      .and_then(|cookie| {
        let cookies_data: Vec<Cookie<'static>> = cookie
          .split(';')
          .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
              return None;
            }
            Cookie::parse(s.to_string()).map(|c| c.into_owned()).ok()
          })
          .collect();

        Ok(cookies_data)
      })
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to get cookies: {}", e)))
  }

  pub fn reload(&self) -> Result<()> {
    self
      .webview
      .reload()
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to reload: {}", e)))
  }

  pub fn load_url(&self, url: &str) -> Result<()> {
    self
      .webview
      .load_url(url)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to load url: {}", e)))
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) -> Result<()> {
    self
      .webview
      .load_url_with_headers(url, headers)
      .map_err(|e| {
        Error::OpenHarmonyWebviewError(format!("Failed to load url with headers: {}", e))
      })
  }

  pub fn load_html(&self, html: &str) -> Result<()> {
    self
      .webview
      .load_html(html)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to load html: {}", e)))
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    self.webview.clear_all_browsing_data().map_err(|e| {
      Error::OpenHarmonyWebviewError(format!("Failed to clear all browsing data: {}", e))
    })
  }

  pub fn create_pdf(
    &self,
    path: &str,
    config: Option<PdfConfig>,
    callback: Box<dyn Fn(bool) + Send + 'static>,
  ) -> Result<()> {
    if !self.page_loaded.load(Ordering::SeqCst) {
      callback(false);
      return Err(Error::OpenHarmonyWebviewError(
        "Page not fully loaded. Call createPdf after onPageEnd.".into(),
      ));
    }
    self
      .webview
      .create_pdf(path, config, callback)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to create PDF: {}", e)))
  }

  pub fn bounds(&self) -> Result<Rect> {
    Ok(*self.bounds_cache.lock().unwrap())
  }

  pub fn set_bounds(&self, bounds: Rect) -> Result<()> {
    // Both child and non-child (main) webviews call ArkTS setBounds, which
    // triggers updateWebviewStyle → node.update → Web component re-render with
    // new data.style.width/height/position. The main webview's Web component
    // is laid out by data.style, so setBounds takes effect for both.
    // This works because tao propagates ContentRectChange as Resized (so
    // set_bounds is called on every window resize with the new dimensions),
    // and WindowIdStore uses or_insert (so the main window's mapping isn't
    // overwritten by child window creation).
    let (x, y) = bounds.position.to_logical::<f64>(1.0).into();
    let (w, h) = bounds.size.to_logical::<f64>(1.0).into();
    self
      .webview
      .set_bounds(x, y, w, h)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set bounds: {}", e)))?;
    *self.bounds_cache.lock().unwrap() = bounds;
    Ok(())
  }

  // set_visible is not gated on is_child because visibility applies to all webviews
  // (both main and child). The main webview should also support hide/show.
  // Note: On OHOS, BuilderNode.update() does not re-render .visibility() for Web
  // components at runtime (ArkUI limitation). This may not produce visual effect.
  pub fn set_visible(&self, visible: bool) -> Result<()> {
    self
      .webview
      .set_visible(visible)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set visible: {}", e)))
  }

  pub fn focus(&self) -> Result<()> {
    self.webview.focus().map_err(|_e| Error::NotMainThread)
  }

  // OHOS has no separate parent window for the webview (the webview is the
  // window content), so focus_parent focuses the webview itself via requestFocus
  // — the closest analog to desktop focus_parent which focuses the parent HWND.
  pub fn focus_parent(&self) -> Result<()> {
    self
      .webview
      .focus()
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to focus parent: {}", e)))
  }

  pub fn dispose_child(&self) {
    if self.is_child && !self.disposed.swap(true, Ordering::SeqCst) {
      let _ = self.webview.dispose();
    }
  }
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    if self.is_child && !self.disposed.swap(true, Ordering::SeqCst) {
      let _ = self.webview.dispose();
    }
  }
}

pub fn platform_webview_version() -> Result<String> {
  Ok("1.0.0".to_string())
}

/// Format a `cookie::Cookie` as a Set-Cookie value string (RFC 6265) for
/// `WebCookieManager.configCookieSync`. Maps the core attributes
/// (name/value/domain/path/secure/http_only/same_site); other attributes
/// (e.g. Max-Age/Expires) are intentionally omitted.
fn format_set_cookie_value(cookie: &Cookie<'_>) -> String {
  let mut value = format!("{}={}", cookie.name(), cookie.value());
  if let Some(d) = cookie.domain() {
    value.push_str(&format!("; Domain={}", d));
  }
  if let Some(p) = cookie.path() {
    value.push_str(&format!("; Path={}", p));
  }
  if cookie.secure().unwrap_or(false) {
    value.push_str("; Secure");
  }
  if cookie.http_only().unwrap_or(false) {
    value.push_str("; HttpOnly");
  }
  if let Some(same_site) = cookie.same_site() {
    value.push_str(&format!("; SameSite={}", same_site));
  }
  value
}

#[cfg(test)]
mod tests {
  use super::*;
  use cookie::SameSite;

  #[test]
  fn format_set_cookie_value_includes_core_attrs() {
    let cookie = Cookie::build(("token", "abc"))
      .domain("example.com")
      .path("/")
      .secure(true)
      .http_only(true)
      .same_site(SameSite::Lax)
      .build();
    let v = format_set_cookie_value(&cookie);
    assert!(v.starts_with("token=abc"));
    assert!(v.contains("; Domain=example.com"));
    assert!(v.contains("; Path=/"));
    assert!(v.contains("; Secure"));
    assert!(v.contains("; HttpOnly"));
    assert!(v.contains("; SameSite=Lax"));
  }

  #[test]
  fn format_set_cookie_value_minimal() {
    let cookie = Cookie::build(("k", "v")).build();
    assert_eq!(format_set_cookie_value(&cookie), "k=v");
  }
}

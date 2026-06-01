use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::WebViewAttributes;
use crate::{Error, PageLoadEvent, Rect, RequestAsyncResponder, Result, RGBA};
use cookie::Cookie;
use http::{Request, Response, Uri};
use openharmony_ability::{native_web::WebProxyBuilder, WebViewBuilder, WebViewStyle, Webview};
pub use openharmony_ability::PdfConfig;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::util::Counter;

static COUNTER: Counter = Counter::new();

pub struct InnerWebView {
  id: String,
  webview: Webview,
}

impl InnerWebView {
  pub(crate) fn new_as_child(
    _window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new(_window, attributes, pl_attrs)
  }

  pub(crate) fn new(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
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
      ..
    } = attributes;

    let initial_url = url.clone();
    let initial_headers = headers;

    let id = id
      .map(|id| id.to_string())
      .unwrap_or_else(|| COUNTER.next().to_string());

    let background_color =
      background_color.map(|c| format!("#{:02X}{:02X}{:02X}{:02X}", c.0, c.1, c.2, c.3));

    // Get window_id from platform-specific attributes
    let window_id = pl_attrs.window_id.unwrap_or(0);

    let merged_scripts = initialization_scripts
        .iter()
        .map(|s| s.script.clone())
        .collect::<Vec<_>>()
        .join("\n");

    let mut webview_builder = WebViewBuilder::new()
      .id(id.clone())
      .style(WebViewStyle {
        x: None,
        y: None,
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
      webview_builder = webview_builder.on_navigation_request(move |url: String| -> bool {
        navigation_handler(url)
      });
    }

    // Wire document title changed handler (on_document_title_changed → WebViewBuilder::on_title_change)
    if let Some(document_title_changed_handler) = document_title_changed_handler {
      webview_builder = webview_builder.on_title_change(move |title: String| {
        document_title_changed_handler(title)
      });
    }

    // Wire download started handler (FnMut wrapped in Mutex to satisfy Fn bound)
    if let Some(download_started_handler) = download_started_handler {
      let handler = Mutex::new(download_started_handler);
      webview_builder = webview_builder.on_download_start(move |url: String, path: &mut PathBuf| -> bool {
        let mut handler = handler.lock().unwrap();
        handler(url, path)
      });
    }

    // Wire download completed handler (Rc<dyn Fn> cloned into closure)
    if let Some(download_completed_handler) = download_completed_handler {
      webview_builder = webview_builder.on_download_end(move |url: String, path: Option<PathBuf>, success: bool| {
        download_completed_handler(url, path, success)
      });
    }

    // Wire on_page_load handler (pre-build: onPageBegin/onPageEnd in ArkTS Web component)
    if let Some(on_page_load_handler) = on_page_load_handler {
      let handler = Arc::new(on_page_load_handler);
      let handler_begin = handler.clone();
      let handler_end = handler.clone();
      webview_builder = webview_builder.on_page_begin(move |url: String| {
        handler_begin(PageLoadEvent::Started, url);
      });
      webview_builder = webview_builder.on_page_end(move |url: String| {
        handler_end(PageLoadEvent::Finished, url);
      });
    }

    // Suppress unused new_window_req_handler (openharmony-ability has no corresponding interface)
    let _ = new_window_req_handler;

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

    Ok(Self { id, webview })
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

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    false
  }

  pub fn zoom(&self, scale_factor: f64) -> Result<()> {
    self
      .webview
      .set_zoom(scale_factor)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set zoom: {}", e)))
  }

  pub fn set_background_color(&self, _background_color: RGBA) -> Result<()> {
    Ok(())
  }

  pub fn cookies(&self) -> Result<Vec<Cookie<'static>>> {
    Ok(vec![])
  }

  pub fn set_cookie(&self, _cookie: &Cookie<'_>) -> Result<()> {
    Ok(())
  }

  pub fn delete_cookie(&self, _cookie: &Cookie<'_>) -> Result<()> {
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
    self
      .webview
      .create_pdf(path, config, callback)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to create PDF: {}", e)))
  }

  pub fn bounds(&self) -> Result<Rect> {
    Ok(Rect::default())
  }

  pub fn set_bounds(&self, _bounds: Rect) -> Result<()> {
    Ok(())
  }

  pub fn set_visible(&self, _visible: bool) -> Result<()> {
    Ok(())
  }

  pub fn focus(&self) -> Result<()> {
    self.webview.focus().map_err(|_e| Error::NotMainThread)
  }

  pub fn focus_parent(&self) -> Result<()> {
    Ok(())
  }
}

pub fn platform_webview_version() -> Result<String> {
  Ok("1.0.0".to_string())
}

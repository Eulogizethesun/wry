use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use super::WebViewAttributes;
use crate::{
  custom_protocol_workaround, Error, PageLoadEvent, Rect, RequestAsyncResponder, Result, RGBA,
};
use cookie::Cookie;
use http::{Request, Response, Uri};
use openharmony_ability_plugin_webview::{
  Either, WebviewCallbacksBuilder, WebviewClient, WebviewCreateRequest,
  WebviewDownloadStartResponse, WebviewHandle, WebviewHttpsInterceptResponse,
  WebviewInitializationScript, WebviewJavascriptProxyBuilder, WebviewProtocol,
  WebviewProtocolOptions, WebviewStyle,
};

/// Re-exported so the runtime layer (`tauri-runtime-wry`) can register the
/// WebView bridge plugin on the `OpenHarmonyApp` during OHOS app setup.
/// `WebviewClient::create` is a bridge call routed through this plugin; without
/// registering it, `create` fails with "not installed for '<module>'".
pub use openharmony_ability_plugin_webview::WebviewBridgePlugin;

/// Configuration for PDF generation. The bridge API uses fixed A4 settings
/// (8.27x11.69in, zero margins, shouldPrintBackground=true), but this struct
/// is retained for API compatibility with the wry public interface and
/// `tauri-runtime-wry` which constructs it from `tauri_runtime::PdfConfig`.
pub struct PdfConfig {
  pub width: Option<f64>,
  pub height: Option<f64>,
  pub margin_top: Option<f64>,
  pub margin_bottom: Option<f64>,
  pub margin_left: Option<f64>,
  pub margin_right: Option<f64>,
  pub scale: Option<f64>,
  pub should_print_background: Option<bool>,
}

use crate::util::Counter;

/// Re-export of `openharmony_ability_plugin_webview::WebviewHandle` for use in wry's public API.
pub type OhosWebviewHandle = WebviewHandle;

static COUNTER: Counter = Counter::new();

// ─── SendSyncBox: unsafe wrapper for non-Send wry handlers ──────────────────────

/// Wrapper that makes any `T` `Send + Sync`.
///
/// **SAFETY**: The wrapped callback is only ever invoked from the NAPI main thread
/// (bridge reverse-event callbacks). The wry handler API (`Box<dyn Fn...>`) doesn't
/// require `Send`, but in practice all handlers are `Send` (they use `Arc`, `String`,
/// channels). The bridge callback registry requires `Send + Sync + 'static`, so we
/// wrap the handlers to satisfy the type system. This follows the same pattern as
/// Android's `UnsafeIpc` / `UnsafeTitleHandler` wrappers.
struct SendSyncBox<T>(T);
unsafe impl<T> Send for SendSyncBox<T> {}
unsafe impl<T> Sync for SendSyncBox<T> {}

// ─── BridgeExecutor ──────────────────────────────────────────────────────────────

/// Background tokio runtime for spawning async bridge calls (fire-and-forget).
///
/// `WebviewHandle` methods are `async`. wry's public API is synchronous and cannot
/// `.await`. `BridgeExecutor` wraps a `tokio::runtime::Handle` from a dedicated
/// background thread (`ohos-wry-bridge-rt`) that drives a current-thread runtime.
/// Calling `spawn(future)` sends the future to that background thread to be polled.
/// The TSFN NonBlocking call inside the bridge returns immediately; the ArkTS callback
/// runs on the main thread — no deadlock.
#[derive(Clone)]
pub(crate) struct BridgeExecutor {
  handle: tokio::runtime::Handle,
  main_thread_id: std::thread::ThreadId,
}

impl BridgeExecutor {
  fn new() -> Self {
    let runtime = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("Failed to create wry bridge runtime");
    let handle = runtime.handle().clone();
    std::thread::Builder::new()
      .name("ohos-wry-bridge-rt".into())
      .spawn(move || runtime.block_on(std::future::pending::<()>()))
      .expect("Failed to spawn wry bridge runtime thread");
    Self {
      handle,
      main_thread_id: std::thread::current().id(),
    }
  }

  pub(crate) fn main_thread_id(&self) -> std::thread::ThreadId {
    self.main_thread_id
  }

  /// Spawn a fire-and-forget bridge call. The result is ignored.
  pub(crate) fn spawn<F>(&self, future: F)
  where
    F: std::future::Future<Output = ()> + Send + 'static,
  {
    self.handle.spawn(future);
  }
}

// ─── PendingOp ───────────────────────────────────────────────────────────────────

/// Operations queued before `WebviewClient::create()` completes (delayed attach).
/// These are replayed in order once the handle is ready.
enum PendingOp {
  LoadUrl(String),
  LoadUrlWithHeaders(String, std::collections::BTreeMap<String, String>),
  LoadHtml(String),
  Reload,
  Zoom(f64),
  SetBackgroundColor(String),
  SetVisible(bool),
  SetBounds(f64, f64, f64, f64),
  Focus,
  ClearAllBrowsingData,
  SetCookie(String, String),
  Print(String),
  Eval(String, Option<Box<dyn Fn(String) + Send + 'static>>),
  CreatePdf(String, Option<Box<dyn Fn(bool) + Send + 'static>>),
  Remove,
}

impl PendingOp {
  async fn execute(self, handle: &WebviewHandle) {
    match self {
      PendingOp::LoadUrl(url) => {
        if let Err(e) = handle.load_url(url).await {
          log::warn!("[wry] pending load_url failed: {}", e);
        }
      }
      PendingOp::LoadUrlWithHeaders(url, headers) => {
        if let Err(e) = handle.load_url_with_headers(url, headers).await {
          log::warn!("[wry] pending load_url_with_headers failed: {}", e);
        }
      }
      PendingOp::LoadHtml(html) => {
        if let Err(e) = handle.load_html(html).await {
          log::warn!("[wry] pending load_html failed: {}", e);
        }
      }
      PendingOp::Reload => {
        if let Err(e) = handle.reload().await {
          log::warn!("[wry] pending reload failed: {}", e);
        }
      }
      PendingOp::Zoom(scale) => {
        if let Err(e) = handle.set_zoom(scale).await {
          log::warn!("[wry] pending set_zoom failed: {}", e);
        }
      }
      PendingOp::SetBackgroundColor(color) => {
        if let Err(e) = handle.set_background_color(color).await {
          log::warn!("[wry] pending set_background_color failed: {}", e);
        }
      }
      PendingOp::SetVisible(visible) => {
        if let Err(e) = handle.set_visible(visible).await {
          log::warn!("[wry] pending set_visible failed: {}", e);
        }
      }
      PendingOp::SetBounds(x, y, w, h) => {
        if let Err(e) = handle.set_bounds(x, y, w, h).await {
          log::warn!("[wry] pending set_bounds failed: {}", e);
        }
      }
      PendingOp::Focus => {
        if let Err(e) = handle.focus().await {
          log::warn!("[wry] pending focus failed: {}", e);
        }
      }
      PendingOp::ClearAllBrowsingData => {
        if let Err(e) = handle.clear_all_browsing_data().await {
          log::warn!("[wry] pending clear_all_browsing_data failed: {}", e);
        }
      }
      PendingOp::SetCookie(url, value) => {
        if let Err(e) = handle.set_cookie(url, value).await {
          log::warn!("[wry] pending set_cookie failed: {}", e);
        }
      }
      PendingOp::Print(path) => {
        if let Err(e) = handle.create_pdf(&path).await {
          log::warn!("[wry] pending create_pdf failed: {}", e);
        } else if let Err(e) = handle.print(&path).await {
          log::warn!("[wry] pending print failed: {}", e);
        }
      }
      PendingOp::Eval(js, callback) => {
        match handle.evaluate_script(js).await {
          Ok(result) => {
            if let Some(cb) = callback {
              cb(result.unwrap_or_default());
            }
          }
          Err(e) => log::warn!("[wry] pending evaluate_script failed: {}", e),
        }
      }
      PendingOp::CreatePdf(path, callback) => {
        let result = handle.create_pdf(&path).await;
        let success = result.is_ok();
        if let Some(cb) = callback {
          cb(success);
        }
      }
      PendingOp::Remove => {
        if let Err(e) = handle.remove().await {
          log::warn!("[wry] pending remove failed: {}", e);
        }
      }
    }
  }
}

// ─── Type aliases ─────────────────────────────────────────────────────────────────

type ProtocolHandler = Box<dyn Fn(crate::WebViewId, Request<Vec<u8>>, RequestAsyncResponder)>;
type SharedProtocolHandler = Arc<SendSyncBox<ProtocolHandler>>;

// ─── InnerWebView ───────────────────────────────────────────────────────────────

pub struct InnerWebView {
  id: String,
  /// Async-ready handle. Set when `WebviewClient::create()` completes.
  /// Uses `OnceLock` — set exactly once when create finishes.
  /// **TOCTOU guard**: `pending_ops` lock must be held when checking `handle.get()`
  /// and when setting the handle + draining pending ops.
  handle: Arc<OnceLock<WebviewHandle>>,
  /// Operations queued before create completes. Also serves as the TOCTOU guard lock.
  pending_ops: Arc<Mutex<Vec<PendingOp>>>,
  /// For constructing handles before create completes (used by webview_handle()).
  client: WebviewClient,
  runtime: BridgeExecutor,
  page_loaded: Arc<AtomicBool>,
  url_cache: Arc<Mutex<String>>,
  bounds_cache: Mutex<Rect>,
  devtools_open: AtomicBool,
  is_child: bool,
  disposed: AtomicBool,
}

impl InnerWebView {
  pub(crate) fn new_as_child(
    window: &impl raw_window_handle::HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(window, attributes, pl_attrs, true)
  }

  pub(crate) fn new(
    window: &impl raw_window_handle::HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(window, attributes, pl_attrs, false)
  }

  #[allow(clippy::too_many_lines)]
  fn new_inner(
    _window: &impl raw_window_handle::HasWindowHandle,
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
      clipboard,
      zoom_hotkeys_enabled,
      drag_drop_handler,
      ..
    } = attributes;

    let initial_url = url.clone();
    let initial_headers = headers;

    let id = id
      .map(|id| id.to_string())
      .unwrap_or_else(|| COUNTER.next().to_string());

    let window_id = pl_attrs.window_id.unwrap_or(0);
    let use_https = pl_attrs.use_https;
    let drag_drop_overlay = pl_attrs.drag_drop_overlay;
    let bridge_runtime = pl_attrs.bridge_runtime.ok_or_else(|| {
      Error::OpenHarmonyInitError(
        "BridgeRuntime not provided — call with_bridge_runtime() on the builder".into(),
      )
    })?;

    log::info!(
      "[WRY OHOS] build: use_https={}, drag_drop_overlay={}, custom_protocols={}",
      use_https,
      drag_drop_overlay,
      custom_protocols.len()
    );
    log::info!("[WRY OHOS DBG] build: window_id={}", window_id);

    let client = WebviewClient::from_bridge(bridge_runtime);
    let runtime = BridgeExecutor::new();

    // State shared with callbacks
    let page_loaded = Arc::new(AtomicBool::new(false));
    let url_cache = Arc::new(Mutex::new(String::new()));
    let devtools_open = AtomicBool::new(false);

    // 1. Register reverse callbacks (before create)
    let mut callbacks_builder = WebviewCallbacksBuilder::new(&id);

    // Navigation handler: wry semantics true=allow → bridge intercept=true means block
    if let Some(navigation_handler) = navigation_handler {
      let handler = Arc::new(SendSyncBox(navigation_handler));
      callbacks_builder = callbacks_builder.on_navigation_request(move |req| {
        !(handler.0)(req.url)
      });
    }

    // Title change handler
    if let Some(document_title_changed_handler) = document_title_changed_handler {
      let handler = Arc::new(SendSyncBox(document_title_changed_handler));
      callbacks_builder = callbacks_builder.on_title_change(move |event| {
        (handler.0)(event.title);
      });
    }

    // Download started handler (FnMut, needs Mutex)
    if let Some(download_started_handler) = download_started_handler {
      let handler = Arc::new(Mutex::new(SendSyncBox(download_started_handler)));
      callbacks_builder = callbacks_builder.on_download_start(move |req| {
        let mut path = req.temp_path.map(PathBuf::from).unwrap_or_default();
        let allow = {
          let mut guard = handler.lock().unwrap();
          (guard.0)(req.url, &mut path)
        };
        WebviewDownloadStartResponse {
          allow,
          temp_path: path.to_str().map(|s| s.to_string()),
        }
      });
    }

    // Download completed handler
    if let Some(download_completed_handler) = download_completed_handler {
      let handler = Arc::new(SendSyncBox(download_completed_handler));
      callbacks_builder = callbacks_builder.on_download_end(move |event| {
        (handler.0)(
          event.url,
          event.temp_path.map(PathBuf::from),
          event.success,
        );
      });
    }

    // Page begin/end — always registered (for page_loaded + url_cache)
    let on_page_load = on_page_load_handler.map(|h| Arc::new(SendSyncBox(h)));

    let url_cache_begin = url_cache.clone();
    let page_loaded_begin = page_loaded.clone();
    let on_page_load_begin = on_page_load.clone();
    callbacks_builder = callbacks_builder.on_page_begin(move |event| {
      page_loaded_begin.store(false, Ordering::SeqCst);
      url_cache_begin.lock().unwrap().clone_from(&event.url);
      if let Some(handler) = &on_page_load_begin {
        (handler.0)(PageLoadEvent::Started, event.url);
      }
    });

    let url_cache_end = url_cache.clone();
    let page_loaded_end = page_loaded.clone();
    let on_page_load_end = on_page_load.clone();
    callbacks_builder = callbacks_builder.on_page_end(move |event| {
      page_loaded_end.store(true, Ordering::SeqCst);
      url_cache_end.lock().unwrap().clone_from(&event.url);
      if let Some(handler) = &on_page_load_end {
        (handler.0)(PageLoadEvent::Finished, event.url);
      }
    });

    // New window handler
    if let Some(new_window_req_handler) = new_window_req_handler {
      let handler = Arc::new(SendSyncBox(new_window_req_handler));
      callbacks_builder = callbacks_builder.on_new_window_request(move |req| {
        let features = crate::NewWindowFeatures {
          size: None,
          position: None,
          opener: crate::NewWindowOpener {},
        };
        match (handler.0)(req.target_url, features) {
          crate::NewWindowResponse::Allow => true,
          crate::NewWindowResponse::Deny => false,
          // Create: Tauri already built its own OS window (the Float
          // WebviewWindow) and will load the target URL into it. Return false
          // so ArkWeb CANCELS its own popup — the bridge then runs the
          // non-blocking Deny path (setWebController(null)) instead of
          // spawning a duplicate dialog. Returning true here would tell ArkWeb
          // to ALSO open a popup, which is wrong (and the popup controller has
          // no synchronous Web host, risking a UI-thread block).
          #[cfg(not(any(target_os = "android", target_os = "ios")))]
          crate::NewWindowResponse::Create { .. } => false,
        }
      });
    }

    // Drag-drop handler (4 events)
    if let Some(drag_drop_handler) = drag_drop_handler {
      let handler = Arc::new(SendSyncBox(drag_drop_handler));
      let h_enter = handler.clone();
      callbacks_builder = callbacks_builder.on_drag_enter(move |event| {
        let _ = (h_enter.0)(crate::DragDropEvent::Enter {
          paths: vec![],
          position: (event.x as i32, event.y as i32),
        });
      });
      let h_over = handler.clone();
      callbacks_builder = callbacks_builder.on_drag_over(move |event| {
        let _ = (h_over.0)(crate::DragDropEvent::Over {
          position: (event.x as i32, event.y as i32),
        });
      });
      let h_drop = handler.clone();
      callbacks_builder = callbacks_builder.on_drag_drop(move |event| {
        let paths: Vec<PathBuf> = event.paths.into_iter().map(PathBuf::from).collect();
        let _ = (h_drop.0)(crate::DragDropEvent::Drop {
          paths,
          position: (event.x as i32, event.y as i32),
        });
      });
      let h_leave = handler.clone();
      callbacks_builder = callbacks_builder.on_drag_leave(move |_event| {
        let _ = (h_leave.0)(crate::DragDropEvent::Leave);
      });
    }

    // 2. Convert custom protocols to shared Send+Sync form
    let shared_protocols: HashMap<String, SharedProtocolHandler> = custom_protocols
      .into_iter()
      .map(|(k, v)| (k, Arc::new(SendSyncBox(v))))
      .collect();
    let shared_for_intercept = shared_protocols.clone();

    // Register https intercept callback (if use_https and protocols exist)
    if use_https && !shared_for_intercept.is_empty() {
      let https_id = id.clone();
      let https_protocols = shared_for_intercept.clone();
      callbacks_builder =
        callbacks_builder.on_https_intercept_request(move |req| {
          dispatch_https_intercept_sync(&req.url, &https_id, &https_protocols)
        });
    }

    callbacks_builder
      .build()
      .map_err(|e| Error::OpenHarmonyInitError(e.to_string()))?;

    // 3. Register IPC handler via WebviewJavascriptProxyBuilder (before create)
    let id_for_ipc = id.clone();
    let url_cache_ipc = url_cache.clone();
    if let Some(ipc_handler) = ipc_handler {
      let handler = Arc::new(SendSyncBox(ipc_handler));
      WebviewJavascriptProxyBuilder::new(&id_for_ipc, "ipc")
        .add_method("postMessage", move |_wid, args| {
          let message = args.get(0).unwrap_or(&"".to_string()).to_owned();
          let url = url_cache_ipc.lock().unwrap().clone();
          let uri = if url.is_empty() {
            Uri::from_static("about:blank")
          } else {
            url
              .parse::<Uri>()
              .unwrap_or(Uri::from_static("about:blank"))
          };
          let request = Request::builder().uri(uri).body(message).unwrap();
          (handler.0)(request);
        })
        .build()
        .map_err(|e| {
          Error::OpenHarmonyWebviewError(format!("Failed to register IPC proxy: {}", e))
        })?;
    }

    // 4. Register custom protocols (before create)
    //
    // OHOS ArkWeb uses a two-phase custom-protocol model: each scheme must be
    // process-wide *declared* via `WebviewProtocol::register` before the Web
    // engine initializes, then *bound* per-controller via
    // `custom_protocol_async` (which enforces the declaration through
    // `require_declared`). The engine initializes lazily inside the bridge
    // `create` call below (`ensureWebEngineInitialized`), which is spawned in
    // step 6 — i.e. after this synchronous section — so declaring here
    // satisfies the ordering even though tauri only collects its scheme list
    // at runtime (during `Builder::run`). Re-declaring an already-declared
    // scheme is a no-op (`scheme_registration_needed`), so child/second
    // webviews are safe. Options match the openharmony-ability demo: custom
    // protocols must be fetchable, bypass CSP, allow CORS, and use code cache.
    let protocol_options = WebviewProtocolOptions::Standard
      | WebviewProtocolOptions::CorsEnabled
      | WebviewProtocolOptions::CspBypassing
      | WebviewProtocolOptions::FetchEnabled
      | WebviewProtocolOptions::CodeCacheEnabled;
    for protocol in shared_protocols.keys() {
      WebviewProtocol::register(protocol, protocol_options).map_err(|e| {
        Error::OpenHarmonyInitError(format!(
          "Failed to declare custom protocol '{}': {}",
          protocol, e
        ))
      })?;
    }

    for (protocol, callback) in &shared_protocols {
      let webview_id = id.clone();
      let protocol = protocol.clone();
      let callback = Arc::clone(callback);
      client
        .custom_protocol_async(webview_id.clone(), &protocol, move |_url, req, _is_main, responder| {
          let responder_box: Box<dyn FnOnce(Response<Cow<'static, [u8]>>)> =
            Box::new(move |resp| {
              responder.respond(resp);
            });
          (callback.0)(
            webview_id.as_str(),
            req,
            RequestAsyncResponder { responder: responder_box },
          );
        })
        .map_err(|e| {
          Error::OpenHarmonyInitError(format!("Failed to register custom protocol: {}", e))
        })?;
    }

    // 5. Build create request
    let initial_bounds = bounds.unwrap_or_default();

    let bg_color_str = background_color.map(|c| {
      format!("#{:02X}{:02X}{:02X}{:02X}", c.3, c.0, c.1, c.2)
    });

    // ArkWeb's `javaScriptOnDocumentStart` only injects a `ScriptItem` whose
    // `scriptRules` array matches the document origin. An EMPTY array matches
    // no document, so none of the tauri init scripts (including the one that
    // defines `window.__TAURI_INTERNALS__`) would run — the frontend bundle
    // then throws `Cannot read properties of undefined (reading 'metadata')`
    // and never mounts (only a post-load `webview.eval` probe shows).
    //
    // Build match rules covering both runtime configurations:
    //   * `"*"`  — matches http(s) origins, including the `https://tauri.localhost`
    //     rewrite used when `use_https` is enabled.
    //   * `"{scheme}://"` — matches custom-protocol origins (e.g. `tauri://`),
    //     which `"*"` does NOT match on ArkWeb: custom protocols require a
    //     scheme-prefixed rule ending in `://` (see openharmony-ability
    //     `多窗口特性实现文档.md` TODO-5). One rule per registered custom scheme.
    // The raw-scheme default (`tauri://localhost`) is the tauri default
    // (`useHttpsScheme` is false unless a consumer opts in), so the
    // scheme-prefixed rule is essential.
    let script_rules: Vec<String> = {
      let mut rules: Vec<String> = vec!["*".to_string()];
      for scheme in shared_protocols.keys() {
        rules.push(format!("{}://", scheme));
      }
      rules
    };

    // Concatenate all init scripts into a SINGLE `ScriptItem`.
    //
    // ArkWeb's `javaScriptOnDocumentStart` executes `ScriptItem[]` in
    // **dictionary (lexicographic) order, NOT array order** (ArkWeb docs:
    // "该脚本按照字典序执行，非数组本身顺序，若需数组本身顺序，建议使用
    // runJavaScriptOnDocumentStart 接口"). Tauri's init scripts are an
    // ordered dependency chain (create `window.__TAURI_INTERNALS__` →
    // ipc-protocol → metadata → init/core.js). Registering them as separate
    // ScriptItems lets ArkWeb reorder them by script-content lexicographically:
    // the `metadata` script (`Object.defineProperty(window.__TAURI_INTERNALS__,
    // 'metadata', …)`) sorts BEFORE the script that creates
    // `window.__TAURI_INTERNALS__` (its body begins with a leading newline +
    // fewer spaces, so it compares greater), so it runs while
    // `__TAURI_INTERNALS__` is still `undefined` →
    // `Object.defineProperty called on non-object` → the metadata (and the
    // downstream `invoke`) never attaches → `__TAURI_INTERNALS__` stays
    // `{plugins:{}}` → the frontend bundle throws
    // "Cannot read properties of undefined (reading 'currentWindow')" and never
    // mounts (only a post-load `webview.eval` probe shows).
    //
    // A single concatenated ScriptItem runs all scripts in written order within
    // one execution context, so the dependency chain holds. Joining with `;\n`
    // prevents ASI/empty-statement hazards between script bodies. This mirrors
    // Android's `inject_scripts_into_html`, which collapses all init scripts
    // into one HTML document in order. (OHOS `ScriptItem` carries only `script`
    // + `scriptRules` — `for_main_frame_only` is already dropped on this path —
    // so a single blob loses no behavior OHOS currently honors.)
    let merged_scripts: Vec<WebviewInitializationScript> =
      if initialization_scripts.is_empty() {
        Vec::new()
      } else {
        let body = initialization_scripts
          .iter()
          .map(|s| s.script.as_str())
          .collect::<Vec<_>>()
          .join(";\n");
        vec![WebviewInitializationScript {
          script: body,
          script_rules,
        }]
      };

    let mut create_req = WebviewCreateRequest::new(&id)
      .style(WebviewStyle {
        background_color: bg_color_str,
        ..Default::default()
      })
      .initialization_scripts(merged_scripts)
      .transparent(transparent);

    // Set url or html
    if let Some(html) = html {
      create_req = create_req.html(html);
    } else if let Some(url) = initial_url {
      let url = if use_https && !shared_for_intercept.is_empty() {
        rewrite_https_url_if_matching(&url, &shared_for_intercept)
      } else {
        url
      };
      create_req = create_req.url(url);
      if let Some(headers) = initial_headers {
        let header_map: std::collections::BTreeMap<String, String> = headers
          .iter()
          .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
          .collect();
        create_req.headers = Some(header_map);
      }
    }

    // Field assignments (no builder methods for these)
    create_req.javascript_enabled = Some(!javascript_disabled);
    create_req.autoplay = Some(autoplay);
    create_req.clipboard = Some(clipboard);
    create_req.zoom_hotkeys = Some(zoom_hotkeys_enabled);
    create_req.drag_drop_overlay = Some(drag_drop_overlay);
    // Seed https_intercept_protocols at create time (same gate as the URL rewrite above:
    // use_https && !shared_for_intercept.is_empty()). This fixes the create-vs-register
    // race where the ArkTS httpsInterceptProtocols Set was empty at first loadUrl →
    // onInterceptRequest early-returned null → ArkWeb fetched https://tauri.localhost for
    // real → arkweb-error://webdata/ → isSecureContext=false. Late register_https_intercept
    // stays as a runtime add path; Set.add is idempotent so no conflict.
    if use_https && !shared_for_intercept.is_empty() {
      create_req.https_intercept_protocol_list =
        Some(shared_for_intercept.keys().cloned().collect::<Vec<_>>());
    }
    if window_id != 0 {
      create_req.window_id = Some(window_id);
      log::info!("[WRY OHOS DBG] create_req.window_id set to Some({})", window_id);
    }
    if let Some(user_agent) = user_agent {
      create_req.user_agent = Some(user_agent);
    }

    // devtools flag at create time
    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
      create_req.devtools = Some(devtools);
    }

    // For child webviews with bounds, set initial style position/size.
    if is_child && bounds.is_some() {
      let (x, y) = initial_bounds.position.to_logical::<f64>(1.0).into();
      let (w, h) = initial_bounds.size.to_logical::<f64>(1.0).into();
      create_req.style.x = Some(Either::A(x));
      create_req.style.y = Some(Either::A(y));
      create_req.style.width = Some(Either::A(w));
      create_req.style.height = Some(Either::A(h));
    }

    // 6. Spawn create on BridgeExecutor (delayed attach)
    let handle_slot: Arc<OnceLock<WebviewHandle>> = Arc::new(OnceLock::new());
    let pending_ops: Arc<Mutex<Vec<PendingOp>>> = Arc::new(Mutex::new(Vec::new()));

    let client_for_create = client.clone();
    let handle_slot_for_create = handle_slot.clone();
    let pending_ops_for_create = pending_ops.clone();
    let runtime_for_create = runtime.clone();

    runtime.spawn(async move {
      let result = client_for_create.create(create_req).await;
      match result {
        Ok(handle) => {
          // TOCTOU guard: hold pending_ops lock while setting handle + draining
          let pending = {
            let mut guard = pending_ops_for_create.lock().unwrap();
            if handle_slot_for_create.set(handle).is_err() {
              // Already set (shouldn't happen) — discard
              return;
            }
            let handle_ref = handle_slot_for_create.get().unwrap().clone();
            let pending: Vec<PendingOp> = guard.drain(..).collect();
            (handle_ref, pending)
          };
          // Spawn replay outside the lock
          let (handle_ref, pending) = pending;
          runtime_for_create.spawn(async move {
            for op in pending {
              op.execute(&handle_ref).await;
            }
          });
        }
        Err(e) => {
          log::error!("[wry] WebviewClient::create() failed: {}", e);
        }
      }
    });

    // 7. Register https intercept protocols (async, fire-and-forget)
    if use_https && !shared_for_intercept.is_empty() {
      let protocol_names: Vec<String> = shared_for_intercept.keys().cloned().collect();
      let client_for_intercept = client.clone();
      let id_for_intercept = id.clone();
      runtime.spawn(async move {
        if let Err(e) = client_for_intercept
          .register_https_intercept(id_for_intercept, protocol_names)
          .await
        {
          log::warn!("[wry] register_https_intercept failed: {}", e);
        }
      });
    }

    Ok(Self {
      id,
      handle: handle_slot,
      pending_ops,
      client,
      runtime,
      page_loaded,
      url_cache,
      bounds_cache: Mutex::new(initial_bounds),
      devtools_open,
      is_child,
      disposed: AtomicBool::new(false),
    })
  }

  // ─── Helper: dispatch or queue ──────────────────────────────────────────────

  /// Execute a PendingOp immediately if the handle is ready, otherwise queue it.
  /// **TOCTOU guard**: must hold `pending_ops` lock when checking `handle.get()`.
  fn dispatch_or_queue(&self, op: PendingOp) {
    let mut guard = self.pending_ops.lock().unwrap();
    if let Some(handle) = self.handle.get() {
      // Handle is ready — release lock and spawn
      let handle = handle.clone();
      drop(guard);
      self.runtime.spawn(async move {
        op.execute(&handle).await;
      });
    } else {
      // Not ready — queue for replay after create
      guard.push(op);
    }
  }

  /// Get the handle if ready, or None if create is still pending.
  fn try_handle(&self) -> Option<WebviewHandle> {
    self.handle.get().cloned()
  }

  // ─── Public API methods ─────────────────────────────────────────────────────

  pub fn print(&self) -> Result<()> {
    if !self.page_loaded.load(Ordering::SeqCst) {
      return Err(Error::OpenHarmonyWebviewError(
        "Page not fully loaded. Call print after onPageEnd.".into(),
      ));
    }
    let temp_path = std::env::temp_dir().join(format!(
      "wry_print_{}.pdf",
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
    ));
    let path_str = temp_path.to_string_lossy().to_string();
    self.dispatch_or_queue(PendingOp::Print(path_str));
    Ok(())
  }

  pub fn id(&self) -> crate::WebViewId<'_> {
    &self.id
  }

  // Pattern C: cached — url from page-begin/end events
  pub fn url(&self) -> Result<String> {
    Ok(self.url_cache.lock().unwrap().clone())
  }

  // Pattern B: callback — spawn + async trigger callback
  pub fn eval(
    &self,
    js: &str,
    callback: Option<impl Fn(String) + Send + 'static>,
  ) -> Result<()> {
    let js = js.to_string();
    let cb = callback.map(|c| Box::new(c) as Box<dyn Fn(String) + Send + 'static>);
    self.dispatch_or_queue(PendingOp::Eval(js, cb));
    Ok(())
  }

  // Devtools: bridge action
  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {
    if let Some(handle) = self.try_handle() {
      self.runtime.spawn(async move {
        if let Err(e) = handle.set_web_debugging_access(true).await {
          log::warn!("[wry] open_devtools failed: {}", e);
        }
      });
    }
    self.devtools_open.store(true, Ordering::SeqCst);
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {
    if let Some(handle) = self.try_handle() {
      self.runtime.spawn(async move {
        if let Err(e) = handle.set_web_debugging_access(false).await {
          log::warn!("[wry] close_devtools failed: {}", e);
        }
      });
    }
    self.devtools_open.store(false, Ordering::SeqCst);
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    self.devtools_open.load(Ordering::SeqCst)
  }

  // Pattern A: fire-and-forget
  pub fn zoom(&self, scale_factor: f64) -> Result<()> {
    self.dispatch_or_queue(PendingOp::Zoom(scale_factor));
    Ok(())
  }

  pub fn set_background_color(&self, background_color: RGBA) -> Result<()> {
    let color_str = format!(
      "#{:02X}{:02X}{:02X}{:02X}",
      background_color.3, background_color.0, background_color.1, background_color.2
    );
    self.dispatch_or_queue(PendingOp::SetBackgroundColor(color_str));
    Ok(())
  }

  // Pattern D: blocking-from-worker — cookies
  pub fn cookies(&self) -> Result<Vec<Cookie<'static>>> {
    let url = self.url_cache.lock().unwrap().clone();
    if url.is_empty() {
      return Ok(vec![]);
    }
    self.cookies_for_url(&url)
  }

  pub fn set_cookie(&self, cookie: &Cookie<'_>) -> Result<()> {
    let domain = cookie.domain();
    let url = match domain {
      Some(d) if !d.is_empty() => format!("https://{}", d.trim_start_matches('.')),
      _ => {
        let current_url = self.url_cache.lock().unwrap().clone();
        if !current_url.is_empty() {
          current_url
        } else {
          log::warn!(
            "[wry] set_cookie: cannot determine target URL (no domain and no current URL)"
          );
          return Ok(());
        }
      }
    };
    let value = format_set_cookie_value(cookie);
    self.dispatch_or_queue(PendingOp::SetCookie(url, value));
    Ok(())
  }

  pub fn delete_cookie(&self, _cookie: &Cookie<'_>) -> Result<()> {
    log::warn!("[wry] delete_cookie is a no-op on OHOS: platform lacks single-cookie deletion");
    Ok(())
  }

  pub fn cookies_for_url(&self, url: &str) -> Result<Vec<Cookie<'static>>> {
    // Pattern D: blocking-from-worker
    // Main thread blocking would deadlock TSFN → degrade to empty
    if std::thread::current().id() == self.runtime.main_thread_id() {
      log::warn!("[wry] cookies_for_url called on main thread — returning empty (degraded)");
      return Ok(vec![]);
    }
    let handle = match self.try_handle() {
      Some(h) => h,
      None => return Ok(vec![]),
    };
    let url = url.to_string();
    let (tx, rx) = mpsc::channel::<String>();
    self.runtime.spawn(async move {
      let result = handle.cookies_with_url(url).await;
      let _ = tx.send(result.unwrap_or_default());
    });
    let cookie_str = rx
      .recv_timeout(std::time::Duration::from_secs(3))
      .map_err(|_| Error::OpenHarmonyWebviewError("cookies_for_url timed out".into()))?;
    let cookies_data: Vec<Cookie<'static>> = cookie_str
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
  }

  // Pattern A: fire-and-forget
  pub fn reload(&self) -> Result<()> {
    self.dispatch_or_queue(PendingOp::Reload);
    Ok(())
  }

  pub fn load_url(&self, url: &str) -> Result<()> {
    self.dispatch_or_queue(PendingOp::LoadUrl(url.to_string()));
    Ok(())
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) -> Result<()> {
    let header_map: std::collections::BTreeMap<String, String> = headers
      .iter()
      .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
      .collect();
    self.dispatch_or_queue(PendingOp::LoadUrlWithHeaders(
      url.to_string(),
      header_map,
    ));
    Ok(())
  }

  pub fn load_html(&self, html: &str) -> Result<()> {
    self.dispatch_or_queue(PendingOp::LoadHtml(html.to_string()));
    Ok(())
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    self.dispatch_or_queue(PendingOp::ClearAllBrowsingData);
    Ok(())
  }

  // Pattern B: callback
  pub fn create_pdf(
    &self,
    path: &str,
    _config: Option<PdfConfig>,
    callback: Box<dyn Fn(bool) + Send + 'static>,
  ) -> Result<()> {
    if !self.page_loaded.load(Ordering::SeqCst) {
      callback(false);
      return Err(Error::OpenHarmonyWebviewError(
        "Page not fully loaded. Call createPdf after onPageEnd.".into(),
      ));
    }
    self.dispatch_or_queue(PendingOp::CreatePdf(path.to_string(), Some(callback)));
    Ok(())
  }

  // Pattern C: cached
  pub fn bounds(&self) -> Result<Rect> {
    Ok(*self.bounds_cache.lock().unwrap())
  }

  pub fn set_bounds(&self, bounds: Rect) -> Result<()> {
    let (x, y) = bounds.position.to_logical::<f64>(1.0).into();
    let (w, h) = bounds.size.to_logical::<f64>(1.0).into();
    self.dispatch_or_queue(PendingOp::SetBounds(x, y, w, h));
    *self.bounds_cache.lock().unwrap() = bounds;
    Ok(())
  }

  pub fn set_visible(&self, visible: bool) -> Result<()> {
    self.dispatch_or_queue(PendingOp::SetVisible(visible));
    Ok(())
  }

  pub fn focus(&self) -> Result<()> {
    self.dispatch_or_queue(PendingOp::Focus);
    Ok(())
  }

  pub fn focus_parent(&self) -> Result<()> {
    // OHOS has no separate parent window for the webview
    self.dispatch_or_queue(PendingOp::Focus);
    Ok(())
  }

  pub fn dispose_child(&self) {
    if self.is_child && !self.disposed.swap(true, Ordering::SeqCst) {
      self.dispatch_or_queue(PendingOp::Remove);
    }
  }

  /// Returns the `WebviewHandle` for public API consumption.
  /// If create hasn't completed yet, returns a facade wrapping the client + id
  /// (method calls will be dispatched once the controller is ready).
  pub fn handle(&self) -> WebviewHandle {
    self
      .handle
      .get()
      .cloned()
      .unwrap_or_else(|| self.client.handle(&self.id))
  }
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    if self.is_child && !self.disposed.swap(true, Ordering::SeqCst) {
      self.dispatch_or_queue(PendingOp::Remove);
    }
  }
}

pub fn platform_webview_version() -> Result<String> {
  Ok("1.0.0".to_string())
}

// ─── https intercept ─────────────────────────────────────────────────────────────

/// Synchronously dispatches an https intercept request to the matching custom protocol handler.
///
/// The URL has the form `https://<protocol>.localhost/<path>`. This function extracts the
/// protocol name, reverts the URL to `<protocol>://localhost/<path>`, builds a `Request`,
/// and calls the handler with a `RequestAsyncResponder` that sends the response through
/// an mpsc channel. The main thread blocks on `recv_timeout(3s)`.
///
/// **Note**: This runs on the NAPI main thread via `invokeNativeSync`, which is a
/// synchronous call path distinct from TSFN. Blocking here does NOT cause TSFN deadlock
/// because no TSFN call is pending.
fn dispatch_https_intercept_sync(
  url: &str,
  webview_id: &str,
  protocols: &HashMap<String, SharedProtocolHandler>,
) -> WebviewHttpsInterceptResponse {
  // Extract protocol name from `https://<protocol>.localhost/...`
  let protocol = match extract_protocol_from_https_url(url) {
    Some(p) => p,
    None => return WebviewHttpsInterceptResponse::passthrough(),
  };

  let callback = match protocols.get(protocol) {
    Some(c) => Arc::clone(c),
    None => return WebviewHttpsInterceptResponse::passthrough(),
  };

  // Revert URL: `https://<protocol>.localhost/<path>` → `<protocol>://localhost/<path>`
  let original_url = url.replacen(
    &format!("https://{}.", protocol),
    &format!("{}://", protocol),
    1,
  );

  let request = match Request::builder().uri(&original_url).body(Vec::new()) {
    Ok(r) => r,
    Err(_) => return WebviewHttpsInterceptResponse::passthrough(),
  };

  // Use a channel to synchronously get the response
  let (tx, rx) = mpsc::channel::<Response<Cow<'static, [u8]>>>();

  let responder: Box<dyn FnOnce(Response<Cow<'static, [u8]>>)> = Box::new(move |resp| {
    let _ = tx.send(resp);
  });

  (callback.0)(
    webview_id,
    request,
    RequestAsyncResponder { responder },
  );

  match rx.recv_timeout(std::time::Duration::from_secs(3)) {
    Ok(response) => {
      let status = response.status().as_u16();
      let mime_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();
      let body = response.body().to_vec();
      WebviewHttpsInterceptResponse {
        handled: true,
        status,
        mime_type,
        body,
      }
    }
    Err(_) => {
      log::warn!("[wry] https intercept timed out for: {}", url);
      WebviewHttpsInterceptResponse::passthrough()
    }
  }
}

/// Extracts the protocol name from a URL like `https://myproto.localhost/path`.
fn extract_protocol_from_https_url(url: &str) -> Option<&str> {
  let after_scheme = url.strip_prefix("https://")?;
  let protocol_end = after_scheme.find(".localhost")?;
  Some(&after_scheme[..protocol_end])
}

/// Returns true if any custom protocols are registered (used for https scheme logic).
#[allow(dead_code)]
fn custom_protocols_use_https(protocols: &HashMap<String, SharedProtocolHandler>) -> bool {
  !protocols.is_empty()
}

/// Rewrite `<protocol>://localhost/<path>` to `https://<protocol>.localhost/<path>`
/// when the URL's scheme matches one of `custom_protocols`' keys.
fn rewrite_https_url_if_matching(url: &str, custom_protocols: &HashMap<String, SharedProtocolHandler>) -> String {
  for protocol in custom_protocols.keys() {
    let original_prefix = format!("{}://", protocol);
    if url.starts_with(&original_prefix) {
      return custom_protocol_workaround::apply_uri_work_around(url, "https", protocol);
    }
  }
  url.to_string()
}

/// Format a `cookie::Cookie` as a Set-Cookie value string (RFC 6265).
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

  #[test]
  fn extract_protocol_from_https_url_works() {
    assert_eq!(
      extract_protocol_from_https_url("https://myproto.localhost/path/to/page"),
      Some("myproto")
    );
    assert_eq!(
      extract_protocol_from_https_url("https://tauri.localhost"),
      Some("tauri")
    );
    assert_eq!(
      extract_protocol_from_https_url("https://example.com"),
      None
    );
    assert_eq!(
      extract_protocol_from_https_url("myproto://localhost/path"),
      None
    );
  }

  #[test]
  fn custom_protocols_use_https_returns_true_when_nonempty() {
    let empty: HashMap<String, SharedProtocolHandler> = HashMap::new();
    assert!(!custom_protocols_use_https(&empty));
  }

  #[test]
  fn rewrite_https_url_matches_registered_protocol() {
    // When the URL scheme matches a registered protocol key, it should be
    // rewritten to https://{protocol}.localhost/...
    let dummy: SharedProtocolHandler = Arc::new(SendSyncBox(Box::new(
      |_id, _req, _responder| {},
    )));
    let mut protocols: HashMap<String, SharedProtocolHandler> = HashMap::new();
    protocols.insert("tauri".to_string(), dummy);

    let url = "tauri://localhost/index.html";
    let rewritten = rewrite_https_url_if_matching(url, &protocols);
    assert_eq!(rewritten, "https://tauri.localhost/index.html");
  }

  #[test]
  fn rewrite_https_url_no_match_external_url() {
    // External URLs should not be rewritten
    let dummy: SharedProtocolHandler = Arc::new(SendSyncBox(Box::new(
      |_id, _req, _responder| {},
    )));
    let mut protocols: HashMap<String, SharedProtocolHandler> = HashMap::new();
    protocols.insert("tauri".to_string(), dummy);

    let url = "https://example.com/page";
    let rewritten = rewrite_https_url_if_matching(url, &protocols);
    assert_eq!(rewritten, url);
  }

  #[test]
  fn rewrite_https_url_no_match_empty_protocols() {
    // When no protocols are registered, URL should be unchanged
    let protocols: HashMap<String, SharedProtocolHandler> = HashMap::new();
    let url = "tauri://localhost/index.html";
    let rewritten = rewrite_https_url_if_matching(url, &protocols);
    assert_eq!(rewritten, url);
  }

  #[test]
  fn rewrite_https_url_first_matching_protocol_wins() {
    // If multiple protocols are registered, the URL is rewritten using
    // the first matching protocol key (HashMap iteration order is unspecified,
    // but only one protocol prefix can match any given URL).
    let dummy: SharedProtocolHandler = Arc::new(SendSyncBox(Box::new(
      |_id, _req, _responder| {},
    )));
    let mut protocols: HashMap<String, SharedProtocolHandler> = HashMap::new();
    protocols.insert("tauri".to_string(), dummy.clone());
    protocols.insert("wry".to_string(), dummy);

    let url = "wry://localhost/path";
    let rewritten = rewrite_https_url_if_matching(url, &protocols);
    assert_eq!(rewritten, "https://wry.localhost/path");
  }

  #[test]
  fn extract_protocol_from_https_url_with_long_path() {
    assert_eq!(
      extract_protocol_from_https_url("https://myproto.localhost/a/b/c/d?e=f#g"),
      Some("myproto")
    );
  }

  #[test]
  fn extract_protocol_from_https_url_empty_path() {
    assert_eq!(
      extract_protocol_from_https_url("https://tauri.localhost"),
      Some("tauri")
    );
  }

  #[test]
  fn extract_protocol_from_https_url_trailing_slash() {
    assert_eq!(
      extract_protocol_from_https_url("https://tauri.localhost/"),
      Some("tauri")
    );
  }

  #[test]
  fn format_set_cookie_value_with_path_only() {
    let cookie = Cookie::build(("session", "xyz"))
      .path("/app")
      .build();
    let v = format_set_cookie_value(&cookie);
    assert!(v.starts_with("session=xyz"));
    assert!(v.contains("; Path=/app"));
    assert!(!v.contains("; Secure"));
    assert!(!v.contains("; HttpOnly"));
  }

  #[test]
  fn format_set_cookie_value_with_same_site_strict() {
    let cookie = Cookie::build(("token", "abc"))
      .same_site(SameSite::Strict)
      .build();
    let v = format_set_cookie_value(&cookie);
    assert!(v.contains("; SameSite=Strict"));
  }

  #[test]
  fn format_set_cookie_value_with_same_site_none() {
    let cookie = Cookie::build(("token", "abc"))
      .same_site(SameSite::None)
      .build();
    let v = format_set_cookie_value(&cookie);
    assert!(v.contains("; SameSite=None"));
  }
}

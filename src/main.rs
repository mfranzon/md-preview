//! MD Preview: a tiny native markdown editor/viewer. Editable source and rendered output side by side,
//! live-reloading when the file changes on disk.

// No console window behind the app on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use http::{Request, Response};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use pulldown_cmark::{html, Options, Parser};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{DragDropEvent, WebView, WebViewBuilder};

#[cfg(windows)]
use tao::platform::windows::{IconExtWindows, WindowExtWindows};
#[cfg(target_os = "linux")]
use {tao::platform::unix::WindowExtUnix, wry::WebViewBuilderExtUnix};

/// Where the `mdp` custom protocol is served. WebView2 exposes custom protocols as `http://<scheme>.localhost`.
#[cfg(not(windows))]
const ORIGIN: &str = "mdp://localhost";
#[cfg(windows)]
const ORIGIN: &str = "http://mdp.localhost";
const POLL: Duration = Duration::from_millis(400);
const MD_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mdx", "txt"];

const CSP: &str = "default-src 'self' mdp:; img-src * data: mdp:; style-src 'self' mdp: 'unsafe-inline'; \
                   script-src 'self' mdp:; connect-src 'self' mdp:";

static ASSETS: &[(&str, &str, &[u8])] = &[
    ("app.js", "text/javascript", include_bytes!("../assets/app.js")),
    ("app.css", "text/css", include_bytes!("../assets/app.css")),
    ("hljs.js", "text/javascript", include_bytes!("../assets/hljs.js")),
    ("hljs-light.css", "text/css", include_bytes!("../assets/hljs-light.css")),
    ("hljs-dark.css", "text/css", include_bytes!("../assets/hljs-dark.css")),
    ("github-markdown.css", "text/css", include_bytes!("../assets/github-markdown.css")),
];

enum UserEvent {
    Open(Vec<PathBuf>),
    PickFiles,
    Menu(MenuEvent),
    /// Text to write to the window's document; `close` closes the window afterwards.
    Save { id: WindowId, original: String, text: String, close: bool },
    Dirty(WindowId, bool),
}

struct Viewer {
    window: Window,
    webview: WebView,
    doc: Arc<Mutex<Option<PathBuf>>>,
    mtime: Option<SystemTime>,
    dirty: bool,
}

fn main() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));

    let menu = Menu::new();
    let open_item = MenuItem::new("Open…", true, "CmdOrCtrl+O".parse().ok());
    let save_item = MenuItem::new("Save", true, "CmdOrCtrl+S".parse().ok());
    let quit_label = if cfg!(target_os = "macos") { "Quit MD Preview" } else { "Quit" };
    let quit_item = MenuItem::new(quit_label, true, "CmdOrCtrl+Q".parse().ok());
    let split_item = MenuItem::new("Split", true, "CmdOrCtrl+1".parse().ok());
    let raw_item = MenuItem::new("Raw Only", true, "CmdOrCtrl+2".parse().ok());
    let ren_item = MenuItem::new("Rendered Only", true, "CmdOrCtrl+3".parse().ok());
    let find_item = MenuItem::new("Find…", true, "CmdOrCtrl+F".parse().ok());
    let find_next_item = MenuItem::new("Find Next", true, "CmdOrCtrl+G".parse().ok());
    let find_prev_item = MenuItem::new("Find Previous", true, "CmdOrCtrl+Shift+G".parse().ok());
    let file_menu = Submenu::with_items("File", true, &[&open_item, &save_item, &PredefinedMenuItem::separator()]).unwrap();
    let edit_menu = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
            &PredefinedMenuItem::separator(),
            &find_item,
            &find_next_item,
            &find_prev_item,
        ],
    )
    .unwrap();
    let view_menu = Submenu::with_items("View", true, &[&split_item, &raw_item, &ren_item]).unwrap();

    // macOS: one app-wide menu bar, with the usual app and Window menus.
    #[cfg(target_os = "macos")]
    let window_menu = {
        let app_menu = Submenu::with_items(
            "MD Preview",
            true,
            &[
                &PredefinedMenuItem::about(None, None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::separator(),
                &quit_item,
            ],
        )
        .unwrap();
        file_menu.append(&PredefinedMenuItem::close_window(None)).unwrap();
        view_menu.append_items(&[&PredefinedMenuItem::separator(), &PredefinedMenuItem::fullscreen(None)]).unwrap();
        let window_menu = Submenu::with_items("Window", true, &[&PredefinedMenuItem::minimize(None)]).unwrap();
        menu.append_items(&[&app_menu, &file_menu, &edit_menu, &view_menu, &window_menu]).unwrap();
        window_menu
    };
    // Windows and Linux: the same menu bar is attached to every window (see `new_viewer`).
    #[cfg(not(target_os = "macos"))]
    {
        file_menu.append(&quit_item).unwrap();
        menu.append_items(&[&file_menu, &edit_menu, &view_menu]).unwrap();
    }

    let cli_files: Vec<PathBuf> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn")).map(PathBuf::from).collect();

    let mut viewers: HashMap<WindowId, Viewer> = HashMap::new();
    let mut focused: Option<WindowId> = None;
    let started = Instant::now();
    let mut opened_any = false;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);

        match event {
            Event::NewEvents(StartCause::Init) => {
                #[cfg(target_os = "macos")]
                {
                    menu.init_for_nsapp();
                    window_menu.set_as_windows_menu_for_nsapp();
                }
                if !cli_files.is_empty() {
                    let _ = proxy.send_event(UserEvent::Open(cli_files.clone()));
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // Launched without a file (and Finder didn't hand us one): show an empty drop target.
                if !opened_any && viewers.is_empty() && started.elapsed() > Duration::from_millis(500) {
                    opened_any = true;
                    new_viewer(target, &proxy, &menu, &mut viewers, None);
                }
                for v in viewers.values_mut() {
                    let Some(path) = v.doc.lock().unwrap().clone() else { continue };
                    let m = mtime(&path);
                    if m != v.mtime {
                        v.mtime = m;
                        let _ = v.webview.evaluate_script("refresh()");
                    }
                }
            }
            Event::Opened { urls } => {
                let paths = urls.iter().filter_map(|u| u.to_file_path().ok()).collect();
                let _ = proxy.send_event(UserEvent::Open(paths));
            }
            Event::Reopen { has_visible_windows: false, .. } => {
                new_viewer(target, &proxy, &menu, &mut viewers, None);
            }
            Event::UserEvent(UserEvent::Open(paths)) => {
                for p in paths {
                    opened_any = true;
                    open_path(target, &proxy, &menu, &mut viewers, p);
                }
            }
            Event::UserEvent(UserEvent::PickFiles) => {
                // Start in the folder of the document in the focused window, if any.
                let dir = focused
                    .and_then(|id| viewers.get(&id))
                    .and_then(|v| v.doc.lock().unwrap().as_ref().and_then(|p| p.parent().map(Path::to_path_buf)));
                let mut dialog = rfd::FileDialog::new().add_filter("Markdown", MD_EXTS);
                if let Some(dir) = dir {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(paths) = dialog.pick_files() {
                    let _ = proxy.send_event(UserEvent::Open(paths));
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == open_item.id() {
                    let _ = proxy.send_event(UserEvent::PickFiles);
                } else if e.id == quit_item.id() {
                    let unsaved = viewers.values().filter(|v| v.dirty).count();
                    if unsaved == 0 || confirm_quit(unsaved) {
                        *control_flow = ControlFlow::Exit;
                    }
                } else if e.id == save_item.id() {
                    if let Some(v) = focused.and_then(|id| viewers.get(&id)) {
                        let _ = v.webview.evaluate_script("save(false)");
                    }
                } else if let Some(v) = focused.and_then(|id| viewers.get(&id)) {
                    let script = if e.id == split_item.id() {
                        "setMode('')"
                    } else if e.id == raw_item.id() {
                        "setMode('raw-only')"
                    } else if e.id == ren_item.id() {
                        "setMode('ren-only')"
                    } else if e.id == find_item.id() {
                        "openFind()"
                    } else if e.id == find_next_item.id() {
                        "findStep(1)"
                    } else if e.id == find_prev_item.id() {
                        "findStep(-1)"
                    } else {
                        return;
                    };
                    let _ = v.webview.evaluate_script(script);
                }
            }
            Event::UserEvent(UserEvent::Dirty(id, dirty)) => {
                if let Some(v) = viewers.get_mut(&id) {
                    v.dirty = dirty;
                    update_title(v);
                }
            }
            Event::UserEvent(UserEvent::Save { id, original, text, close }) => {
                let Some(v) = viewers.get_mut(&id) else { return };
                let saved = save(v, &original, &text);
                if saved && close {
                    close_viewer(&menu, &mut viewers, id, control_flow);
                } else {
                    let _ = v.webview.evaluate_script(&format!("saveFinished({saved})"));
                }
            }
            Event::WindowEvent { window_id, event, .. } => match event {
                WindowEvent::Focused(true) => focused = Some(window_id),
                WindowEvent::CloseRequested => {
                    let Some(v) = viewers.get(&window_id) else { return };
                    if !v.dirty {
                        close_viewer(&menu, &mut viewers, window_id, control_flow);
                        return;
                    }
                    match confirm_close(v) {
                        CloseChoice::Save => {
                            let _ = v.webview.evaluate_script("save(true)");
                        }
                        CloseChoice::Discard => close_viewer(&menu, &mut viewers, window_id, control_flow),
                        CloseChoice::Cancel => {}
                    }
                }
                _ => {}
            },
            _ => {}
        }
    });
}

#[cfg_attr(not(windows), allow(unused_variables))]
fn close_viewer(menu: &Menu, viewers: &mut HashMap<WindowId, Viewer>, id: WindowId, control_flow: &mut ControlFlow) {
    if let Some(v) = viewers.remove(&id) {
        // Detach the shared menu first: Windows destroys a window's menu along with the window.
        #[cfg(windows)]
        unsafe {
            let _ = menu.remove_for_hwnd(v.window.hwnd() as _);
        }
        drop(v);
    }
    if viewers.is_empty() {
        *control_flow = ControlFlow::Exit;
    }
}

enum CloseChoice {
    Save,
    Discard,
    Cancel,
}

fn confirm_close(v: &Viewer) -> CloseChoice {
    let name = v.doc.lock().unwrap().as_deref().map(title_for).unwrap_or_default();
    let (save, discard, cancel) = ("Save".to_string(), "Don't Save".to_string(), "Cancel".to_string());
    let res = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Unsaved changes")
        .set_description(format!("Do you want to save the changes you made to “{name}”?"))
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(save.clone(), discard.clone(), cancel))
        .set_parent(&v.window)
        .show();
    match res {
        rfd::MessageDialogResult::Yes => CloseChoice::Save,
        rfd::MessageDialogResult::No => CloseChoice::Discard,
        rfd::MessageDialogResult::Custom(s) if s == save => CloseChoice::Save,
        rfd::MessageDialogResult::Custom(s) if s == discard => CloseChoice::Discard,
        _ => CloseChoice::Cancel,
    }
}

fn confirm_quit(unsaved: usize) -> bool {
    let quit = "Quit Anyway".to_string();
    let docs = if unsaved == 1 { "1 document has".to_string() } else { format!("{unsaved} documents have") };
    let res = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Unsaved changes")
        .set_description(format!("{docs} unsaved changes. Quit and discard them?"))
        .set_buttons(rfd::MessageButtons::OkCancelCustom(quit.clone(), "Cancel".into()))
        .show();
    match res {
        rfd::MessageDialogResult::Ok => true,
        rfd::MessageDialogResult::Custom(s) => s == quit,
        _ => false,
    }
}

fn confirm_overwrite(v: &Viewer, path: &Path, deleted: bool) -> bool {
    let name = title_for(path);
    let message = if deleted {
        format!("“{name}” is missing from disk. Recreate it with your edits?")
    } else {
        format!("“{name}” changed on disk. Replace its current contents with your edits?")
    };
    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("File changed on disk")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::YesNo)
        .set_parent(&v.window)
        .show();
    result == rfd::MessageDialogResult::Yes
}

fn read_current(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_edit(v: &Viewer, path: &Path, original: &str, text: &str) -> io::Result<bool> {
    let current = read_current(path)?;
    if current.as_deref() != Some(original.as_bytes()) {
        if !confirm_overwrite(v, path, current.is_none()) {
            return Ok(false);
        }
        if read_current(path)? != current {
            return Err(io::Error::other("The file changed again. Review it before trying to save again."));
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if current.is_some() {
        options.truncate(true);
    } else {
        // If the file reappeared, preserve it and let the user retry.
        options.create_new(true);
    }
    options.open(path)?.write_all(text.as_bytes())?;
    Ok(true)
}

fn save(v: &mut Viewer, original: &str, text: &str) -> bool {
    let Some(path) = v.doc.lock().unwrap().clone() else { return false };
    if text == original {
        return true;
    }
    match write_edit(v, &path, original, text) {
        Ok(saved) => {
            if saved {
                v.mtime = mtime(&path);
            }
            saved
        }
        Err(e) => {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Couldn't save")
                .set_description(format!("{}: {e}", path.display()))
                .set_parent(&v.window)
                .show();
            false
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn update_title(v: &Viewer) {
    let Some(path) = v.doc.lock().unwrap().clone() else { return };
    let title = title_for(&path);
    v.window.set_title(&if v.dirty { format!("{title} (Edited)") } else { title });
}

/// Focus the window already showing `path`, else reuse an empty window, else open a new one.
fn open_path(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: &EventLoopProxy<UserEvent>,
    menu: &Menu,
    viewers: &mut HashMap<WindowId, Viewer>,
    path: PathBuf,
) {
    let path = canonical(path);
    if let Some(v) = viewers.values().find(|v| v.doc.lock().unwrap().as_ref() == Some(&path)) {
        v.window.set_focus();
        return;
    }
    if let Some(v) = viewers.values_mut().find(|v| v.doc.lock().unwrap().is_none()) {
        *v.doc.lock().unwrap() = Some(path.clone());
        v.mtime = mtime(&path);
        let _ = v.webview.evaluate_script("refresh()");
        v.window.set_title(&title_for(&path));
        v.window.set_focus();
        return;
    }
    new_viewer(target, proxy, menu, viewers, Some(path));
}

#[cfg_attr(target_os = "macos", allow(unused_variables))]
fn new_viewer(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: &EventLoopProxy<UserEvent>,
    menu: &Menu,
    viewers: &mut HashMap<WindowId, Viewer>,
    path: Option<PathBuf>,
) {
    let title = path.as_deref().map(title_for).unwrap_or_else(|| "MD Preview".into());
    let builder = WindowBuilder::new().with_title(title).with_inner_size(tao::dpi::LogicalSize::new(1300.0, 850.0));
    // The icon embedded in the .exe by build.rs.
    #[cfg(windows)]
    let builder = builder.with_window_icon(tao::window::Icon::from_resource(1, None).ok());
    let window = builder.build(target).expect("create window");

    #[cfg(windows)]
    unsafe {
        let _ = menu.init_for_hwnd(window.hwnd() as _);
    }
    #[cfg(target_os = "linux")]
    let _ = menu.init_for_gtk_window(window.gtk_window(), window.default_vbox());

    let doc = Arc::new(Mutex::new(path));
    let proto_doc = doc.clone();
    let nav_proxy = proxy.clone();
    let drop_proxy = proxy.clone();
    let ipc_proxy = proxy.clone();
    let id = window.id();

    let builder = WebViewBuilder::new()
        .with_custom_protocol("mdp".into(), move |_id, req| serve(&proto_doc, req))
        .with_ipc_handler(move |req| {
            let body = req.body();
            let event = if body == "open" {
                UserEvent::PickFiles
            } else if let Some(payload) = body.strip_prefix("save:") {
                let Ok((close, original, text)) = serde_json::from_str(payload) else { return };
                UserEvent::Save { id, original, text, close }
            } else if let Some(flag) = body.strip_prefix("dirty:") {
                UserEvent::Dirty(id, flag == "1")
            } else {
                return;
            };
            let _ = ipc_proxy.send_event(event);
        })
        .with_navigation_handler(move |url| handle_navigation(&nav_proxy, &url))
        .with_drag_drop_handler(move |e| {
            if let DragDropEvent::Drop { paths, .. } = e {
                let _ = drop_proxy.send_event(UserEvent::Open(paths));
            }
            true
        })
        .with_url(format!("{ORIGIN}/"));
    // On Linux the webview goes into the window's box, below the menu bar.
    #[cfg(target_os = "linux")]
    let webview = builder.build_gtk(window.default_vbox().expect("window box"));
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window);
    let webview = webview.expect("create webview");

    let mtime = doc.lock().unwrap().as_deref().and_then(mtime);
    viewers.insert(window.id(), Viewer { window, webview, doc, mtime, dirty: false });
}

fn title_for(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Allow the page itself; open other markdown files in new windows; send everything else to the system.
fn handle_navigation(proxy: &EventLoopProxy<UserEvent>, url: &str) -> bool {
    let Some(rest) = url.strip_prefix(ORIGIN) else {
        if url.starts_with("http:") || url.starts_with("https:") || url.starts_with("mailto:") {
            open_external(url);
        }
        return false;
    };
    let path = rest.split(['#', '?']).next().unwrap_or("");
    if path == "/" {
        return true;
    }
    let path = fs_path(path);
    let is_md = path.extension().is_some_and(|e| MD_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()));
    if is_md {
        let _ = proxy.send_event(UserEvent::Open(vec![path]));
    } else if path.exists() {
        open_external(&path);
    }
    false
}

/// Open a URL or file with the system's default application.
fn open_external(target: impl AsRef<std::ffi::OsStr>) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler");
        cmd
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = std::process::Command::new("xdg-open");
    let _ = cmd.arg(target).spawn();
}

/// Canonical form of `path`, minus the `\\?\` prefix Windows adds (it doesn't survive a trip through a URL).
fn canonical(path: PathBuf) -> PathBuf {
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(s) if !s.starts_with("UNC\\") => PathBuf::from(s),
        _ => path,
    }
}

/// The URL path for a file: `/home/me/a.md`, or `/C:/Users/me/a.md` on Windows. Not percent-encoded.
fn url_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) { format!("/{}", s.replace('\\', "/")) } else { s.into_owned() }
}

/// The inverse of `url_path`, for a percent-encoded URL path.
fn fs_path(url_path: &str) -> PathBuf {
    let decoded = percent_decode_str(url_path).decode_utf8_lossy();
    PathBuf::from(if cfg!(windows) { decoded.trim_start_matches('/') } else { &decoded })
}

fn serve(doc: &Mutex<Option<PathBuf>>, req: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let path = req.uri().path();
    match path {
        "/" => respond(200, "text/html; charset=utf-8", include_bytes!("../assets/index.html").as_slice().into()),
        "/__mdp/content" => {
            let json = content_json(doc.lock().unwrap().as_deref());
            respond(200, "application/json", json.into_bytes().into())
        }
        // Live preview of unsaved edits: the request body is the markdown source.
        "/__mdp/render" => {
            let json = serde_json::json!({ "html": render(&String::from_utf8_lossy(req.body())) }).to_string();
            respond(200, "application/json", json.into_bytes().into())
        }
        p if p.starts_with("/__mdp/") => match ASSETS.iter().find(|(n, _, _)| p[7..] == **n) {
            Some((_, mime, data)) => respond(200, mime, Cow::Borrowed(data)),
            None => respond(404, "text/plain", Cow::Borrowed(b"not found")),
        },
        // Anything else is a file referenced by the document (images etc.), by absolute path.
        p => {
            let fs_path = fs_path(p);
            match std::fs::read(&fs_path) {
                Ok(bytes) => respond(200, mime_for(&fs_path), bytes.into()),
                Err(_) => respond(404, "text/plain", Cow::Borrowed(b"not found")),
            }
        }
    }
}

fn respond(status: u16, mime: &str, body: Cow<'static, [u8]>) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header("Content-Type", mime)
        .header("Content-Security-Policy", CSP)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap()
}

fn content_json(path: Option<&Path>) -> String {
    let Some(path) = path else {
        return serde_json::json!({ "empty": true }).to_string();
    };
    let raw = match read_current(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return serde_json::json!({ "error": format!("File missing: {}", path.display()) }).to_string(),
        Err(e) => return serde_json::json!({ "error": format!("{}: {e}", path.display()) }).to_string(),
    };
    let raw = String::from_utf8_lossy(&raw);
    let read_only = matches!(raw, Cow::Owned(_));
    let rendered = render(raw.strip_prefix('\u{feff}').unwrap_or(&raw));
    const PATH_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?').add(b'<').add(b'>');
    let dir = url_path(path.parent().unwrap_or(Path::new("/")));
    let base = format!("{ORIGIN}{}/", utf8_percent_encode(dir.trim_end_matches('/'), PATH_SET));
    serde_json::json!({ "raw": raw, "html": rendered, "base": base, "readOnly": read_only }).to_string()
}

fn render(md: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(md, opts));
    out
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

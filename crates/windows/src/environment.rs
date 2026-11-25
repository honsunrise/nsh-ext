//! WebView2 environments, one browser process per profile folder.
//!
//! The environment is created asynchronously on the UI thread that first
//! asks for it; views created later reuse it. The extension owns its own
//! profile (`%LOCALAPPDATA%\nsh-ext\WebView2`) instead of reaching into
//! NSH's built-in web view state.
//!
//! Portions of the setup come from `nsh-platform-windows` 0.5.1
//! (Apache-2.0, Longbridge); the adapter targets the 0.7.0 contract.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use webview2_com::{
    CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Environment,
        ICoreWebView2EnvironmentOptions,
    },
};
use windows::core::PCWSTR;

type Callback = Box<dyn FnOnce(Option<ICoreWebView2Environment>)>;

enum State {
    /// Waiters for the environment being made.
    Pending(Vec<Callback>),
    /// The environment, shared by every view of the profile.
    Ready(ICoreWebView2Environment),
    /// WebView2 is missing or the environment could not be made.
    Failed,
}

thread_local! {
    /// Environments by profile folder, on the UI thread that owns them.
    static ENVIRONMENTS: RefCell<HashMap<PathBuf, State>> = RefCell::new(HashMap::new());
}

/// Calls `callback` with the environment once it is made (`None` when
/// WebView2 is unavailable).
pub(crate) fn request_environment(callback: Callback) {
    let folder = profile_folder();
    let known = ENVIRONMENTS.with(|environments| match environments.borrow().get(&folder) {
        Some(State::Ready(environment)) => Some(Some(environment.clone())),
        Some(State::Failed) => Some(None),
        _ => None,
    });
    if let Some(environment) = known {
        callback(environment);
        return;
    }
    let starting = ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        match environments.get_mut(&folder) {
            Some(State::Pending(waiting)) => {
                waiting.push(callback);
                false
            }
            _ => {
                environments.insert(folder.clone(), State::Pending(vec![callback]));
                true
            }
        }
    });
    // Outside the borrow: starting may call back synchronously.
    if starting {
        start(&folder);
    }
}

/// Where extension views keep their WebView2 data. The executable's name
/// keeps apps (and NSH's own web view) from sharing one profile folder.
fn profile_folder() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let app = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "nsh-ext".to_owned());
    let app: String = app
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    base.join("nsh-ext").join(app).join("WebView2")
}

/// Makes the environment of `folder`, handing it to everything waiting.
fn start(folder: &Path) {
    let key = folder.to_owned();
    let wide = wide(folder.as_os_str());
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, environment| {
            let environment = match result {
                Ok(()) => environment,
                Err(error) => {
                    log::warn!("nsh-ext webview: no WebView2 environment: {error}");
                    None
                }
            };
            let waiting = ENVIRONMENTS.with(|environments| {
                let mut environments = environments.borrow_mut();
                let ready = match &environment {
                    Some(environment) => State::Ready(environment.clone()),
                    None => State::Failed,
                };
                match environments.insert(key.clone(), ready) {
                    Some(State::Pending(waiting)) => waiting,
                    _ => Vec::new(),
                }
            });
            for callback in waiting {
                callback(environment.clone());
            }
            Ok(())
        },
    ));
    // SAFETY: valid NUL-terminated strings that outlive the call; the
    // handler is an owned COM object WebView2 keeps until it calls it.
    let started = unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR::from_raw(wide.as_ptr()),
            None::<&ICoreWebView2EnvironmentOptions>,
            &handler,
        )
    };
    if let Err(error) = started {
        log::warn!("nsh-ext webview: cannot start WebView2: {error}");
        let waiting = ENVIRONMENTS.with(|environments| {
            match environments
                .borrow_mut()
                .insert(folder.to_owned(), State::Failed)
            {
                Some(State::Pending(waiting)) => waiting,
                _ => Vec::new(),
            }
        });
        for callback in waiting {
            callback(None);
        }
    }
}

/// `path` as a NUL-terminated UTF-16 string.
fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(std::iter::once(0)).collect()
}

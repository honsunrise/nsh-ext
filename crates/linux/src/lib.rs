//! Linux GTK 4 / WebKitGTK 6 loading-progress adapter for [`nsh_ext`].
//!
//! [`register`] installs an extension-owned native component. It uses only
//! released NSH public embedding APIs, never NSH's private WebView state.
//! Each explicit `src` change retires the old browser before creating its
//! replacement. This trades cross-source history and browser warm-up for
//! unambiguous callback ownership: WebKit's stop operation does not always
//! deliver a terminal callback that can serve as a drain barrier.
//!
//! The stable component widget owns the changing browser child. Clearing a
//! source also works while hidden; a new non-empty source waits for mapping.
//! Page-originated navigations remain in their browser and are not replayed.
//! `trackProgress` gates emission only, and estimates are the engine's own.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use nsh_core::element::{Registry, native_components, native_module};
use nsh_core::{JsonValue, geometry::Percentage};
use nsh_ext::{COMPONENT_NAME, LoadProgress, PROGRESS_EVENT, ProgressTracker};
use nsh_platform_linux::{ComponentContext, Components, NativeComponent, gtk4 as gtk};
use webkit6::{LoadEvent, NetworkError, PolicyError, WebView, prelude::*};

/// Registers the browser component on the registry used by an NSH app view.
/// Call before loading the app. Registered native components are trusted
/// host code; the embedding app must authorize sources supplied by modules.
pub fn register(components: &Components) {
    components.register(
        COMPONENT_NAME,
        Rc::new(|context| Box::new(Component::new(context))),
    );
}

/// The adapter's TSX UI set: it adds no elements of its own, because the
/// typed `WebView` element belongs to [`nsh_ext`], which an app lists beside
/// this crate. NSH requires a set for every crate `nsh.json` names.
#[native_module("nsh-ext-linux")]
fn native(_registry: &mut Registry) {}

/// Registers the adapter on an NSH Linux host: `nsh run`, `nsh test
/// --target linux` and packaged apps call this for every view.
#[native_components(linux)]
fn components(components: &Components) {
    register(components);
}

struct Component {
    shared: Rc<Shared>,
    mapped: Option<glib::SignalHandlerId>,
}

impl Component {
    fn new(context: ComponentContext) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        let shared = Rc::new(Shared {
            root,
            context,
            browser: RefCell::new(None),
            tracker: RefCell::new(ProgressTracker::default()),
            source: RefCell::new(String::new()),
            pending: Cell::new(false),
            scheduled: Cell::new(false),
            epoch: Cell::new(0),
            track: Cell::new(false),
            alive: Cell::new(true),
        });
        let weak = Rc::downgrade(&shared);
        let mapped = shared.root.connect_map(move |_| {
            if let Some(shared) = weak.upgrade() {
                shared.schedule();
            }
        });
        Self {
            shared,
            mapped: Some(mapped),
        }
    }
}

impl NativeComponent for Component {
    fn widget(&self) -> gtk::Widget {
        self.shared.root.clone().upcast()
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        match name {
            "src" => {
                let source = value.and_then(JsonValue::as_str).unwrap_or_default();
                if *self.shared.source.borrow() != source {
                    *self.shared.source.borrow_mut() = source.to_owned();
                    self.shared.pending.set(true);
                    self.shared.schedule();
                }
            }
            "trackProgress" => self
                .shared
                .track
                .set(value.and_then(JsonValue::as_bool).unwrap_or(false)),
            _ => {}
        }
    }
}

impl Drop for Component {
    fn drop(&mut self) {
        self.shared.alive.set(false);
        if let Some(mapped) = self.mapped.take() {
            self.shared.root.disconnect(mapped);
        }
        self.shared.retire();
    }
}

struct Browser {
    web: WebView,
    handlers: Vec<glib::SignalHandlerId>,
}

struct Shared {
    root: gtk::Box,
    context: ComponentContext,
    browser: RefCell<Option<Browser>>,
    tracker: RefCell<ProgressTracker>,
    source: RefCell<String>,
    pending: Cell<bool>,
    scheduled: Cell<bool>,
    epoch: Cell<u64>,
    track: Cell<bool>,
    alive: Cell<bool>,
}

impl Shared {
    fn emit(&self, event: Option<LoadProgress>) {
        if self.alive.get()
            && self.track.get()
            && let Some(event) = event
        {
            self.context.emit(PROGRESS_EVENT, event.to_value());
        }
    }

    fn current(&self, epoch: u64) -> bool {
        self.alive.get() && self.epoch.get() == epoch
    }

    fn schedule(self: &Rc<Self>) {
        if !self.alive.get() || !self.pending.get() || self.scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(shared) = weak.upgrade() {
                shared.scheduled.set(false);
                shared.apply();
            }
        });
    }

    fn retire(&self) {
        // Invalidate callbacks before touching the native browser. Stopping it
        // may call back synchronously, or produce no terminal callback at all.
        self.epoch.set(
            self.epoch
                .get()
                .checked_add(1)
                .expect("browser identity exhausted"),
        );
        let old = self.browser.borrow_mut().take();
        let cancelled = {
            let mut tracker = self.tracker.borrow_mut();
            let navigation = tracker.navigation();
            tracker.cancel(navigation)
        };
        self.emit(cancelled);
        if let Some(mut old) = old {
            for handler in old.handlers.drain(..) {
                old.web.disconnect(handler);
            }
            old.web.stop_loading();
            self.root.remove(&old.web);
            // Releasing our last browser reference retires the old document,
            // including its scripts. The NSH host holds only the stable root.
        }
    }

    fn apply(self: &Rc<Self>) {
        if !self.alive.get() || !self.pending.get() {
            return;
        }
        self.retire();
        let source = self.source.borrow().clone();
        if source.is_empty() {
            self.pending.set(false);
            return;
        }
        if !self.root.is_mapped() {
            return;
        }
        self.pending.set(false);
        let epoch = self.epoch.get();
        let web = WebView::new();
        web.set_hexpand(true);
        web.set_vexpand(true);
        let mut handlers = Vec::with_capacity(4);
        let weak = Rc::downgrade(self);
        handlers.push(web.connect_load_changed(move |web, event| {
            let Some(shared) = weak.upgrade().filter(|shared| shared.current(epoch)) else {
                return;
            };
            match event {
                LoadEvent::Started => {
                    let (cancelled, started) = {
                        let mut tracker = shared.tracker.borrow_mut();
                        let navigation = tracker.navigation();
                        (tracker.cancel(navigation), tracker.start())
                    };
                    shared.emit(cancelled);
                    shared.emit(Some(started));
                }
                LoadEvent::Finished if !web.is_loading() => {
                    let finished = {
                        let mut tracker = shared.tracker.borrow_mut();
                        let navigation = tracker.navigation();
                        tracker.finish(navigation)
                    };
                    shared.emit(finished);
                }
                _ => {}
            }
        }));
        let weak = Rc::downgrade(self);
        handlers.push(web.connect_load_failed(move |web, _, _, error| {
            let Some(shared) = weak.upgrade().filter(|shared| shared.current(epoch)) else {
                return true;
            };
            // WebKit commits native failure state before this signal. A
            // superseded in-page load must not end a newer load in progress.
            if !web.is_loading() {
                let event = {
                    let mut tracker = shared.tracker.borrow_mut();
                    let navigation = tracker.navigation();
                    if error.matches(NetworkError::Cancelled)
                        || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
                    {
                        tracker.cancel(navigation)
                    } else {
                        tracker.fail(
                            navigation,
                            if shared.track.get() {
                                error.message().to_owned()
                            } else {
                                String::new()
                            },
                        )
                    }
                };
                shared.emit(event);
            }
            true
        }));
        let weak = Rc::downgrade(self);
        handlers.push(web.connect_estimated_load_progress_notify(move |web| {
            let Some(shared) = weak
                .upgrade()
                .filter(|shared| shared.current(epoch) && shared.track.get())
            else {
                return;
            };
            if !web.is_loading() {
                return;
            }
            let estimate = web.estimated_load_progress();
            if !estimate.is_finite() || !(0.0..=1.0).contains(&estimate) {
                return;
            }
            let event = {
                let mut tracker = shared.tracker.borrow_mut();
                let navigation = tracker.navigation();
                tracker.estimate(navigation, Percentage(estimate as f32))
            };
            shared.emit(event);
        }));
        let weak = Rc::downgrade(self);
        handlers.push(web.connect_web_process_terminated(move |_, reason| {
            let Some(shared) = weak.upgrade().filter(|shared| shared.current(epoch)) else {
                return;
            };
            let failed = {
                let mut tracker = shared.tracker.borrow_mut();
                let navigation = tracker.navigation();
                tracker.fail(
                    navigation,
                    if shared.track.get() {
                        format!("web process terminated: {reason:?}")
                    } else {
                        String::new()
                    },
                )
            };
            shared.emit(failed);
            shared.retire();
        }));
        *self.browser.borrow_mut() = Some(Browser {
            web: web.clone(),
            handlers,
        });
        self.root.append(&web);
        // GTK allocates the new child before the next default idle. Avoid
        // loading into an unallocated native child during the mount batch.
        let weak = Rc::downgrade(self);
        let weak_web = web.downgrade();
        glib::idle_add_local_once(move || {
            if let (Some(shared), Some(web)) = (weak.upgrade(), weak_web.upgrade())
                && shared.current(epoch)
            {
                web.load_uri(&source);
            }
        });
    }
}

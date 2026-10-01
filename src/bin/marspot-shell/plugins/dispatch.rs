//! The one way a hook gets called.
//!
//! The log namespace and `state_dir()` ask the host "which plugin is
//! running right now?", and the answer is set by whoever is about to call
//! the hook. That made it a method on the trait every plugin holds, and
//! the implementation wrote whatever it was given -- it did not check that
//! the caller was the plugin it named. A plugin could call it inside its
//! own hook and spend the rest of that hook writing state and logs under
//! another plugin's name.
//!
//! So the methods take a [`Dispatching`], whose field is private to
//! this module. A private field is visible to the defining module *and
//! its descendants* -- which is why this is a module of its own rather
//! than a type in `plugins`: the plugins are children of `plugins`,
//! and would have been able to make one.
//!
//! Nothing outside here can construct it, and nothing here hands one
//! out; [`with`] is the only way it is ever made.

use super::PluginHost;

/// Proof that the registry is the caller. See the module docs.
pub struct Dispatching(());

/// Run `f` with `name` as the plugin the host answers questions about.
///
/// The clear happens even if `f` panics, so a plugin that dies inside a
/// hook does not leave its name installed for whatever the supervisor
/// does next.
pub fn with<R>(host: &dyn PluginHost, name: &'static str, f: impl FnOnce() -> R) -> R {
    struct Clear<'a>(&'a dyn PluginHost, Dispatching);
    impl Drop for Clear<'_> {
        fn drop(&mut self) {
            self.0.clear_active_plugin(&self.1);
        }
    }
    host.set_active_plugin(&Dispatching(()), name);
    let _clear = Clear(host, Dispatching(()));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The six methods with no default, so a spy can be about the two
    /// that matter.
    macro_rules! rest_of_the_host {
        () => {
            fn pane_count(&self) -> usize {
                0
            }
            fn pane_pty_device(&self, _p: usize) -> Result<Option<std::path::PathBuf>, crate::plugins::PluginError> {
                Ok(None)
            }
            fn pane_pty_pid_tree(
                &self,
                _p: usize,
            ) -> Result<Vec<crate::plugins::PtyChild>, crate::plugins::PluginError> {
                Ok(Vec::new())
            }
            fn pane_focused(&self) -> Option<usize> {
                None
            }
            fn state_dir(&self) -> Result<std::path::PathBuf, crate::plugins::PluginError> {
                Ok(std::path::PathBuf::new())
            }
            fn log(&self, _l: crate::plugins::LogLevel, _t: &str, _m: &str) {}
        };
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    /// The clear has to happen even when the hook dies, or a plugin
    /// that panics leaves its name and its permissions installed for
    /// whatever the supervisor does next.
    #[test]
    fn a_panicking_hook_still_gives_the_name_back() {
        struct Spy(AtomicBool);
        impl PluginHost for Spy {
            fn clear_active_plugin(&self, _who: &Dispatching) {
                self.0.store(true, Ordering::Relaxed);
            }
            rest_of_the_host!();
        }
        let spy = Spy(AtomicBool::new(false));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with(&spy, "t", || panic!("the hook dies"));
        }));
        assert!(r.is_err(), "the panic has to reach the caller");
        assert!(spy.0.load(Ordering::Relaxed), "and the name has to be back");
    }

    #[test]
    fn the_name_is_set_before_the_hook_runs() {
        use std::sync::Mutex;
        struct Spy(Mutex<Vec<&'static str>>);
        impl PluginHost for Spy {
            fn set_active_plugin(&self, _w: &Dispatching, name: &'static str) {
                self.0.lock().unwrap().push(name);
            }
            fn clear_active_plugin(&self, _w: &Dispatching) {
                self.0.lock().unwrap().push("<cleared>");
            }
            rest_of_the_host!();
        }
        let spy = Spy(Mutex::new(Vec::new()));
        with(&spy, "t", || {
            spy.0.lock().unwrap().push("<hook>");
        });
        assert_eq!(*spy.0.lock().unwrap(), ["t", "<hook>", "<cleared>"]);
    }
}

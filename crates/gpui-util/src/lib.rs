use std::{
    ffi::OsStr,
    fmt::{Debug, Display},
    future::Future,
    hash::BuildHasherDefault,
    ops::AddAssign,
    panic::Location,
    path::PathBuf,
    process::Command,
    time::Instant,
};

pub mod arc_cow {
    use std::{borrow::Borrow, ops::Deref, sync::Arc};

    #[derive(Clone, Debug)]
    pub enum ArcCow<'a, T: ?Sized> {
        Borrowed(&'a T),
        Owned(Arc<T>),
    }

    impl<T: ?Sized> ArcCow<'_, T> {
        fn inner(&self) -> &T {
            match self {
                Self::Borrowed(value) => value,
                Self::Owned(value) => value,
            }
        }
    }

    impl<T: ?Sized> AsRef<T> for ArcCow<'_, T> {
        fn as_ref(&self) -> &T {
            self.inner()
        }
    }

    impl<T: ?Sized> Borrow<T> for ArcCow<'_, T> {
        fn borrow(&self) -> &T {
            self.inner()
        }
    }

    impl<T: ?Sized> Deref for ArcCow<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            self.inner()
        }
    }

    impl<'a, T: ?Sized> From<&'a T> for ArcCow<'a, T> {
        fn from(value: &'a T) -> Self {
            Self::Borrowed(value)
        }
    }

    impl<T: ?Sized> From<Arc<T>> for ArcCow<'_, T> {
        fn from(value: Arc<T>) -> Self {
            Self::Owned(value)
        }
    }
}

pub type TypeIdHashBuilder = BuildHasherDefault<std::collections::hash_map::DefaultHasher>;

#[derive(Debug)]
pub struct Deferred<F: FnOnce()> {
    callback: Option<F>,
}

impl<F: FnOnce()> Deferred<F> {
    pub fn cancel(mut self) {
        self.callback = None;
    }
}

impl<F: FnOnce()> Drop for Deferred<F> {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            callback();
        }
    }
}

pub fn defer<F: FnOnce()>(callback: F) -> Deferred<F> {
    Deferred {
        callback: Some(callback),
    }
}

pub trait ResultExt<T> {
    fn log_err(self) -> Option<T>;
    fn warn_on_err(self) -> Option<T>;
    fn log_with_level(self, level: log::Level) -> Option<T>;
}

impl<T, E: Display> ResultExt<T> for Result<T, E> {
    #[track_caller]
    fn log_err(self) -> Option<T> {
        self.log_with_level(log::Level::Error)
    }

    #[track_caller]
    fn warn_on_err(self) -> Option<T> {
        self.log_with_level(log::Level::Warn)
    }

    #[track_caller]
    fn log_with_level(self, level: log::Level) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                let location = Location::caller();
                log::log!(
                    level,
                    "{} at {}:{}",
                    error,
                    location.file(),
                    location.line()
                );
                None
            }
        }
    }
}

pub trait TryFutureExt<T, E>: Future<Output = Result<T, E>> + Sized {
    fn log_tracked_err(self, location: Location<'static>) -> impl Future<Output = Option<T>>
    where
        E: Display,
    {
        async move {
            match self.await {
                Ok(value) => Some(value),
                Err(error) => {
                    log::error!("{} at {}:{}", error, location.file(), location.line());
                    None
                }
            }
        }
    }
}

impl<F, T, E> TryFutureExt<T, E> for F where F: Future<Output = Result<T, E>> + Sized {}

pub trait TryFutureExtBacktrace<T, E>: Future<Output = Result<T, E>> + Sized {
    fn log_tracked_err_with_backtrace(
        self,
        location: Location<'static>,
    ) -> impl Future<Output = Option<T>>
    where
        E: Debug,
    {
        async move {
            match self.await {
                Ok(value) => Some(value),
                Err(error) => {
                    log::error!("{error:?} at {}:{}", location.file(), location.line());
                    None
                }
            }
        }
    }
}

impl<F, T, E> TryFutureExtBacktrace<T, E> for F where F: Future<Output = Result<T, E>> + Sized {}

pub fn post_inc<T>(value: &mut T) -> T
where
    T: Copy + AddAssign + From<u8>,
{
    let previous = *value;
    *value += T::from(1);
    previous
}

pub fn measure<T>(label: &str, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let output = operation();
    log::trace!("{label}: {:?}", started.elapsed());
    output
}

pub fn new_std_command(program: impl AsRef<OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(target_os = "windows"))]
    {
        command
    }
}

#[must_use]
pub fn get_windows_system_shell() -> PathBuf {
    let windows_directory = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(windows_directory)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

#[macro_export]
macro_rules! debug_panic {
    ($($argument:tt)*) => {{
        if cfg!(debug_assertions) {
            panic!($($argument)*);
        } else {
            log::error!($($argument)*);
        }
    }};
}

#[macro_export]
macro_rules! maybe {
    ($body:block) => {
        (|| $body)()
    };
}

#[cfg(test)]
mod tests {
    use super::{defer, post_inc};
    use std::cell::Cell;

    #[test]
    fn deferred_callback_runs_on_drop() {
        let called = Cell::new(false);
        {
            let _deferred = defer(|| called.set(true));
        }
        assert!(called.get());
    }

    #[test]
    fn post_increment_returns_previous_value() {
        let mut value = 2_u64;
        assert_eq!(post_inc(&mut value), 2);
        assert_eq!(value, 3);
    }
}

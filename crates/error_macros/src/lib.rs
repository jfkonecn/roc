//! Provides macros for consistent reporting of errors in Roc's rust code.
#![no_std]

use core::fmt;

#[cfg(unix)]
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn exit(status: i32) -> !;
}

#[cfg(unix)]
const STDERR_FD: i32 = 2;

#[cfg(windows)]
extern "C" {
    fn GetStdHandle(nStdHandle: i32) -> *mut core::ffi::c_void;
    fn WriteFile(
        hFile: *mut core::ffi::c_void,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: *mut u32,
        lpOverlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn ExitProcess(exit_code: u32) -> !;
}

#[cfg(windows)]
const STD_ERROR_HANDLE: i32 = -12;

/// Print each of the given strings to stderr (if it's available; on wasm, nothing will
/// be printed) and then immediately exit the program with an error.
/// On wasm, this will trap, and on UNIX or Windows it will exit with a code of 1.
#[cfg(any(unix, windows, wasm32))]
pub fn error_and_exit(args: fmt::Arguments) -> ! {
    use fmt::Write;

    struct StderrWriter;

    impl Write for StderrWriter {
        #[cfg(unix)]
        fn write_str(&mut self, s: &str) -> fmt::Result {
            unsafe {
                let _ = write(STDERR_FD, s.as_ptr(), s.len());
            }
            Ok(())
        }

        #[cfg(windows)]
        fn write_str(&mut self, s: &str) -> fmt::Result {
            unsafe {
                let handle = GetStdHandle(STD_ERROR_HANDLE);
                let mut written = 0;
                let _ = WriteFile(
                    handle,
                    s.as_ptr(),
                    s.len() as u32,
                    &mut written,
                    core::ptr::null_mut(),
                );
            }
            Ok(())
        }

        #[cfg(wasm32)]
        fn write_str(&mut self, _s: &str) -> fmt::Result {
            Ok(())
        }
    }

    let _ = fmt::write(&mut StderrWriter, args);

    // Write a newline at the end to make sure stderr gets flushed.
    let _ = StderrWriter.write_str("\n");

    #[cfg(unix)]
    unsafe {
        exit(1)
    }

    #[cfg(windows)]
    unsafe {
        ExitProcess(1)
    }

    #[cfg(wasm32)]
    {
        // We have no way to write to any stderr equivalent in wasm,
        // so just trap to end the program immediately.
        core::arch::wasm32::unreachable()
    }
}

pub const INTERNAL_ERROR_MESSAGE: &str = concat!(
    "An internal compiler expectation was broken.\n",
    "This is definitely a compiler bug.\n",
    "Please file an issue here: <https://github.com/roc-lang/roc/issues/new/choose>\n",
);

/// `internal_error!` should be used whenever a compiler invariant is broken.
/// It is a wrapper around panic that tells the user to file a bug.
/// This should only be used in cases where there would be a compiler bug and the user can't fix it.
/// If there is simply an unimplemented feature, please use `unimplemented!`
/// If there is a user error, please use roc_reporting to print a nice error message.
#[macro_export]
macro_rules! internal_error {
    () => ({
        $crate::error_and_exit(format_args!("{}", $crate::INTERNAL_ERROR_MESSAGE))
    });
    ($($arg:tt)*) => ({
        $crate::error_and_exit(format_args!(
            "{}{}",
            $crate::INTERNAL_ERROR_MESSAGE,
            format_args!($($arg)*)
        ))
    })
}

pub const USER_ERROR_MESSAGE: &str = concat!(
    "We ran into an issue while compiling your code.\n",
    "Sadly, we don't have a pretty error message for this case yet.\n",
    "If you can't figure out the problem from the context below, please reach out at <https://roc.zulipchat.com>\n",
);

/// `user_error!` should only ever be used temporarily.
/// It is a way to document locations where we do not yet have nice error reporting.
/// All cases of `user_error!` should eventually be replaced with pretty error printing using roc_reporting.
#[macro_export]
macro_rules! user_error {
    () => ({
        $crate::error_and_exit(format_args!("{}", $crate::USER_ERROR_MESSAGE))
    });
    ($($arg:tt)*) => ({
        $crate::error_and_exit(format_args!(
            "{}{}",
            $crate::USER_ERROR_MESSAGE,
            format_args!($($arg)*)
        ))
    })
}

/// Assert that a type has the expected size on ARM
#[macro_export]
macro_rules! assert_sizeof_aarch64 {
    ($t: ty, $expected_size: expr) => {
        #[cfg(target_arch = "aarch64")]
        static_assertions::assert_eq_size!($t, [u8; $expected_size]);
    };
}

/// Assert that a type has the expected size in Wasm
#[macro_export]
macro_rules! assert_sizeof_wasm {
    ($t: ty, $expected_size: expr) => {
        #[cfg(target_family = "wasm")]
        static_assertions::assert_eq_size!($t, [u8; $expected_size]);
    };
}

/// Assert that a type has the expected size on any target not covered above
/// In practice we use this for x86_64, and add specific macros for other targets
#[macro_export]
macro_rules! assert_sizeof_default {
    ($t: ty, $expected_size: expr) => {
        #[cfg(not(any(target_family = "wasm", target_arch = "aarch64")))]
        static_assertions::assert_eq_size!($t, [u8; $expected_size]);
    };
}

/// Assert that a type has the expected size on all targets
#[macro_export]
macro_rules! assert_sizeof_all {
    ($t: ty, $expected_size: expr) => {
        static_assertions::assert_eq_size!($t, [u8; $expected_size]);
    };
}

/// Assert that a type has the expected size on all targets except wasm
#[macro_export]
macro_rules! assert_sizeof_non_wasm {
    ($t: ty, $expected_size: expr) => {
        #[cfg(not(target_family = "wasm"))]
        static_assertions::assert_eq_size!($t, [u8; $expected_size]);
    };
}

/// Assert that a type has `Copy`
#[macro_export]
macro_rules! assert_copyable {
    ($t: ty) => {
        static_assertions::assert_impl_all!($t: Copy);
    };
}

// LARGE SCALE PROJECTS
//
// This section is for "todo!"-style macros enabled in sections where large-scale changes to the
// language are in progress.

#[macro_export]
macro_rules! _incomplete_project {
    ($project_name:literal, $tracking_issue_no:literal) => {
        panic!(
            "[{}] not yet implemented. Tracking issue: https://github.com/roc-lang/roc/issues/{}",
            $project_name, $tracking_issue_no,
        )
    };
    ($project_name:literal, $tracking_issue_no:literal, $($arg:tt)+) => {
        panic!(
            "[{}] not yet implemented. Tracking issue: https://github.com/roc-lang/roc/issues/{}.\nAdditional information: {}",
            $project_name, $tracking_issue_no,
            format_args!($($arg)+),
        )
    };
}

#[macro_export]
macro_rules! todo_abilities {
    () => {
        $crate::_incomplete_project!("Abilities", 2463)
    };
    ($($arg:tt)+) => {
        $crate::_incomplete_project!("Abilities", 2463, $($arg)+)
    };
}

#[macro_export]
macro_rules! todo_lambda_erasure {
    () => {
        $crate::_incomplete_project!("Lambda Erasure", 5575)
    };
    ($($arg:tt)+) => {
        $crate::_incomplete_project!("Lambda Erasure", 5575, $($arg)+)
    };
}

// END LARGE SCALE PROJECTS

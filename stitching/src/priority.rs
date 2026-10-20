//! Lowering the processor priority of work that can wait, like rendering
//! recordings, so live previews keep up while it runs.

use std::io;
use std::process::Command;

/// Unix's usual nice value for background work; higher gives way more.
#[cfg(unix)]
const NICENESS: libc::c_int = 10;

/// Has the calling thread give way to others for good: on Linux, taking a
/// thread's priority back up needs privileges.
pub fn lower_this_thread() {
    if let Err(e) = lower_thread() {
        tracing::warn!("couldn't lower a thread's priority: {e}");
    }
}

#[cfg(target_os = "linux")]
fn lower_thread() -> io::Result<()> {
    // SAFETY: only sets the calling thread's nice value, which threads and
    // processes it starts inherit.
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, NICENESS) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn lower_thread() -> io::Result<()> {
    // SAFETY: only sets the calling thread's quality of service.
    match unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) } {
        0 => Ok(()),
        error => Err(io::Error::from_raw_os_error(error)),
    }
}

#[cfg(windows)]
fn lower_thread() -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // SAFETY: the current thread's pseudo handle needs no closing.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Has the process `command` starts give way to others.
#[cfg(unix)]
pub fn lower_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setpriority is async-signal-safe, so it may run between fork
    // and exec.
    unsafe {
        command.pre_exec(|| {
            // Running at the usual priority is no reason not to run.
            libc::setpriority(libc::PRIO_PROCESS, 0, NICENESS);
            Ok(())
        });
    }
}

/// Has the process `command` starts give way to others.
#[cfg(windows)]
pub fn lower_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
    command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
}

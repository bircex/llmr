//! llmr as the container's first process.
//!
//! Process 1 has two jobs no other process does: it reaps orphans, and it passes the stop
//! signal on. llmr needs both, because the command line tools start processes of their own
//! and some outlive the tool that started them.
//!
//! A general init such as tini does both, but it is started with llmr's environment, master
//! key included, and a program that is not llmr cannot be made undumpable: any process of the
//! same user can read `/proc/1/environ`, and the command line tools run as that user. So when
//! llmr finds itself process 1, it stays process 1 as a small init of its own, undumpable,
//! and runs the gateway as its child.

use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
use std::process::ExitCode;

/// Runs the gateway as a child with these arguments, reaps every process that ends up here,
/// and exits with the gateway's status.
pub fn supervise(args: &[String]) -> ExitCode {
    crate::guard_memory();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return crate::fail(&format!("cannot find the llmr executable: {e}")),
    };
    let child = match std::process::Command::new(exe).args(args).spawn() {
        Ok(child) => child,
        Err(e) => return crate::fail(&format!("cannot start the gateway: {e}")),
    };
    let Ok(raw) = i32::try_from(child.id()) else {
        return crate::fail("the gateway's process id does not fit a pid");
    };
    let gateway = Pid::from_raw(raw);
    // Reaped below with everything else, never through `child`.
    drop(child);

    std::thread::spawn(move || forward_signals(gateway));

    loop {
        match waitpid(None::<Pid>, None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == gateway => {
                return ExitCode::from(u8::try_from(code).unwrap_or(1));
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) if pid == gateway => {
                let number = u8::try_from(signal as i32).unwrap_or(0);
                return ExitCode::from(128u8.saturating_add(number));
            }
            // An orphan, now reaped; or a wait interrupted by a signal.
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(e) => return crate::fail(&format!("waiting on the gateway: {e}")),
        }
    }
}

/// Passes `docker stop`'s SIGTERM, and an interrupt or hangup, on to the gateway.
fn forward_signals(gateway: Pid) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) else {
            return;
        };
        loop {
            let forwarded = tokio::select! {
                _ = term.recv() => Signal::SIGTERM,
                _ = int.recv() => Signal::SIGINT,
                _ = hup.recv() => Signal::SIGHUP,
            };
            let _ = kill(gateway, forwarded);
        }
    });
}

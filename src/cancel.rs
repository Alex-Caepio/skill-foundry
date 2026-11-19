use std::sync::atomic::{AtomicI32, Ordering};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

pub fn signal() -> i32 {
    SIGNAL.load(Ordering::Acquire)
}

pub fn interrupted() -> bool {
    signal() != 0
}

#[cfg(unix)]
extern "C" fn handler(signal: i32) {
    SIGNAL.store(signal, Ordering::Release);
}

pub struct Scope {
    #[cfg(unix)]
    previous: Vec<(i32, libc::sigaction)>,
}

impl Scope {
    pub fn install() -> crate::Result<Self> {
        SIGNAL.store(0, Ordering::Release);
        #[cfg(unix)]
        {
            let mut previous = vec![];
            for sig in [libc::SIGINT, libc::SIGTERM] {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = handler as *const () as usize;
                unsafe { libc::sigemptyset(&mut action.sa_mask) };
                action.sa_flags = 0;
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                if unsafe { libc::sigaction(sig, &action, &mut old) } != 0 {
                    for (prev_sig, prev_act) in &previous {
                        unsafe { libc::sigaction(*prev_sig, prev_act, std::ptr::null_mut()) };
                    }
                    return Err(std::io::Error::last_os_error().into());
                }
                previous.push((sig, old));
            }
            Ok(Self { previous })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (sig, previous) in &self.previous {
            unsafe { libc::sigaction(*sig, previous, std::ptr::null_mut()) };
        }
    }
}

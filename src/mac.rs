//! Who macOS thinks this process is.
//!
//! Two identities are stamped on a process when it starts and can never be
//! changed afterwards, and both are inherited by everything it spawns:
//!
//! * the **audit session**, which securityd checks before it will unlock the
//!   login keychain or show an authorisation prompt;
//! * the **responsible process**, which TCC (Full Disk Access, Documents,
//!   Desktop, microphone, ...) attributes every permission check to.
//!
//! A ttmux server is forked from whichever terminal first started it, so it
//! carries that terminal's identity for life, and hands it to every pane it
//! spawns. Quit that terminal and the responsible process is a pid that no
//! longer exists: TCC has nothing to prompt with, so it denies in silence.
//! Nothing in userspace can change either stamp after the fact -- the fix is
//! to spawn a fresh server from a live terminal, which is what
//! [`crate::migrate`] does.
//!
//! Everything here is a report. Off macOS it is all `None`.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::path::PathBuf;

    /// `auditinfo_addr_t` from `<bsm/audit.h>`, checked against the SDK:
    /// 48 bytes, asid at 36, flags at 40.
    #[repr(C)]
    #[derive(Default)]
    struct AuditInfoAddr {
        auid: u32,
        mask: [u32; 2],
        termid: [u32; 6],
        asid: i32,
        flags: u64,
    }

    extern "C" {
        fn getaudit_addr(info: *mut AuditInfoAddr, len: i32) -> i32;
        fn proc_pidpath(pid: i32, buf: *mut libc::c_void, len: u32) -> i32;
    }

    /// The flags of an audit session, in the order `<bsm/audit.h>` lists them.
    const FLAGS: &[(u64, &str)] = &[
        (0x0001, "initial"),
        (0x0010, "graphic-access"),
        (0x0020, "tty"),
        (0x1000, "remote"),
        (0x2000, "console-access"),
        (0x4000, "authenticated"),
    ];

    /// A GUI login session has all of these; anything without them cannot
    /// reach the keychain or raise a prompt.
    const INTERACTIVE: u64 = 0x0010 | 0x2000;

    pub struct Identity {
        pub asid: i32,
        pub flags: u64,
        /// The pid TCC attributes this process's permission checks to.
        pub responsible: Option<i32>,
        /// What that pid is now, if it is still alive.
        pub responsible_path: Option<PathBuf>,
    }

    impl Identity {
        /// Can this process reach the keychain and raise prompts at all?
        pub fn interactive(&self) -> bool {
            self.flags & INTERACTIVE == INTERACTIVE
        }

        /// The thing that actually breaks: TCC points at a process that has
        /// gone, so a prompt can never be shown and the check just fails.
        pub fn orphaned(&self) -> bool {
            self.responsible.is_some() && self.responsible_path.is_none()
        }

        pub fn flag_names(&self) -> String {
            let names: Vec<&str> = FLAGS
                .iter()
                .filter(|(bit, _)| self.flags & bit != 0)
                .map(|(_, name)| *name)
                .collect();
            if names.is_empty() {
                "none".into()
            } else {
                names.join(",")
            }
        }
    }

    /// `responsibility_get_pid_responsible_for_pid` is private, so it is
    /// looked up rather than linked: a macOS that drops it should cost us a
    /// line of the report, not the ability to start.
    fn responsible_for(pid: i32) -> Option<i32> {
        type Fun = unsafe extern "C" fn(i32) -> i32;
        let name = CString::new("responsibility_get_pid_responsible_for_pid").ok()?;
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        if sym.is_null() {
            return None;
        }
        let fun: Fun = unsafe { std::mem::transmute(sym) };
        match unsafe { fun(pid) } {
            -1 => None,
            other => Some(other),
        }
    }

    /// The executable behind a pid, and `None` once it has exited -- which is
    /// the whole question being asked of a responsible process.
    fn path_of(pid: i32) -> Option<PathBuf> {
        let mut buf = [0u8; 4096];
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        let s = CStr::from_bytes_until_nul(&buf[..=n as usize]).ok()?;
        Some(PathBuf::from(s.to_string_lossy().into_owned()))
    }

    pub fn identity() -> Option<Identity> {
        let pid = std::process::id() as i32;
        let mut info = AuditInfoAddr::default();
        let ok =
            unsafe { getaudit_addr(&mut info, std::mem::size_of::<AuditInfoAddr>() as i32) } == 0;
        if !ok {
            return None;
        }
        let responsible = responsible_for(pid);
        Some(Identity {
            asid: info.asid,
            flags: info.flags,
            responsible,
            responsible_path: responsible.and_then(path_of),
        })
    }
}

/// One line about this process's macOS identity, or nothing off macOS.
#[cfg(target_os = "macos")]
pub fn line() -> Option<String> {
    let id = imp::identity()?;
    let who = match (id.responsible, id.responsible_path.as_ref()) {
        (Some(pid), Some(path)) => format!(
            "{} (pid {pid})",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        (Some(pid), None) => format!("pid {pid}, gone"),
        (None, _) => "unknown".into(),
    };
    Some(format!(
        "audit session {} [{}], responsible process {who}",
        id.asid,
        id.flag_names()
    ))
}

/// Why a pane here would fail to reach the keychain or a TCC-guarded folder,
/// or `None` when nothing is wrong. Written for the status line and for
/// `ttmux doctor`, so it is one sentence.
#[cfg(target_os = "macos")]
pub fn complaint() -> Option<String> {
    let id = imp::identity()?;
    if id.orphaned() {
        return Some(
            "the terminal this session was started from has quit, so macOS \
             has nothing to show permission prompts on: new panes cannot be \
             granted Documents, Desktop or Full Disk Access. `ttmux upgrade` \
             moves the session to this terminal."
                .into(),
        );
    }
    if !id.interactive() {
        return Some(
            "this session's audit session is not a console login, so the \
             keychain will refuse to unlock (errSecInteractionNotAllowed). \
             Started over ssh or from launchd? `ttmux upgrade` from a local \
             terminal moves it."
                .into(),
        );
    }
    None
}

#[cfg(not(target_os = "macos"))]
pub fn line() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn complaint() -> Option<String> {
    None
}

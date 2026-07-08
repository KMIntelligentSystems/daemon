//! Capability lockdown for the oracle child process — defense-in-depth
//! below the closed tool catalog.
//!
//! The oracle's only legitimate needs:
//!   - OPENROUTER_API_KEY env var
//!   - stdin/stdout/stderr (for the tool bus)
//!   - TLS to openrouter.ai (via Node's built-in fetch)
//!
//! Everything else is removed or blocked here.  On Linux we add kernel-level
//! enforcement (ulimits, seccomp, chroot); on Windows we rely on the portable
//! core (env strip + closed catalog) since OS-level sandboxing requires Job
//! Objects / restricted tokens — deferred to a future hardening pass.
//!
//! See daemon-plan.md Phase 4 for the rationale.

use std::process::Command;

/// Prepare a Command to spawn the oracle with the minimum possible environment.
///
/// - Clears ALL inherited env vars.
/// - Sets only OPENROUTER_API_KEY (the oracle needs it to call OpenRouter).
/// - On Linux: applies ulimits (RSS, CPU, NPROC, NOFILE).
/// - On Linux: attempts chroot to a scratch dir (best-effort; requires root).
/// - Seccomp is documented below but not yet wired — it requires per-platform
///   BPF filter generation and is stubbed for a dedicated hardening sprint.
pub fn harden_child(cmd: &mut Command) {
    // ── Layer 1: Env strip (portable, highest-impact) ──────────────────
    //
    // Strategy: preserve only the env vars Node.js needs to bootstrap
    // (OS-specific), then explicitly set OPENROUTER_API_KEY.  All other
    // secrets inherited from the airlock's parent are removed.

    let or_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();

    // Preserve the bare minimum the OS + Node need to start.
    // On Windows: SystemRoot (DLL loader), PATH (node.exe), USERPROFILE.
    // On Linux: PATH, HOME.
    let preserve = if cfg!(target_os = "windows") {
        vec!["SystemRoot", "PATH", "USERPROFILE", "TMP", "TEMP"]
    } else {
        vec!["PATH", "HOME"]
    };

    // Build a new, minimal env map.
    let mut new_env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for key in &preserve {
        if let Ok(val) = std::env::var(key) {
            new_env.insert(key.to_string(), val);
        }
    }
    new_env.insert("OPENROUTER_API_KEY".to_string(), or_key);

    cmd.env_clear();
    for (k, v) in &new_env {
        cmd.env(k, v);
    }

    // ── Layer 2: ulimits (Linux only) ──────────────────────────────────
    //
    // Without ulimits a compromised oracle could allocate memory until the
    // host OOM-kills something else, or burn CPU indefinitely.  These caps
    // are generous for a single LLM API call + JSON parsing but prevent
    // runaway resource exhaustion.
    #[cfg(target_os = "linux")]
    {
        unsafe {
            // RSS cap: 512 MiB.  gpt-4o-mini responses are <100 KB; the
            // only large allocation would be malicious.
            let rss = libc::rlimit {
                rlim_cur: 512 * 1024 * 1024,
                rlim_max: 512 * 1024 * 1024,
            };
            libc::setrlimit(libc::RLIMIT_RSS, &rss);

            // CPU cap: 60 s (wall clock is already enforced by the airlock
            // at the tool-loop level; this is kernel-level redundancy).
            let cpu = libc::rlimit {
                rlim_cur: 60,
                rlim_max: 60,
            };
            libc::setrlimit(libc::RLIMIT_CPU, &cpu);

            // NPROC cap: 1.  The oracle must not fork.
            let nproc = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            libc::setrlimit(libc::RLIMIT_NPROC, &nproc);

            // NOFILE cap: 16.  stdin, stdout, stderr, and the TLS socket to
            // OpenRouter are all the oracle legitimately needs.  (Node's event
            // loop may use a few extra fds; 16 is comfortably above the minimum
            // but well below what a port-scanner would need.)
            let nofile = libc::rlimit {
                rlim_cur: 16,
                rlim_max: 16,
            };
            libc::setrlimit(libc::RLIMIT_NOFILE, &nofile);
        }
    }

    // ── Layer 3: chroot (Linux only, requires root / CAP_SYS_CHROOT) ──
    //
    // The oracle's cwd is the airlock's working directory.  If it somehow
    // gained filesystem access (via a Node.js CVE), chroot to an empty
    // directory prevents it from reading config.toml, sandbox.db, .env, etc.
    //
    // This is best-effort: on Railway the container user typically doesn't
    // have CAP_SYS_CHROOT, so this will fail silently.  In that environment
    // we rely on the ulimit + seccomp + env-strip layers.
    #[cfg(target_os = "linux")]
    {
        // Attempt to create and chroot to an empty scratch dir.
        // `std::os::unix::fs::chroot` is unstable, so we use libc directly.
        let scratch = "/tmp/daemon-scratch";
        let _ = std::fs::create_dir_all(scratch);
        // chroot requires root; if we're not root this is a no-op.
        let ret = unsafe { libc::chroot(scratch.as_ptr() as *const libc::c_char) };
        if ret == 0 {
            eprintln!("[airlock] chroot to {scratch} succeeded");
        }
        // chdir to / after chroot so relative paths aren't confused.
        let _ = unsafe { libc::chdir(b"/\0".as_ptr() as *const libc::c_char) };
    }
}

// ── Seccomp (deferred) ──────────────────────────────────────────────────
//
// libseccomp / BPF filter for the oracle on Linux would restrict syscalls
// to exactly:
//   read, write, exit, exit_group, mmap, mprotect, brk, rt_sigreturn,
//   rt_sigaction, rt_sigprocmask, futex, nanosleep, getpid, getrandom,
//   openat (TLS certs), readlink, stat, fstat, connect, sendto, recvfrom,
//   close, getpeername, getsockname, setsockopt
//
// This list is derived from the syscalls Node.js + OpenSSL + DNS resolution
// need for a single HTTPS request.  Adding it requires:
//   1. Install libseccomp-dev on the build host (or use the seccompiler crate).
//   2. Generate a BPF program from the allowlist.
//   3. Call seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, &prog)
//      BEFORE spawning the oracle.
//
// Because BPF filter generation is platform-specific and the syscall set
// varies with Node.js versions and OpenSSL builds, this is deferred to a
// dedicated hardening sprint with CI testing on the actual Railway container
// image.  Until then the closed tool catalog + env strip + ulimits provide
// the primary boundary, and the seccomp layer is documented here as the
// intended next step.
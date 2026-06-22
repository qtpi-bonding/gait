//! `gait daemons` subcommand: list and stop tracked background daemons.

pub fn cmd_daemons(gait_dir: &std::path::Path, args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("stop") => {
            let name = match args.get(1) {
                Some(n) => n,
                None => {
                    eprintln!("[gait daemons] usage: gait daemons stop <name>");
                    return;
                }
            };
            cmd_daemons_stop(gait_dir, name);
        }
        Some("list") | None => cmd_daemons_list(gait_dir),
        Some(other) => eprintln!("[gait daemons] unknown subcommand: {}", other),
    }
}

fn cmd_daemons_list(gait_dir: &std::path::Path) {
    use gait_core::session::daemons::{active, is_alive};
    match active(gait_dir) {
        Ok(recs) => {
            if recs.is_empty() {
                println!("No daemons.");
                return;
            }
            for r in &recs {
                let status = if is_alive(r) { "running" } else { "stopped" };
                println!(
                    "{} (pid {}) — {} — log: {}",
                    r.name, r.pid, status, r.log_path
                );
            }
        }
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

fn cmd_daemons_stop(gait_dir: &std::path::Path, name: &str) {
    use gait_core::session::daemons::{active, append_stop, is_alive};
    let recs = match active(gait_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait daemons] error: {}", e);
            return;
        }
    };
    match recs.iter().find(|r| r.name == name) {
        None => eprintln!("[gait daemons] no daemon named '{}'", name),
        Some(rec) => {
            if is_alive(rec) {
                // SAFETY: is_alive performed the identity check (proc_start_time comparison).
                // killpg sends SIGTERM to the daemon's process group.
                unsafe { libc::killpg(rec.pgid, libc::SIGTERM) };
                println!("[gait daemons] stopped {} (pid {})", name, rec.pid);
            } else {
                println!("[gait daemons] {} is not running", name);
            }
            let _ = append_stop(gait_dir, name);
        }
    }
}

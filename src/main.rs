//! xses — unified picker, cross-importer and converter for AI coding
//! agent sessions (claude, codex, pi, oh-my-pi, opencode).
mod ansi;
mod canonical;
mod cli;
mod cmd;
mod continuations;
mod extract;
mod index;
mod issues;
mod jsonl;
#[cfg(test)]
mod matrix;
mod model;
mod picker;
mod proc;
mod progress;
mod publication;
mod refs;
mod scan;
mod scope;
mod storage;
mod sync;
mod text;
mod write;

fn main() -> anyhow::Result<()> {
    // Rust ignores SIGPIPE, which turns `xses ls | head` into a panic on the
    // first write that fails; standard-tool behaviour is to exit quietly.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    cmd::run()
}

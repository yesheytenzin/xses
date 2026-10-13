//! xses — unified picker, cross-importer and converter for AI coding
//! agent sessions (claude, codex, pi, oh-my-pi, opencode).
mod canonical;
mod cli;
mod cmd;
mod extract;
mod index;
mod issues;
mod jsonl;
mod model;
mod picker;
mod proc;
mod refs;
mod scan;
mod scope;
mod sync;
mod write;

fn main() -> anyhow::Result<()> {
    cmd::run()
}

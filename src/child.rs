//! Owned subprocess with optional captured stderr.

use std::path::PathBuf;
use std::process::{Child, ExitStatus};

pub struct ChildProcess {
    name: &'static str,
    child: Child,
    stderr_path: Option<PathBuf>,
    exit: Option<ExitStatus>,
}

impl ChildProcess {
    pub fn new(name: &'static str, child: Child, stderr_path: Option<PathBuf>) -> Self {
        Self {
            name,
            child,
            stderr_path,
            exit: None,
        }
    }

    pub fn is_running(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exit = Some(status);
                false
            }
            Err(_) => false,
        }
    }

    pub fn exit_detail(&self) -> String {
        let status = match self.exit {
            Some(s) => s.to_string(),
            None => "still running".to_string(),
        };
        match &self.stderr_path {
            Some(path) => {
                let tail = std::fs::read_to_string(path).unwrap_or_default();
                format!("{}: {status}\n{}", self.name, tail_lines(&tail, 40))
            }
            None => format!("{}: {status}", self.name),
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let mut lines: Vec<&str> = s.lines().rev().take(n).collect();
    lines.reverse();
    lines.join("\n")
}

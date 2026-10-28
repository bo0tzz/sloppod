//! What was still running when the pod was told to stop (ADR 10, 17): the agent's tool commands, foreground or in
//! the background, which ending the pod cuts off. The runtime tells the agent, so it can start them again.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sloppod_protocol::link::RunningProcess;

/// How long after the agent its own helpers (the agent's CLI, MCP servers) may start. A command only starts in a turn,
/// later than that, so a process under the agent that started later is taken for one, whatever it runs: its shell may
/// have `exec`ed the program, leaving no shell to recognize. In clock ticks, at Linux's 100 a second.
const HELPERS_START_WITHIN: u64 = 10 * 100;

/// How much of a command's output to report.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: u64 = 4096;

/// A process as `/proc` has it: its parent, and when it started, in clock ticks since boot.
struct Proc {
    ppid: u32,
    start: u64,
}

/// The commands running under `agent`: each process below it that started after the agent's helpers did, with no
/// other such process between it and the agent, so a command and everything it starts count once. A helper that
/// starts late counts as a command too, which errs on the side of the session being busy.
fn commands(procs: &HashMap<u32, Proc>, agent: u32) -> Vec<u32> {
    let Some(agent_start) = procs.get(&agent).map(|p| p.start) else {
        return Vec::new();
    };
    let is_command = |pid: u32| {
        procs
            .get(&pid)
            .is_some_and(|p| p.start > agent_start + HELPERS_START_WITHIN)
    };
    // Whether `pid` is under the agent with no other command above it on the way.
    let outermost = |pid: u32| {
        let mut parent = procs.get(&pid).map(|p| p.ppid);
        while let Some(ppid) = parent {
            if ppid == agent {
                return true;
            }
            if is_command(ppid) {
                return false;
            }
            parent = procs.get(&ppid).map(|p| p.ppid);
        }
        false
    };
    let mut commands: Vec<u32> = procs
        .keys()
        .copied()
        .filter(|pid| is_command(*pid) && outermost(*pid))
        .collect();
    commands.sort_unstable();
    commands
}

fn read_procs(root: &Path) -> HashMap<u32, Proc> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // `pid (comm) state ppid ... starttime ...`; comm can contain spaces and parentheses, so read after the
            // last `)`, where the state is field 3 of proc(5)'s numbering and the start time field 22.
            let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            let ppid = fields.get(1)?.parse().ok()?;
            let start = fields.get(19)?.parse().ok()?;
            Some((pid, Proc { ppid, start }))
        })
        .collect()
}

/// The last lines a process wrote to its standard output, if that's a file (as for background commands).
fn output_tail(root: &Path, pid: u32) -> Option<String> {
    let target = std::fs::read_link(root.join(pid.to_string()).join("fd/1")).ok()?;
    let mut file = std::fs::File::open(&target).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    // Only the end is read: a long-running command's output can be large, and this runs in the grace period.
    file.seek(SeekFrom::Start(metadata.len().saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut content = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut content).ok()?;
    let text = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    (!tail.trim().is_empty()).then_some(tail)
}

/// How many commands are running under the agent.
pub fn count(agent: u32) -> usize {
    commands(&read_procs(Path::new("/proc")), agent).len()
}

/// The commands still running under the agent, read from `/proc`.
pub fn running(agent: u32) -> Vec<RunningProcess> {
    running_in(Path::new("/proc"), agent)
}

fn running_in(root: &Path, agent: u32) -> Vec<RunningProcess> {
    let procs = read_procs(root);
    commands(&procs, agent)
        .into_iter()
        .filter_map(|pid| {
            let cmdline = std::fs::read(root.join(pid.to_string()).join("cmdline")).ok()?;
            let command = cmdline
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            Some(RunningProcess {
                command,
                output_tail: output_tail(root, pid),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// A stat line for a process started `start` ticks after boot.
    fn stat(pid: u32, comm: &str, ppid: u32, start: u64) -> String {
        format!(
            "{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0"
        )
    }

    fn procs(table: &[(u32, u32, u64)]) -> HashMap<u32, Proc> {
        table
            .iter()
            .map(|(pid, ppid, start)| {
                (
                    *pid,
                    Proc {
                        ppid: *ppid,
                        start: *start,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn each_command_under_the_agent_counts_once() {
        // supervisor 1 > agent 10 (started at 100) > claude 11 and an MCP server 40, helpers started with it
        //   claude 11 > bash 20 (a background build) > cargo 21 > bash 22 (a build script)
        //             > sleep 30, a command whose shell exec'd it, so no shell is left
        // bash 50 elsewhere in the pod isn't the agent's.
        let table = procs(&[
            (1, 0, 10),
            (10, 1, 100),
            (11, 10, 150),
            (40, 10, 300),
            (20, 11, 5_000),
            (21, 20, 5_001),
            (22, 21, 5_002),
            (30, 11, 9_000),
            (50, 1, 9_000),
        ]);
        assert_eq!(commands(&table, 10), [20, 30]);
        assert!(commands(&procs(&[(10, 1, 100), (11, 10, 150)]), 10).is_empty());
        assert!(commands(&table, 999).is_empty(), "no agent, no commands");
    }

    #[test]
    fn reads_commands_and_their_output_from_proc() {
        let root = std::env::temp_dir().join(format!("shutdown-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proc = |pid: u32, stat: &str, cmdline: &[u8]| {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(dir.join("fd")).unwrap();
            std::fs::write(dir.join("stat"), stat).unwrap();
            std::fs::write(dir.join("cmdline"), cmdline).unwrap();
        };
        proc(10, &stat(10, "node", 1, 100), b"node\0agent\0");
        proc(
            20,
            &stat(20, "bash (a) b)", 10, 5_000),
            b"/bin/bash\0-c\0cargo test 2>&1\0",
        );
        let output = root.join("task.output");
        let lines: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
        std::fs::write(&output, lines.join("\n")).unwrap();
        std::os::unix::fs::symlink(&output, root.join("20/fd/1")).unwrap();

        let running = running_in(&root, 10);
        assert_eq!(running.len(), 1);
        // A huge output file is only read at its end.
        let huge = root.join("huge.output");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(64 << 30).unwrap();
        drop(file);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&huge)
            .unwrap()
            .write_all(b"\nthe end\n")
            .unwrap();
        std::fs::remove_file(root.join("20/fd/1")).unwrap();
        std::os::unix::fs::symlink(&huge, root.join("20/fd/1")).unwrap();
        assert!(
            output_tail(&root, 20).unwrap().ends_with("the end"),
            "read from the end of a 64 GiB sparse file"
        );
        std::fs::remove_file(root.join("20/fd/1")).unwrap();
        std::os::unix::fs::symlink(&output, root.join("20/fd/1")).unwrap();
        let running = running_in(&root, 10);
        assert_eq!(running[0].command, "/bin/bash -c cargo test 2>&1");
        let tail = running[0].output_tail.as_deref().unwrap();
        assert!(tail.starts_with("line 11\n") && tail.ends_with("line 30"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

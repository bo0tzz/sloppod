//! What was still running when the pod was told to stop (ADR 10, 17): the agent's tool commands, foreground or in
//! the background, which ending the pod cuts off. The runtime tells the agent, so it can start them again.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sloppod_protocol::link::RunningProcess;

/// Shells the agent runs its commands in.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash"];

/// How much of a command's output to report.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: u64 = 4096;

/// A process as `/proc` has it: its parent and its name.
struct Proc {
    ppid: u32,
    comm: String,
}

/// The commands running under `agent`: each shell below it with no other shell between it and the agent, so a
/// command and everything it starts count once.
fn commands(procs: &HashMap<u32, Proc>, agent: u32) -> Vec<u32> {
    let is_shell = |pid: u32| {
        procs
            .get(&pid)
            .is_some_and(|p| SHELLS.contains(&p.comm.as_str()))
    };
    // Whether `pid` is under the agent with no shell above it on the way.
    let outermost = |pid: u32| {
        let mut parent = procs.get(&pid).map(|p| p.ppid);
        while let Some(ppid) = parent {
            if ppid == agent {
                return true;
            }
            if is_shell(ppid) {
                return false;
            }
            parent = procs.get(&ppid).map(|p| p.ppid);
        }
        false
    };
    let mut commands: Vec<u32> = procs
        .keys()
        .copied()
        .filter(|pid| is_shell(*pid) && outermost(*pid))
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
            // `pid (comm) state ppid ...`; comm can contain spaces and parentheses, so read around the last `)`.
            let (head, rest) = stat.rsplit_once(')')?;
            let comm = head.split_once('(')?.1.to_owned();
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, Proc { ppid, comm }))
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

    fn procs(table: &[(u32, u32, &str)]) -> HashMap<u32, Proc> {
        table
            .iter()
            .map(|(pid, ppid, comm)| {
                (
                    *pid,
                    Proc {
                        ppid: *ppid,
                        comm: (*comm).to_owned(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn each_command_under_the_agent_counts_once() {
        // supervisor 1 > agent 10 > claude 11 > bash 20 (a background build) > cargo 21 > bash 22 (a build script)
        //                                     > bash 30 (a dev server) > node 31
        //                         > mcp server 40, not a command.
        // bash 50 elsewhere in the pod isn't the agent's.
        let table = procs(&[
            (1, 0, "sloppod-supervi"),
            (10, 1, "node"),
            (11, 10, "claude"),
            (20, 11, "bash"),
            (21, 20, "cargo"),
            (22, 21, "bash"),
            (30, 11, "bash"),
            (31, 30, "node"),
            (40, 10, "node"),
            (50, 1, "bash"),
        ]);
        assert_eq!(commands(&table, 10), [20, 30]);
        assert!(commands(&procs(&[(10, 1, "node"), (11, 10, "claude")]), 10).is_empty());
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
        proc(10, "10 (node) S 1 10", b"node\0agent\0");
        proc(20, "20 (bash) S 10 20", b"/bin/bash\0-c\0cargo test 2>&1\0");
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

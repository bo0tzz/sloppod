//! `sloppod-supervisor guard`: Claude Code's `PreToolUse` hook for shell commands (#339). It refuses a few commands
//! that go wrong in ways an agent doesn't notice, and says what to do instead. It prevents mistakes; it isn't a
//! boundary (ADR 11): anything it can't read it lets through.

use std::io::Read;

use serde_json::Value;
use sloppod_contracts::github::{Site, clone_instead, repo_path, single_file_read};

/// Reads the hook's input from stdin; exits 2 with the reason on stderr to refuse, which Claude Code shows the agent.
pub fn run() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let Ok(call) = serde_json::from_str::<Value>(&input) else {
        return 0;
    };
    match refusal(&call) {
        Some(reason) => {
            eprintln!("sloppod refused this command: {reason}");
            2
        }
        None => 0,
    }
}

/// Why a tool call is refused, if it is. `Bash` and `Monitor` run shell commands; a `Monitor` and a background `Bash`
/// are watched until they exit.
fn refusal(call: &Value) -> Option<String> {
    let input = &call["tool_input"];
    let command = input["command"].as_str()?;
    let watched = match call["tool_name"].as_str()? {
        "Bash" => input["run_in_background"].as_bool() == Some(true),
        "Monitor" => true,
        _ => return None,
    };
    let commands = split(command);
    commands
        .iter()
        .find_map(|c| {
            blanket_staging(&c.words)
                .or_else(|| pkill(&c.words))
                .or_else(|| file_fetch(&c.words))
        })
        .or_else(|| watched.then(|| pgrep_wait(&commands)).flatten())
        .or_else(|| watched.then(|| detached(&commands)).flatten())
        .or_else(|| loop_wait(&commands))
}

/// A simple command: its words, unquoted, and whether a lone `&` sent it to the background.
#[derive(Debug, Default, PartialEq)]
struct Simple {
    words: Vec<String>,
    background: bool,
}

/// Splits a shell command into simple commands at `;`, `&`, `&&`, `|`, `||`, newlines and parentheses, honouring
/// quotes and backslashes. Rough, but enough to read the command words and their flags.
fn split(command: &str) -> Vec<Simple> {
    let mut commands = Vec::new();
    let mut current = Simple::default();
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    // Here-documents whose bodies start after the next newline: their delimiters, and whether tabs are stripped.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let end_word = |word: &mut Option<String>, current: &mut Simple| {
        if let Some(word) = word.take() {
            current.words.push(word);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_default();
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => w.extend(chars.next()),
                        c => w.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(c) = chars.next().filter(|c| *c != '\n') {
                    word.get_or_insert_default().push(c);
                }
            }
            '<' if chars.peek() == Some(&'<') => {
                end_word(&mut word, &mut current);
                chars.next();
                // `<<<` is a here-string: its word is an argument, not a body.
                if chars.next_if_eq(&'<').is_some() {
                    continue;
                }
                let tabs = chars.next_if_eq(&'-').is_some();
                while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                let mut delimiter = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_whitespace() && !";|&()<>".contains(*c))
                {
                    if !matches!(c, '\'' | '"' | '\\') {
                        delimiter.push(c);
                    }
                }
                heredocs.push((delimiter, tabs));
            }
            c if c.is_whitespace() && c != '\n' => end_word(&mut word, &mut current),
            ';' | '\n' | '|' | '&' | '(' | ')' => {
                end_word(&mut word, &mut current);
                if c == '&' {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    } else if chars.peek() == Some(&'>') {
                        // `&>` redirects; it's part of this command.
                        chars.next();
                        continue;
                    } else if !current.words.last().is_some_and(|w| w.ends_with('>')) {
                        current.background = true;
                    } else {
                        // `2>&1`: the `&` belongs to the redirection.
                        word = current.words.pop().map(|w| w + "&");
                        continue;
                    }
                } else if c == '|' && chars.peek() == Some(&'|') {
                    chars.next();
                }
                if !current.words.is_empty() {
                    commands.push(std::mem::take(&mut current));
                }
                current.background = false;
                // A here-document's body is data, not commands.
                if c == '\n' {
                    for (delimiter, tabs) in heredocs.drain(..) {
                        loop {
                            let line: String = chars.by_ref().take_while(|c| *c != '\n').collect();
                            let line = if tabs {
                                line.trim_start_matches('\t')
                            } else {
                                &line
                            };
                            if line == delimiter || chars.peek().is_none() {
                                break;
                            }
                        }
                    }
                }
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    end_word(&mut word, &mut current);
    if !current.words.is_empty() {
        commands.push(current);
    }
    commands
}

/// The command a simple command runs, past keywords and wrappers that run what follows them.
fn program(words: &[String]) -> &[String] {
    let skip = words
        .iter()
        .take_while(|w| {
            matches!(
                w.as_str(),
                "!" | "until"
                    | "while"
                    | "if"
                    | "then"
                    | "do"
                    | "else"
                    | "elif"
                    | "time"
                    | "{"
                    | "sudo"
                    | "exec"
            ) || (w.contains('=') && !w.starts_with('-'))
        })
        .count();
    &words[skip..]
}

/// Whether `word` is a short-option cluster (`-am`) with `flag` in it, or the long option.
fn has_flag(word: &str, flag: char, long: &str) -> bool {
    word == long || (word.starts_with('-') && !word.starts_with("--") && word[1..].contains(flag))
}

/// A git command's subcommand and its arguments, past git's own options (`-C <path>`, `-c <name=value>`).
fn git(words: &[String]) -> Option<(&str, &[String])> {
    let words = program(words);
    if words.first()? != "git" {
        return None;
    }
    let mut rest = &words[1..];
    while let Some(first) = rest.first() {
        match first.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => rest = rest.get(2..)?,
            w if w.starts_with('-') => rest = &rest[1..],
            _ => break,
        }
    }
    Some((rest.first()?.as_str(), &rest[1..]))
}

const STAGE_BY_NAME: &str = "it sweeps unrelated and untracked files (build output, test reports, scratch files) into \
    the commit. Stage the paths you changed by name, e.g. `git add src/a.rs docs/b.md`, and check `git status` first.";

fn blanket_staging(words: &[String]) -> Option<String> {
    let (subcommand, args) = git(words)?;
    match subcommand {
        "add" => {
            let blanket = args.iter().take_while(|a| *a != "--").any(|a| {
                has_flag(a, 'A', "--all")
                    || has_flag(a, 'u', "--update")
                    || a == "--no-ignore-removal"
            }) || args
                .iter()
                .any(|a| matches!(a.as_str(), "." | "./" | ":/" | ":/." | "*" | ":(top)"));
            blanket.then(|| format!("`git add` of everything: {STAGE_BY_NAME}"))
        }
        "commit" => {
            let refused = || Some(format!("`git commit -a`: {STAGE_BY_NAME}"));
            let mut args = args.iter();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--" => break,
                    "--all" => return refused(),
                    // A long option's value is the next word, unless it's attached with `=`.
                    "--message" | "--file" | "--author" | "--date" | "--template"
                    | "--reuse-message" | "--reedit-message" | "--fixup" | "--squash"
                    | "--cleanup" => {
                        args.next();
                    }
                    _ => {
                        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.starts_with('-'))
                        else {
                            continue;
                        };
                        // Read in order: an option that takes a value takes the rest of the word (`-m"msg"`), or
                        // the next word when nothing's left.
                        for (i, flag) in cluster.char_indices() {
                            if flag == 'a' {
                                return refused();
                            }
                            if "mFCct".contains(flag) {
                                if i + 1 == cluster.len() {
                                    args.next();
                                }
                                break;
                            }
                            if flag == 'S' {
                                break;
                            }
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn pkill(words: &[String]) -> Option<String> {
    (program(words).first()? == "pkill").then(|| {
        "`pkill`: its pattern also matches the shell running this command, so it can kill that shell and silently \
         skip the rest. Find the process with `pgrep -af <pattern>`, check it's the one, then `kill <pid>`."
            .to_owned()
    })
}

/// Reading one file from GitHub with a command, which the agent clones the repository for instead. The same rule the
/// runtime's request policy applies to the requests themselves (`sloppod_contracts::github`), and the same words, so
/// the agent hears it before a request is made and however it tried.
///
/// `raw.githubusercontent.com` stays reachable at the network level — package and tool installers fetch from it, so
/// it's one of the hosts a session reaches directly (`SLOPPOD_DIRECT_HOSTS`, ADR 11) — which is why an agent's own
/// commands are refused here rather than at egress.
fn file_fetch(words: &[String]) -> Option<String> {
    let program = program(words);
    match program.first()?.as_str() {
        "gh" => gh_file_fetch(&program[1..]),
        "curl" | "wget" => download_file_fetch(&program[1..]),
        _ => None,
    }
}

/// A `gh` command that reads one file: `gh api` of a single-file endpoint, and `gh browse` of a path.
fn gh_file_fetch(args: &[String]) -> Option<String> {
    match args.first()?.as_str() {
        "api" => {
            let endpoint = api_path(&args[1..])?;
            single_file_read(Site::Api, &endpoint).map(|repo| clone_instead(Some(&repo)))
        }
        // The repository is the working copy's, which a command line doesn't say: the hint is the generic one.
        "browse" => browse_path(&args[1..])
            .is_some_and(|path| path.contains('/') || path.contains('.'))
            .then(|| clone_instead(None)),
        _ => None,
    }
}

/// The long flags whose value is the next word, for the tools read here.
const TAKES_VALUE: [&str; 17] = [
    "-H",
    "--header",
    "-X",
    "--method",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--hostname",
    "--cache",
    "-b",
    "--branch",
];

/// A `gh api` command's endpoint, as a path: `repos/o/r/contents/x`, `/repos/…` and `https://api.github.com/repos/…`
/// all read the same. `None` when the command has no endpoint, or one the flags hid.
fn api_path(args: &[String]) -> Option<String> {
    let endpoint = first_positional(args)?;
    let path = endpoint
        .strip_prefix("https://api.github.com")
        .or_else(|| endpoint.strip_prefix("http://api.github.com"))
        .unwrap_or(&endpoint);
    Some(if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    })
}

/// The path a `gh browse` command names, if it names one.
fn browse_path(args: &[String]) -> Option<String> {
    first_positional(args)
}

/// The first word that isn't a flag or a flag's value: the positional argument a command takes.
fn first_positional(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(word) = args.next() {
        if word == "--" {
            return args.next().cloned();
        }
        if word.starts_with('-') {
            // A flag's value is the next word, unless it's attached with `=`.
            if TAKES_VALUE.contains(&word.as_str()) {
                args.next();
            }
            continue;
        }
        return Some(word.clone());
    }
    None
}

/// A `curl` or `wget` of one file from GitHub: `raw.githubusercontent.com`, or github.com's `…/raw/…` and
/// `…/blob/…` views.
fn download_file_fetch(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|word| github_file_url(word))
        .map(|repo| clone_instead(repo.as_deref()))
}

/// The repository a word is a download of one file from, if it is: `raw.githubusercontent.com/<owner>/<name>/…` (the
/// raw host serves one file, whatever the path), or github.com's view of one. `Some(None)` when it's a download of
/// one file but the URL doesn't name a repository, which the hint says without one.
fn github_file_url(word: &str) -> Option<Option<String>> {
    let (host, path) = url_parts(word)?;
    match host {
        "raw.githubusercontent.com" => {
            Some(repo_path(&format!("/{path}"), "/").map(|(repo, _)| repo))
        }
        "github.com" | "www.github.com" => {
            single_file_read(Site::Web, &format!("/{path}")).map(Some)
        }
        _ => None,
    }
}

/// A URL's host and path, from a word that's one, with a scheme or without, and with any userinfo and port.
fn url_parts(word: &str) -> Option<(&str, &str)> {
    let rest = word.split_once("://").map(|(_, rest)| rest).unwrap_or(word);
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty() && !path.is_empty()).then_some((host, path))
}

/// Whether a simple command is `pgrep` with `-f`, which matches the watcher's own shell.
fn pgrep_f(words: &[String]) -> bool {
    let words = program(words);
    words.first().is_some_and(|w| w == "pgrep")
        && words[1..].iter().any(|w| has_flag(w, 'f', "--full"))
}

const WAIT_INSTEAD: &str = "Let the job's own exit be the signal (run it in this task's foreground, or wait for the \
    background task that runs it to finish), or wait on its pid with `kill -0 <pid>`.";

/// `pgrep -f` in a watched command: it always matches the watcher's own shell, so waiting on it never ends.
fn pgrep_wait(commands: &[Simple]) -> Option<String> {
    commands.iter().any(|c| pgrep_f(&c.words)).then(|| {
        format!(
            "`pgrep -f` as a wait condition: it matches the shell running this watch, so it never stops matching. \
             {WAIT_INSTEAD}"
        )
    })
}

/// `until`/`while` on `pgrep -f`, anywhere.
fn loop_wait(commands: &[Simple]) -> Option<String> {
    commands
        .iter()
        .any(|c| {
            matches!(c.words.first().map(String::as_str), Some("until" | "while"))
                && pgrep_f(&c.words)
        })
        .then(|| pgrep_wait(commands))
        .flatten()
}

/// A watched command that detaches what it runs: the task ends at once, and nothing watches the real work.
fn detached(commands: &[Simple]) -> Option<String> {
    let waits = commands
        .iter()
        .any(|c| program(&c.words).first().is_some_and(|w| w == "wait"));
    let detaches = commands.iter().any(|c| {
        let program = program(&c.words);
        matches!(
            program.first().map(String::as_str),
            Some("nohup" | "setsid" | "disown")
        ) || (c.background && !waits)
    });
    detaches.then(|| {
        "detaching inside a background task (`nohup`, `setsid`, `disown` or `&` without `wait`): the task ends within \
         seconds, so its finishing no longer means the work did, and nothing watches the real process. Run the \
         command in the task's foreground; the task itself is what runs in the background."
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn bash(command: &str) -> Option<String> {
        refusal(&json!({"tool_name": "Bash", "tool_input": {"command": command}}))
    }

    fn background(command: &str) -> Option<String> {
        refusal(
            &json!({"tool_name": "Bash", "tool_input": {"command": command, "run_in_background": true}}),
        )
    }

    fn monitor(command: &str) -> Option<String> {
        refusal(&json!({"tool_name": "Monitor", "tool_input": {"command": command}}))
    }

    #[test]
    fn splits_like_a_shell_would() {
        let words = |c: &Simple| c.words.join(" ");
        let commands =
            split("cd /a && git add -A; echo 'x; y' | grep \"a b\" &\nsleep 1 2>&1 >log");
        assert_eq!(
            commands.iter().map(words).collect::<Vec<_>>(),
            [
                "cd /a",
                "git add -A",
                "echo x; y",
                "grep a b",
                "sleep 1 2>&1 >log"
            ]
        );
        assert_eq!(
            commands.iter().map(|c| c.background).collect::<Vec<_>>(),
            [false, false, false, true, false]
        );
    }

    #[test]
    fn refuses_blanket_staging() {
        for command in [
            "git add -A",
            "git add --all",
            "git add -u",
            "git add .",
            "git add :/",
            "cd /workspace/repo && git add -A && git commit -m wip",
            "git -C /workspace/repo add -A",
            "git commit -a -m 'wip'",
            "git commit -am wip",
            "git commit --all",
            "git commit -vam wip",
            "cat <<EOF > a\nhi\nEOF\ngit add -A",
        ] {
            assert!(
                bash(command).is_some_and(|r| r.contains("Stage the paths")),
                "{command}"
            );
        }
        for command in [
            "git add src/a.rs docs/b.md",
            "git add -p src/a.rs",
            "git commit -m 'stage with git add -A'",
            "git commit -m -a",
            "git commit -m\"update docs\"",
            "git commit -mupdate",
            "git commit -F./a.txt",
            "git commit -S -m wip",
            "cat <<'EOF' > notes.md\ngit add -A\npkill -f x\nEOF\ngit status",
            "cat <<-EOF\n\tgit add -A\n\tEOF",
            "grep -c x <<< \"git add -A\"",
            "git commit -F msg.txt",
            "echo git add -A",
            "git status",
        ] {
            assert_eq!(bash(command), None, "{command}");
        }
    }

    #[test]
    fn refuses_pkill() {
        assert!(bash("pkill -f 'mock/server'").is_some_and(|r| r.contains("pgrep -af")));
        assert!(bash("sleep 1; pkill node").is_some());
        assert_eq!(bash("pgrep -af node"), None);
        assert_eq!(bash("kill 1234"), None);
    }

    #[test]
    fn refuses_reading_one_file_from_github() {
        for command in [
            "gh api repos/o/r/contents/src/main.rs",
            "gh api /repos/o/r/contents/README.md",
            "gh api repos/o/r/contents",
            "gh api repos/o/r/readme",
            "gh api repos/o/r/readme/docs",
            "gh api repos/o/r/git/blobs/8e2a1c",
            "gh api --method GET -H 'Accept: application/vnd.github.raw' repos/o/r/git/blobs/8e2a1c",
            "gh api https://api.github.com/repos/o/r/contents/a/b",
            "gh api -q .name /repos/o/r/contents",
            "cd /workspace/repo && gh api repos/o/r/contents/x",
            "gh browse src/main.rs",
            "gh browse README.md",
            "gh browse -- wiki/Home.md",
            "curl -sL https://raw.githubusercontent.com/o/r/main/src/main.rs",
            "curl -s https://raw.githubusercontent.com/o/r/main/f -o out.txt",
            "wget https://raw.githubusercontent.com/o/r/main/README.md",
            "curl https://github.com/o/r/raw/main/src/main.rs",
            "curl -O https://github.com/o/r/blob/main/src/main.rs",
        ] {
            assert!(
                bash(command).is_some_and(|r| r.contains("clone the repository instead")),
                "{command}"
            );
        }
        // The hint names the clone to make, when the command says which repository it's about.
        assert!(
            bash("gh api repos/futo/kupra/contents/README.md")
                .is_some_and(|r| r.contains("https://github.com/futo/kupra /workspace/tmp/kupra"))
        );
        assert!(
            bash("curl -sL https://raw.githubusercontent.com/futo/kupra/main/x.rs")
                .is_some_and(|r| r.contains("https://github.com/futo/kupra /workspace/tmp/kupra"))
        );
    }

    #[test]
    fn leaves_the_rest_of_github_alone_at_the_command_line() {
        for command in [
            "git clone --depth 1 https://github.com/o/r /workspace/tmp/r",
            "git clone --branch fix https://github.com/o/r.git /workspace/tmp/r",
            "git fetch origin",
            "gh issue view 12",
            "gh pr view 12 --comments",
            "gh api repos/o/r/pulls/12",
            "gh api /repos/o/r/pulls",
            "gh api repos/o/r/issues -q '.[].title'",
            "gh api repos/o/r/contentsomething",
            "gh api repos/o/r/git/trees/main",
            "gh api repos/o/r/git/blobsomething",
            // A repository named `blob` or `raw` is read through the API like any other.
            "gh api repos/acme/blob/issues",
            "gh api repos/acme/blob/git/trees/main",
            "gh api repos/acme/raw/pulls",
            "gh api search/code -f q=clone",
            "gh api graphql -f query='{ viewer { login } }'",
            "gh api -X POST repos/o/r/issues -f title=x",
            "gh browse",
            "gh browse 12",
            "gh browse -b main",
            "curl -sL https://api.github.com/repos/o/r/git/trees/main",
            "curl -sL https://github.com/o/r/archive/main.tar.gz",
            "wget https://objects.githubusercontent.com/o/r/main/f",
            "curl -sL https://raw.githubusercontent.com.evil.test/o/r/main/f",
        ] {
            assert_eq!(bash(command), None, "{command}");
        }
    }

    #[test]
    fn refuses_pgrep_f_as_a_wait_condition() {
        for refused in [
            monitor("until ! pgrep -f 'cargo test'; do sleep 5; done"),
            background("while pgrep -f playwright >/dev/null; do sleep 5; done; echo done"),
            bash("until ! pgrep -f 'cargo test'; do sleep 5; done"),
            monitor("pgrep -fa server || echo gone"),
        ] {
            assert!(refused.is_some_and(|r| r.contains("kill -0")));
        }
        // Looking once, in the foreground, is fine.
        assert_eq!(bash("pgrep -af server"), None);
        assert_eq!(monitor("until grep -q done log; do sleep 5; done"), None);
    }

    #[test]
    fn refuses_detaching_inside_a_background_task() {
        for command in [
            "nohup cargo test > log 2>&1",
            "setsid npm run dev",
            "npm run dev & disown",
            "cargo test > log 2>&1 &",
        ] {
            assert!(
                background(command).is_some_and(|r| r.contains("foreground")),
                "{command}"
            );
        }
        // Waited for, or in the foreground of the task, or not a background task at all.
        assert_eq!(background("a & b & wait"), None);
        assert_eq!(
            background("cargo test > log 2>&1; echo exit=$? >> log"),
            None
        );
        assert_eq!(bash("cargo build &"), None);
    }

    #[test]
    fn lets_through_what_it_cant_read() {
        assert_eq!(
            refusal(&json!({"tool_name": "Read", "tool_input": {"file_path": "/a"}})),
            None
        );
        assert_eq!(refusal(&json!({"tool_name": "Bash"})), None);
        assert_eq!(refusal(&json!("not a call")), None);
    }
}

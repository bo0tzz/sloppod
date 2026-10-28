use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use graphql_parser::query::{Definition, OperationDefinition};
use sloppod_contracts::capability;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};
use sloppod_contracts::{repo_from_git_path, same_repo};

use crate::credentials::Credentials;
use crate::github::{GitHubApp, PushLog};
use crate::proposals::is_session_branch;
use crate::rebases::{Recheck, Rechecks};

/// GitHub: reads and session-branch pushes with the owner's GitHub App, through tokens scoped to the session's
/// grants (ADR 11); all other writes go through proposals.
pub struct GitHubPolicy {
    credentials: Arc<Credentials>,
    pushes: Arc<PushLog>,
    rechecks: Arc<Rechecks>,
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

impl GitHubPolicy {
    pub fn new(
        credentials: Arc<Credentials>,
        pushes: Arc<PushLog>,
        rechecks: Arc<Rechecks>,
    ) -> Self {
        Self {
            credentials,
            pushes,
            rechecks,
        }
    }

    /// The owner's GitHub App, if they configured one.
    async fn app(&self, session: &SessionContext) -> Option<Arc<GitHubApp>> {
        self.credentials.github(&session.owner).await.ok()
    }

    async fn git(
        &self,
        session: &SessionContext,
        request: &HttpRequest,
        git: GitRequest,
    ) -> Result<Decision> {
        match git.service {
            Service::UploadPack => {
                // Granted repositories are read with a token covering only the grants. Anything else goes out
                // without one, so it's readable only if it's public.
                let granted: Vec<&str> = session.repos().collect();
                let token = match self.app(session).await {
                    Some(app) if granted.iter().any(|r| same_repo(r, &git.repo)) => {
                        app.read_token(&git.repo, &granted).await.ok()
                    }
                    _ => None,
                };
                let headers = token
                    .map(|t| vec![("authorization".into(), basic(&t))])
                    .unwrap_or_default();
                Ok(Decision::allow_with(headers, Vec::new()))
            }
            Service::ReceivePack => {
                let Some(grant) = session.writable_repo_for(&git.repo) else {
                    let writable: Vec<&str> = session.writable_repos().map(|g| g.repo).collect();
                    let can = if writable.is_empty() {
                        String::new()
                    } else {
                        format!(" (it can push to {})", writable.join(", "))
                    };
                    return Ok(Decision::deny(format!(
                        "sloppod: this session can't push to {}{can}. Ask for write access with the \
                         request_repository tool.\n",
                        git.repo
                    )));
                };
                let (repo, branch) = (grant.repo, grant.branch);
                let Some(app) = self.app(session).await else {
                    return Ok(Decision::deny(format!(
                        "sloppod: {} has no GitHub App configured.\n",
                        session.owner
                    )));
                };
                if !git.advertisement {
                    match pushed_refs(&request.body) {
                        Some(push) if !push.refs.is_empty() => {
                            if let Some(bad) = push.refs.iter().find(|r| {
                                !r.strip_prefix("refs/heads/")
                                    .is_some_and(|b| is_session_branch(branch, b))
                            }) {
                                let message = format!(
                                    "sloppod: pushes may only update your session branch `{branch}` or branches \
                                     named after it (`{branch}-<name>`), so `{bad}` was refused. Push with \
                                     `git push origin HEAD:{branch}`. To get changes into another branch, propose a \
                                     pull request with the propose_pull_request tool."
                                );
                                return Ok(push.reject(&message));
                            }
                            // Auto-merge must only merge what was reviewed (ADR 3): new commits on its head turn
                            // it off. If every pull request it's turned off for knows what was reviewed, the push goes
                            // through and is rechecked once it lands: auto-merge goes back on if the change is still
                            // the one reviewed (#106), and the agent hears either way. Otherwise the agent hears
                            // before the push goes through.
                            for (name, new) in push.refs.iter().zip(&push.new) {
                                let Some(pushed) = name.strip_prefix("refs/heads/") else {
                                    continue;
                                };
                                self.pushes.admit(repo, pushed);
                                // Holds the branch until this push is decided, and from now on no recheck of an earlier
                                // push turns auto-merge back on.
                                let admission = self.rechecks.admit(repo, pushed).await;
                                let waiting = admission.waiting();
                                let disabled =
                                    app.disable_auto_merge(repo, pushed).await.map_err(err)?;
                                if disabled.is_empty() && waiting.is_empty() {
                                    continue;
                                }
                                let reviewed = match (session.session_id.parse(), is_deletion(new))
                                {
                                    (Ok(session_id), false) => self
                                        .rechecks
                                        .reviewed(repo, &disabled)
                                        .await
                                        .map_err(err)?
                                        .map(|pulls| (session_id, pulls)),
                                    _ => None,
                                };
                                if let Some((session_id, pulls)) = reviewed {
                                    let number = admission.number;
                                    self.rechecks.send(Recheck {
                                        owner: session.owner.clone(),
                                        session: session_id,
                                        repo: repo.to_owned(),
                                        branch: pushed.to_owned(),
                                        pulls: admission.recheck(pulls),
                                        sha: new.clone(),
                                        admission: number,
                                    });
                                } else {
                                    admission.forget();
                                    let mut affected = disabled.clone();
                                    affected.extend(waiting.iter().copied());
                                    affected.sort_unstable();
                                    affected.dedup();
                                    let pulls: Vec<String> =
                                        affected.iter().map(|n| format!("#{n}")).collect();
                                    let message = format!(
                                        "sloppod: `{pushed}` is the head of {}, which was set to merge once its checks \
                                         pass. New commits there haven't been reviewed, so auto-merge is now off and it \
                                         needs merging by hand. Push again to go ahead.",
                                        pulls.join(", ")
                                    );
                                    return Ok(push.reject(&message));
                                }
                            }
                        }
                        // Before a push larger than its post buffer, git checks it can talk to the server with a
                        // request that updates nothing.
                        _ if is_push_probe(request) => {}
                        _ => {
                            return Ok(Decision::deny(
                                "sloppod: could not read which refs this push updates.\n",
                            ));
                        }
                    }
                }
                let token = app.push_token(repo).await.map_err(err)?;
                Ok(Decision::allow_with(
                    vec![("authorization".into(), basic(&token))],
                    Vec::new(),
                ))
            }
        }
    }

    async fn api(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        let path = request.path_only();
        let read = match (request.method.as_str(), path) {
            ("GET" | "HEAD", _) => true,
            ("POST", "/markdown" | "/markdown/raw") => true,
            ("POST", "/graphql") => {
                if request.body_truncated {
                    return Ok(Decision::deny(
                        "sloppod: GraphQL request too large to inspect.\n",
                    ));
                }
                match graphql_is_read_only(&request.body) {
                    Ok(true) => true,
                    Ok(false) => {
                        return Ok(Decision::deny(proposal_hint(
                            "GraphQL mutations are refused",
                            session,
                        )));
                    }
                    Err(e) => {
                        return Ok(Decision::deny(format!(
                            "sloppod: could not parse the GraphQL request: {e}\n"
                        )));
                    }
                }
            }
            _ => false,
        };
        if !read {
            return Ok(Decision::deny(proposal_hint(
                &format!(
                    "direct GitHub writes are refused ({} {path})",
                    request.method
                ),
                session,
            )));
        }
        // Reads carry a read-only token covering the granted repositories in one installation, which also reads
        // public data: the installation of the repository the path names, if it's granted, or else the first granted
        // repository whose installation still works. Without any, or without a GitHub App, reads are unauthenticated.
        let granted: Vec<&str> = session.repos().collect();
        let token = match self.app(session).await {
            Some(app) => {
                let order = read_token_order(path, &granted);
                first_token(&session.session_id, order, READ_TOKEN_BUDGET, |repo| {
                    app.read_token(repo, &granted)
                })
                .await
            }
            None => None,
        };
        let headers = token
            .map(|t| vec![("authorization".into(), format!("token {t}"))])
            .unwrap_or_default();
        Ok(Decision::allow_with(headers, Vec::new()))
    }
}

#[async_trait]
impl RequestPolicy for GitHubPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn hosts(&self) -> Vec<String> {
        vec!["github.com".into(), "api.github.com".into()]
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if !session.has(capability::GITHUB) {
            return Ok(Decision::deny(
                "sloppod: this session has no GitHub access.\n",
            ));
        }
        match request.host.as_str() {
            "api.github.com" => self.api(session, request).await,
            _ => {
                if let Some(git) = GitRequest::parse(request) {
                    return self.git(session, request, git).await;
                }
                if matches!(request.method.as_str(), "GET" | "HEAD") {
                    return Ok(Decision::allow_with(Vec::new(), Vec::new()));
                }
                Ok(Decision::deny(proposal_hint(
                    &format!(
                        "{} {} on github.com is refused",
                        request.method,
                        request.path_only()
                    ),
                    session,
                )))
            }
        }
    }
}

fn proposal_hint(what: &str, session: &SessionContext) -> String {
    let pull_request = match session.writable_repo() {
        Some(grant) => format!(
            "to open a pull request, push to your session branch `{}` and call the propose_pull_request tool",
            grant.branch
        ),
        None => "to change a repository, ask for write access with the request_repository tool, then push to your \
                 session branch and call propose_pull_request"
            .to_owned(),
    };
    format!(
        "sloppod: {what}. Outward actions go through human review: {pull_request}. To file issues, call the \
         propose_issues tool. Other GitHub writes (comments, reviews, releases) aren't available to sessions yet.\n"
    )
}

/// How long finding a read token may take, well inside Envoy's 10 s limit on the whole decision.
const READ_TOKEN_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);

/// The first token `mint` gives for `repos`, tried in order, or `None` if none does within `budget`. Each attempt can
/// take a while when GitHub is slow, and Envoy gives up on the whole decision after 10 s; reading unauthenticated
/// before then is better than failing the read.
async fn first_token<'a, F, Fut>(
    session: &str,
    repos: Vec<&'a str>,
    budget: std::time::Duration,
    mint: F,
) -> Option<String>
where
    F: Fn(&'a str) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    let attempts = async {
        for repo in repos {
            match mint(repo).await {
                Ok(token) => return Some(token),
                Err(e) => tracing::warn!(session, repo, error = %e, "minting a read token"),
            }
        }
        None
    };
    tokio::time::timeout(budget, attempts)
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(
                session,
                "minting a read token took too long; reading without one"
            );
            None
        })
}

/// Granted repositories to mint a read token through, in order: the one the API path names first, if any.
fn read_token_order<'a>(path: &str, granted: &[&'a str]) -> Vec<&'a str> {
    let named = path.strip_prefix("/repos/").and_then(|rest| {
        let mut parts = rest.splitn(3, '/');
        Some(format!("{}/{}", parts.next()?, parts.next()?))
    });
    let first = named.and_then(|named| granted.iter().find(|g| same_repo(g, &named)).copied());
    first
        .into_iter()
        .chain(granted.iter().copied().filter(|g| Some(*g) != first))
        .collect()
}

fn basic(token: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Service {
    UploadPack,
    ReceivePack,
}

#[derive(Debug)]
struct GitRequest {
    repo: String,
    service: Service,
    /// `GET info/refs`, as opposed to the `POST` carrying the actual exchange.
    advertisement: bool,
}

impl GitRequest {
    fn parse(request: &HttpRequest) -> Option<Self> {
        let path = request.path_only().trim_start_matches('/');
        let mut parts = path.splitn(3, '/');
        let owner = parts.next()?;
        let repo = repo_from_git_path(parts.next()?);
        let rest = parts.next()?;
        let (service, advertisement) = match (request.method.as_str(), rest) {
            ("GET", "info/refs") => {
                let query = request.query().unwrap_or_default();
                let service = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("service="))?;
                match service {
                    "git-upload-pack" => (Service::UploadPack, true),
                    "git-receive-pack" => (Service::ReceivePack, true),
                    _ => return None,
                }
            }
            ("POST", "git-upload-pack") => (Service::UploadPack, false),
            ("POST", "git-receive-pack") => (Service::ReceivePack, false),
            _ => return None,
        };
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(Self {
            repo: format!("{owner}/{repo}"),
            service,
            advertisement,
        })
    }
}

/// A `git-receive-pack` request's ref updates and the capabilities the client asked for.
struct Push {
    refs: Vec<String>,
    /// The commit each of `refs` is set to.
    new: Vec<String>,
    capabilities: Vec<String>,
}

/// The all-zero commit git sets a ref to when deleting it.
fn is_deletion(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

fn pkt(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}

impl Push {
    /// Refuses the whole push in git's own response format, so git shows the reason as `remote:` output and
    /// `! [remote rejected]` lines instead of a bare HTTP error.
    fn reject(&self, message: &str) -> Decision {
        let mut report = pkt("unpack ok\n");
        for name in &self.refs {
            report.push_str(&pkt(&format!("ng {name} refused by sloppod, see above\n")));
        }
        report.push_str("0000");
        let sideband = self
            .capabilities
            .iter()
            .any(|c| c == "side-band-64k" || c == "side-band");
        let body = if sideband {
            let mut body = pkt(&format!("\u{2}{message}\n"));
            // Band 1 carries the report; it would need splitting past 64k, which a ref list never reaches.
            body.push_str(&pkt(&format!("\u{1}{report}")));
            body.push_str("0000");
            body
        } else {
            report
        };
        Decision::Deny {
            status: 200,
            message: body,
            content_type: Some("application/x-git-receive-pack-result".into()),
        }
    }
}

/// git's probe before a large push: a `git-receive-pack` request with no commands, just a flush packet.
fn is_push_probe(request: &HttpRequest) -> bool {
    !request.body_truncated && request.body == b"0000"
}

/// Reads the leading pkt-lines of a `git-receive-pack` request.
fn pushed_refs(body: &[u8]) -> Option<Push> {
    let mut push = Push {
        refs: Vec::new(),
        new: Vec::new(),
        capabilities: Vec::new(),
    };
    let mut rest = body;
    loop {
        if rest.len() < 4 {
            return None;
        }
        let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).ok()?, 16).ok()?;
        if len == 0 {
            return Some(push);
        }
        if len < 4 || rest.len() < len {
            return None;
        }
        let line = &rest[4..len];
        rest = &rest[len..];
        let mut parts = line.splitn(2, |b| *b == 0);
        let command = std::str::from_utf8(parts.next()?)
            .ok()?
            .trim_end_matches('\n');
        if let Some(capabilities) = parts.next() {
            push.capabilities = std::str::from_utf8(capabilities)
                .ok()?
                .split_whitespace()
                .map(str::to_owned)
                .collect();
        }
        if command.starts_with("shallow ") {
            continue;
        }
        // A signed push starts with a push certificate; not supported.
        if command.starts_with("push-cert") {
            return None;
        }
        let mut fields = command.split(' ');
        let (_old, new, name) = (fields.next()?, fields.next()?, fields.next()?);
        push.refs.push(name.to_owned());
        push.new.push(new.to_owned());
    }
}

fn graphql_is_read_only(body: &[u8]) -> std::result::Result<bool, String> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let requests = match &json {
        serde_json::Value::Array(batch) => batch.iter().collect::<Vec<_>>(),
        other => vec![other],
    };
    for request in requests {
        let query = request["query"].as_str().ok_or("no query")?;
        let document = graphql_parser::parse_query::<&str>(query).map_err(|e| e.to_string())?;
        for definition in document.definitions {
            if let Definition::Operation(
                OperationDefinition::Mutation(_) | OperationDefinition::Subscription(_),
            ) = definition
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(line: &str) -> Vec<u8> {
        let mut out = format!("{:04x}", line.len() + 4).into_bytes();
        out.extend_from_slice(line.as_bytes());
        out
    }

    #[test]
    fn reads_pushed_refs() {
        let zero = "0".repeat(40);
        let new = "a".repeat(40);
        let mut body = pkt(&format!(
            "{zero} {new} refs/heads/sloppod/x\0report-status side-band-64k\n"
        ));
        body.extend(pkt(&format!("{new} {zero} refs/heads/main\n")));
        body.extend(b"0000PACK....");
        let push = pushed_refs(&body).unwrap();
        assert_eq!(
            push.refs,
            vec![
                "refs/heads/sloppod/x".to_owned(),
                "refs/heads/main".to_owned()
            ]
        );
        assert_eq!(push.new, vec![new.clone(), zero.clone()]);
        assert!(!is_deletion(&push.new[0]) && is_deletion(&push.new[1]));
        assert_eq!(push.capabilities, vec!["report-status", "side-band-64k"]);
        assert!(pushed_refs(b"00").is_none());
        let probe = |body: &[u8], body_truncated: bool| HttpRequest {
            method: "POST".into(),
            host: "github.com".into(),
            path: "/o/r.git/git-receive-pack".into(),
            headers: Default::default(),
            body: body.to_vec(),
            body_truncated,
        };
        assert!(is_push_probe(&probe(b"0000", false)));
        assert!(!is_push_probe(&probe(b"0000", true)));
        assert!(!is_push_probe(&probe(b"0000PACK", false)));
        assert!(!is_push_probe(&probe(&body, false)));
        let Decision::Deny {
            status, message, ..
        } = push.reject("no")
        else {
            panic!("expected a rejection");
        };
        assert_eq!(status, 200);
        assert!(message.starts_with("0008\u{2}no\n"));
        assert!(message.contains("ng refs/heads/main refused by sloppod"));
        assert!(message.ends_with("00000000"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_token_service_falls_back_to_reading_without_one_in_time() {
        let budget = std::time::Duration::from_secs(6);
        let slow = |_: &str| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            anyhow::bail!("timed out")
        };
        let started = tokio::time::Instant::now();
        assert_eq!(
            first_token("s", vec!["o/a", "o/b", "o/c"], budget, slow).await,
            None
        );
        assert_eq!(started.elapsed(), budget);

        let second_works = |repo: &str| {
            let repo = repo.to_owned();
            async move {
                if repo == "o/a" {
                    anyhow::bail!("gone")
                }
                Ok(format!("token for {repo}"))
            }
        };
        assert_eq!(
            first_token("s", vec!["o/a", "o/b"], budget, second_works).await,
            Some("token for o/b".to_owned())
        );
    }

    #[test]
    fn reads_go_through_the_named_repository_first() {
        let granted = ["o/a", "o/b", "p/c"];
        assert_eq!(
            read_token_order("/repos/o/b/pulls", &granted),
            ["o/b", "o/a", "p/c"]
        );
        assert_eq!(
            read_token_order("/repos/O/B", &granted),
            ["o/b", "o/a", "p/c"]
        );
        assert_eq!(
            read_token_order("/repos/x/y/issues", &granted),
            ["o/a", "o/b", "p/c"]
        );
        assert_eq!(read_token_order("/user", &granted), ["o/a", "o/b", "p/c"]);
        assert!(read_token_order("/user", &[]).is_empty());
    }

    #[test]
    fn graphql_queries_and_mutations() {
        assert_eq!(
            graphql_is_read_only(br#"{"query":"query { viewer { login } }"}"#),
            Ok(true)
        );
        assert_eq!(
            graphql_is_read_only(br#"{"query":"{ viewer { login } }"}"#),
            Ok(true)
        );
        assert_eq!(
            graphql_is_read_only(br#"{"query":"mutation { addStar(input:{starrableId:\"x\"}) { clientMutationId } }"}"#),
            Ok(false)
        );
        assert_eq!(
            graphql_is_read_only(br#"{"query":"query A { a } mutation B { b }"}"#),
            Ok(false)
        );
    }

    #[test]
    fn parses_git_paths() {
        let request = |method: &str, path: &str| HttpRequest {
            method: method.into(),
            host: "github.com".into(),
            path: path.into(),
            headers: Default::default(),
            body: Vec::new(),
            body_truncated: false,
        };
        let git = GitRequest::parse(&request(
            "GET",
            "/o/r.git/info/refs?service=git-receive-pack",
        ))
        .unwrap();
        assert_eq!(
            (git.repo.as_str(), git.service, git.advertisement),
            ("o/r", Service::ReceivePack, true)
        );
        let git = GitRequest::parse(&request("POST", "/o/r/git-upload-pack")).unwrap();
        assert_eq!(
            (git.repo.as_str(), git.service, git.advertisement),
            ("o/r", Service::UploadPack, false)
        );
        assert!(GitRequest::parse(&request("GET", "/o/r/pulls")).is_none());
    }
}

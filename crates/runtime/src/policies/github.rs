use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use graphql_parser::query::{Definition, OperationDefinition};
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::credentials::Credentials;
use crate::github::GitHubApp;
use crate::proposals::is_session_branch;

/// GitHub: reads and session-branch pushes with the owner's GitHub App, through tokens scoped to the session's
/// grants (ADR 11); all other writes go through proposals.
pub struct GitHubPolicy {
    credentials: Arc<Credentials>,
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

impl GitHubPolicy {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
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
                    Some(app) if granted.iter().any(|r| r.eq_ignore_ascii_case(&git.repo)) => {
                        app.read_token(&git.repo, &granted).await.ok()
                    }
                    _ => None,
                };
                let headers = token
                    .map(|t| vec![("authorization".into(), basic(&t))])
                    .unwrap_or_default();
                Ok(Decision::allow_with(headers, vec!["authorization".into()]))
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
                            // it off, and the agent hears about it before the push goes through.
                            for pushed in push
                                .refs
                                .iter()
                                .filter_map(|r| r.strip_prefix("refs/heads/"))
                            {
                                let disabled =
                                    app.disable_auto_merge(repo, pushed).await.map_err(err)?;
                                if !disabled.is_empty() {
                                    let pulls: Vec<String> =
                                        disabled.iter().map(|n| format!("#{n}")).collect();
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
        // Reads carry a read-only token covering the granted repositories (in the first one's installation), which
        // also reads public data. Sessions without a repository grant, or a GitHub App, read unauthenticated.
        let granted: Vec<&str> = session.repos().collect();
        let token = match (self.app(session).await, granted.first()) {
            (Some(app), Some(repo)) => Some(app.read_token(repo, &granted).await.map_err(err)?),
            _ => None,
        };
        let headers = token
            .map(|t| vec![("authorization".into(), format!("token {t}"))])
            .unwrap_or_default();
        Ok(Decision::allow_with(headers, vec!["authorization".into()]))
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
        if !session.has("github") {
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
                    return Ok(Decision::allow_with(
                        Vec::new(),
                        vec!["authorization".into(), "cookie".into()],
                    ));
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
        let repo = parts.next()?.trim_end_matches(".git");
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
    capabilities: Vec<String>,
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

/// Reads the leading pkt-lines of a `git-receive-pack` request.
fn pushed_refs(body: &[u8]) -> Option<Push> {
    let mut push = Push {
        refs: Vec::new(),
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
        let (_old, _new, name) = (fields.next()?, fields.next()?, fields.next()?);
        push.refs.push(name.to_owned());
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
        assert_eq!(push.capabilities, vec!["report-status", "side-band-64k"]);
        assert!(pushed_refs(b"00").is_none());
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

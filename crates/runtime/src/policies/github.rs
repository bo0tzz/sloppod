use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use graphql_parser::query::{Definition, OperationDefinition};
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::github::GitHubApp;

/// GitHub: reads with the app's token, pushes only to the session branch, all other writes through proposals.
pub struct GitHubPolicy {
    app: Arc<GitHubApp>,
}

impl GitHubPolicy {
    pub fn new(app: Arc<GitHubApp>) -> Self {
        Self { app }
    }

    async fn token(&self, repo: &str) -> Result<String> {
        self.app
            .token_for(repo)
            .await
            .map_err(|e| Error::new(e.to_string()))
    }

    async fn git(
        &self,
        session: &SessionContext,
        request: &HttpRequest,
        git: GitRequest,
    ) -> Result<Decision> {
        match git.service {
            Service::UploadPack => {
                // Public repositories outside the app's installations are still readable without a token.
                let headers = match self.token(&git.repo).await {
                    Ok(token) => vec![("authorization".into(), basic(&token))],
                    Err(_) => Vec::new(),
                };
                Ok(Decision::allow_with(headers, vec!["authorization".into()]))
            }
            Service::ReceivePack => {
                if !git.repo.eq_ignore_ascii_case(&session.repo) {
                    return Ok(Decision::deny(format!(
                        "sloppod: this session can only push to {}, not {}.\n",
                        session.repo, git.repo
                    )));
                }
                if git.advertisement {
                    let token = self.token(&session.repo).await?;
                    return Ok(Decision::allow_with(
                        vec![("authorization".into(), basic(&token))],
                        Vec::new(),
                    ));
                }
                match pushed_refs(&request.body) {
                    Some(refs) if !refs.is_empty() => {
                        let allowed = format!("refs/heads/{}", session.branch);
                        if let Some(bad) = refs
                            .iter()
                            .find(|r| **r != allowed && !r.starts_with(&format!("{allowed}/")))
                        {
                            return Ok(Decision::deny(format!(
                                "sloppod: pushes may only update your session branch `{}`; `{bad}` was refused. \
                                 Push with `git push origin HEAD:{}`. To get changes into another branch, \
                                 propose a pull request with the propose_pull_request tool.\n",
                                session.branch, session.branch
                            )));
                        }
                        let token = self.token(&session.repo).await?;
                        Ok(Decision::allow_with(
                            vec![("authorization".into(), basic(&token))],
                            Vec::new(),
                        ))
                    }
                    _ => Ok(Decision::deny(
                        "sloppod: could not read which refs this push updates.\n",
                    )),
                }
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
        let token = self.token(&session.repo).await?;
        Ok(Decision::allow_with(
            vec![("authorization".into(), format!("token {token}"))],
            Vec::new(),
        ))
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
    format!(
        "sloppod: {what}. Outward actions go through human review: to open a pull request, push to your \
         session branch `{}` and call the propose_pull_request tool. Other GitHub writes (comments, issues, \
         reviews, releases) aren't available to sessions yet.\n",
        session.branch
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

/// Refs updated by a `git-receive-pack` request, read from its leading pkt-lines.
fn pushed_refs(body: &[u8]) -> Option<Vec<String>> {
    let mut refs = Vec::new();
    let mut rest = body;
    loop {
        if rest.len() < 4 {
            return None;
        }
        let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).ok()?, 16).ok()?;
        if len == 0 {
            return Some(refs);
        }
        if len < 4 || rest.len() < len {
            return None;
        }
        let line = &rest[4..len];
        rest = &rest[len..];
        let line = line.split(|b| *b == 0).next()?;
        let line = std::str::from_utf8(line).ok()?.trim_end_matches('\n');
        if line.starts_with("shallow ") {
            continue;
        }
        // A signed push starts with a push certificate; not supported.
        if line.starts_with("push-cert") {
            return None;
        }
        let mut fields = line.split(' ');
        let (_old, _new, name) = (fields.next()?, fields.next()?, fields.next()?);
        refs.push(name.to_owned());
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
        assert_eq!(
            pushed_refs(&body).unwrap(),
            vec![
                "refs/heads/sloppod/x".to_owned(),
                "refs/heads/main".to_owned()
            ]
        );
        assert_eq!(pushed_refs(b"00"), None);
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

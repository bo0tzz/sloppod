use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use graphql_parser::query::{
    Definition, OperationDefinition, Selection, SelectionSet, Value as GraphqlValue,
};
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};
use sloppod_contracts::{repo_from_git_path, same_repo};

use crate::capabilities::{Part, holds};
use crate::credentials::Credentials;
use tracing::Instrument;

use crate::github::{GitHubApp, PushLog, acting_span};
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

    /// The identity the session acts with on `repo` (ADR 29): the GitHub App, or the owner's token its grant names.
    /// `None` for the App when there's none; a named token that's gone is a refusal, never a fallback to the App.
    async fn acting_as(
        &self,
        session: &SessionContext,
        repo: &str,
    ) -> std::result::Result<Option<Arc<GitHubApp>>, Decision> {
        let Some(credential) = session.repo_credential(repo) else {
            return Ok(self.app(session).await);
        };
        match self
            .credentials
            .github_as(&session.owner, Some(credential))
            .await
        {
            Ok(client) => Ok(Some(client)),
            Err(e) => Err(Decision::deny(format!(
                "sloppod: this session acts on {repo} with a GitHub token of the user's that can't be used ({e}). \
                 Tell the user; they can switch the grant to another token in the session's grants.\n"
            ))),
        }
    }

    /// Whether the owner's token the session acts with on `repo` reaches nothing the session isn't granted with it:
    /// every repository it listed when it was added is granted with that token, and it listed them all.
    async fn reaches_only_grants(&self, session: &SessionContext, repo: &str) -> bool {
        let Some(credential) = session.repo_credential(repo) else {
            return false;
        };
        let Ok(Some(row)) = self
            .credentials
            .github_user(&session.owner, credential)
            .await
        else {
            return false;
        };
        let Some(reach) = row.config["repositories"].as_array() else {
            return false;
        };
        row.config["more_repositories"].as_bool() == Some(false)
            && reach.iter().all(|reached| {
                reached
                    .as_str()
                    .is_some_and(|r| session.repo_credential(r) == Some(credential))
            })
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
                let token = if granted.iter().any(|r| same_repo(r, &git.repo)) {
                    match self.acting_as(session, &git.repo).await {
                        Ok(Some(client)) => {
                            let token = client
                                .read_token(&git.repo, &app_repos(session, &granted))
                                .await
                                .ok();
                            audit(session, &client, &git.repo, request);
                            token
                        }
                        Ok(None) => None,
                        Err(refusal) => return Ok(refusal),
                    }
                } else {
                    None
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
                    let next = session.role.repo_next_step();
                    return Ok(Decision::deny(format!(
                        "sloppod: this session can't push to {}{can}{next}.\n",
                        git.repo
                    )));
                };
                let (repo, branch) = (grant.repo, grant.branch);
                let app = match self.acting_as(session, repo).await {
                    Ok(Some(app)) => app,
                    Ok(None) => {
                        return Ok(Decision::deny(format!(
                            "sloppod: {} has no GitHub App configured.\n",
                            session.owner
                        )));
                    }
                    Err(refusal) => return Ok(refusal),
                };
                // Before the guard, which may refuse the push, so every push made as the user is logged.
                audit(session, &app, repo, request);
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
                                        credential: session
                                            .repo_credential(repo)
                                            .map(str::to_owned),
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
                                    let pulls: Vec<String> = affected
                                        .iter()
                                        .map(|n| crate::pulls::pull_ref(repo, *n))
                                        .collect();
                                    let message = format!(
                                        "sloppod: `{pushed}` is the head of {}, which was set to merge once its checks \
                                         pass. New commits there haven't been reviewed, so auto-merge is now off and it \
                                         needs merging by hand. Push again to go ahead. {}",
                                        pulls.join(", "),
                                        crate::pulls::TELL_THE_USER
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
        // public data: the installation of the repository the request names, if it's granted, or else the first
        // granted repository whose installation still works. A session with no repository it reads as the App gets a
        // token for public data only. Without a GitHub App, or when minting fails, reads are unauthenticated.
        // Repositories the session acts on with one of the owner's tokens are read with that token, and only when the
        // request names nothing else; the App's tokens never cover them. GraphQL also needs a token that reaches nothing
        // beyond what it's granted for, since a query can go on from a repository it names to others.
        let granted: Vec<&str> = session.repos().collect();
        let graphql = path == "/graphql";
        let named: Vec<String> = if graphql {
            graphql_repos(&request.body).unwrap_or_default()
        } else {
            named_repo(path).into_iter().collect()
        };
        let mut as_user = user_token_repo(session, &granted, &named);
        if let Some(repo) = as_user
            && graphql
            && !self.reaches_only_grants(session, repo).await
        {
            tracing::info!(
                session = %session.session_id,
                %repo,
                "GraphQL read as the App: the owner's token reaches more than this session is granted with it; \
                 REST reads of the repository use the token"
            );
            as_user = None;
        }
        if let Some(repo) = as_user {
            return match self.acting_as(session, repo).await {
                Ok(Some(client)) => {
                    let token = client.read_token(repo, &[]).await.map_err(err)?;
                    for repo in &named {
                        audit(session, &client, repo, request);
                    }
                    Ok(Decision::allow_with(
                        vec![("authorization".into(), format!("token {token}"))],
                        Vec::new(),
                    ))
                }
                Ok(None) => Ok(anonymous(session, path)),
                Err(refusal) => Ok(refusal),
            };
        }
        let granted = app_repos(session, &granted);
        let token = match self.app(session).await {
            Some(app) if granted.is_empty() => public_token(&session.session_id, &app).await,
            Some(app) => {
                let order = read_token_order(&named, &granted);
                first_token(&session.session_id, order, READ_TOKEN_BUDGET, |repo| {
                    app.read_token(repo, &granted)
                })
                .await
            }
            None => None,
        };
        Ok(match token {
            Some(t) => Decision::allow_with(
                vec![("authorization".into(), format!("token {t}"))],
                Vec::new(),
            ),
            None => anonymous(session, path),
        })
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
        if !holds(session, &Part::Github) {
            return Ok(Decision::deny(
                "sloppod: this session has no GitHub access.\n",
            ));
        }
        match request.host.as_str() {
            "api.github.com" => self.api(session, request).await,
            _ => {
                if let Some(git) = GitRequest::parse(request) {
                    let span = acting_span(&session.session_id, session.repo_credential(&git.repo));
                    return self.git(session, request, git).instrument(span).await;
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
    // A review session has none of the proposal tools, so there's nothing to point it to.
    if !session.role.acts() {
        return format!(
            "sloppod: {what}. This session can't make GitHub writes or propose them.\n"
        );
    }
    let pull_request = match session.writable_repo() {
        Some(grant) => format!(
            "to open a pull request, push to your session branch `{}` and call the propose_pull_request tool",
            grant.branch
        ),
        None => format!(
            "to change a repository, {}, then push to your session branch and call propose_pull_request",
            session.role.repo_advice().unwrap_or_default()
        ),
    };
    format!(
        "sloppod: {what}. Outward actions go through human review: {pull_request}. Issues are filed with \
         propose_issues; propose_issue_updates and propose_pull_request_updates comment on, edit and close existing \
         issues and open pull requests; propose_stack opens pull requests that build on each other, and \
         request_code_review has a branch reviewed without proposing anything. Other GitHub writes (reviews, \
         releases) aren't available to sessions yet.\n"
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

/// A token for public data from the owner's App, for a session with no repository to scope a read token to, or
/// `None` if none comes within the budget.
async fn public_token(session: &str, app: &GitHubApp) -> Option<String> {
    match tokio::time::timeout(READ_TOKEN_BUDGET, app.public_read_token()).await {
        Ok(Ok(token)) => Some(token),
        Ok(Err(e)) => {
            tracing::warn!(session, error = %e, "minting a public read token");
            None
        }
        Err(_) => {
            tracing::warn!(session, "minting a public read token took too long");
            None
        }
    }
}

/// Lets a read through without a token. GitHub allows 60 of those an hour per IP, shared by the cluster's egress,
/// and refuses GraphQL outright, so each is logged.
fn anonymous(session: &SessionContext, path: &str) -> Decision {
    tracing::warn!(session = %session.session_id, path, "reading GitHub without a token");
    Decision::allow_with(Vec::new(), Vec::new())
}

/// The repository to read through with one of the owner's tokens: the first of `named`, when every one of them is
/// granted with the same token. A request that names no repository never gets the token, which reaches more than
/// the grants do (ADR 29).
fn user_token_repo<'a>(
    session: &SessionContext,
    granted: &[&str],
    named: &'a [String],
) -> Option<&'a str> {
    let first = named.first()?;
    let credential = session.repo_credential(first)?;
    named
        .iter()
        .all(|repo| {
            granted.iter().any(|g| same_repo(g, repo))
                && session.repo_credential(repo) == Some(credential)
        })
        .then_some(first.as_str())
}

/// The repository an API path names, `/repos/<owner>/<name>/…`.
fn named_repo(path: &str) -> Option<String> {
    path.strip_prefix("/repos/").and_then(|rest| {
        let mut parts = rest.splitn(3, '/');
        Some(format!("{}/{}", parts.next()?, parts.next()?))
    })
}

/// The granted repositories the session acts on as the GitHub App, the only ones its tokens may cover.
fn app_repos<'a>(session: &SessionContext, granted: &[&'a str]) -> Vec<&'a str> {
    granted
        .iter()
        .copied()
        .filter(|repo| session.repo_credential(repo).is_none())
        .collect()
}

/// Every request made with one of the owner's tokens is logged with the session and the grant's credential (ADR 28).
fn audit(session: &SessionContext, client: &GitHubApp, repo: &str, request: &HttpRequest) {
    if let Some(login) = client.user_login() {
        tracing::info!(
            session = %session.session_id,
            credential = session.repo_credential(repo).unwrap_or_default(),
            as_user = login,
            %repo,
            method = %request.method,
            path = %request.path_only(),
            "request with the owner's GitHub token"
        );
    }
}

/// Granted repositories to mint a read token through, in order: the first the request names, if any, first.
fn read_token_order<'a>(named: &[String], granted: &[&'a str]) -> Vec<&'a str> {
    let first = named
        .iter()
        .find_map(|named| granted.iter().find(|g| same_repo(g, named)).copied());
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

/// The requests in a GraphQL body, which may be a batch.
fn graphql_requests(json: &serde_json::Value) -> Vec<&serde_json::Value> {
    match json {
        serde_json::Value::Array(batch) => batch.iter().collect(),
        other => vec![other],
    }
}

/// The repositories a GraphQL query reads: those its `repository(owner:, name:)` lookups name, at any depth, from
/// literals or the request's `variables`. `None` when it reads anything else from the root (`viewer`, `search`,
/// `node`), or looks up a repository whose owner or name it doesn't say, so can't be matched against the grants.
fn graphql_repos(body: &[u8]) -> Option<Vec<String>> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let mut repos = Vec::new();
    for request in graphql_requests(&json) {
        let document = graphql_parser::parse_query::<&str>(request["query"].as_str()?).ok()?;
        let variables = &request["variables"];
        for definition in &document.definitions {
            let set = match definition {
                Definition::Operation(OperationDefinition::Query(query)) => &query.selection_set,
                Definition::Operation(OperationDefinition::SelectionSet(set)) => set,
                Definition::Operation(_) => return None,
                Definition::Fragment(fragment) => {
                    repositories_in(&fragment.selection_set, variables, &mut repos)?;
                    continue;
                }
            };
            let only_repositories = set.items.iter().all(|item| {
                matches!(item, Selection::Field(field) if matches!(field.name, "repository" | "__typename"))
            });
            if !only_repositories {
                return None;
            }
            repositories_in(set, variables, &mut repos)?;
        }
    }
    (!repos.is_empty()).then_some(repos)
}

/// Adds the repositories `set`'s `repository` lookups name to `repos`, or `None` for a lookup it can't tell. A
/// `repository` field without arguments is the repository of an object already reached, not a lookup.
fn repositories_in<'a>(
    set: &SelectionSet<'a, &'a str>,
    variables: &serde_json::Value,
    repos: &mut Vec<String>,
) -> Option<()> {
    for item in &set.items {
        let field = match item {
            Selection::Field(field) => field,
            Selection::InlineFragment(fragment) => {
                repositories_in(&fragment.selection_set, variables, repos)?;
                continue;
            }
            Selection::FragmentSpread(_) => continue,
        };
        if field.name == "repository" && !field.arguments.is_empty() {
            let argument = |name: &str| {
                let (_, value) = field.arguments.iter().find(|(n, _)| *n == name)?;
                match value {
                    GraphqlValue::String(s) => Some(s.clone()),
                    GraphqlValue::Variable(v) => variables[*v].as_str().map(str::to_owned),
                    _ => None,
                }
            };
            repos.push(format!("{}/{}", argument("owner")?, argument("name")?));
        }
        repositories_in(&field.selection_set, variables, repos)?;
    }
    Some(())
}

fn graphql_is_read_only(body: &[u8]) -> std::result::Result<bool, String> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    for request in graphql_requests(&json) {
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

    fn context(role: sloppod_contracts::SessionRole) -> SessionContext {
        SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![],
            role,
        }
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_repository_without_github_reaches_no_github(db: sqlx::PgPool) {
        use base64::Engine;
        use serde_json::json;
        use sloppod_contracts::{Grant, capability};

        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let credentials =
            Credentials::new(db.clone(), crate::vault::Vault::from_base64(&key).unwrap())
                .await
                .unwrap();
        let (rechecks, _) = Rechecks::new(db);
        let policy = GitHubPolicy::new(Arc::new(credentials), Arc::default(), rechecks);
        let fetch = HttpRequest {
            method: "GET".into(),
            host: "github.com".into(),
            path: "/o/r.git/info/refs?service=git-upload-pack".into(),
            headers: Default::default(),
            body: Vec::new(),
            body_truncated: false,
        };
        let refused = |decision: Decision| matches!(decision, Decision::Deny { message, .. } if message.contains("no GitHub access"));

        // A sub-session narrowed to its repository, say, without github.
        let mut session = context(Default::default());
        session.grants = vec![
            Grant::new(capability::MODEL, json!({})),
            Grant::new(
                capability::REPO,
                json!({"repo": "o/r", "access": "rw", "branch": "sloppod/s"}),
            ),
        ];
        assert!(refused(policy.decide(&session, &fetch).await.unwrap()));
        session
            .grants
            .push(Grant::new(capability::GITHUB, json!({})));
        assert!(!refused(policy.decide(&session, &fetch).await.unwrap()));
    }

    /// A policy over the fake GitHub, with the owner's App and one token of theirs for each of `reaches` (the
    /// repositories it reaches, and whether there are more), `github_pat_0` on, whose credential ids it returns.
    async fn reads_policy(
        db: sqlx::PgPool,
        fake: crate::github::tests::Fake,
        reaches: &[(&[&str], bool)],
    ) -> (GitHubPolicy, Arc<crate::github::tests::Fake>, Vec<String>) {
        use crate::credentials::{GITHUB_APP, GITHUB_USER};
        use crate::github::GitHubApp;

        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let credentials =
            Credentials::new(db.clone(), crate::vault::Vault::from_base64(&key).unwrap())
                .await
                .unwrap();
        let (fake, app) = crate::github::tests::serve(fake).await;
        credentials
            .add_github_client("dev", GITHUB_APP, serde_json::json!({}), app)
            .await;
        let mut users = Vec::new();
        for (n, (repositories, more)) in reaches.iter().enumerate() {
            let client = GitHubApp::for_user_with_api(
                crate::github::API,
                format!("github_pat_{n}"),
                "octo".into(),
            )
            .unwrap();
            let config = serde_json::json!({
                "login": "octo",
                "repositories": repositories,
                "more_repositories": more,
            });
            let id = credentials
                .add_github_client("dev", GITHUB_USER, config, client)
                .await;
            users.push(id.to_string());
        }
        let (rechecks, _) = Rechecks::new(db);
        let policy = GitHubPolicy::new(Arc::new(credentials), Arc::default(), rechecks);
        (policy, fake, users)
    }

    /// A session holding `github` and read grants on `repos`, each with the credential it names, if any.
    fn reading(repos: &[(&str, Option<&str>)]) -> SessionContext {
        use serde_json::json;
        use sloppod_contracts::{Grant, capability};

        let mut session = context(Default::default());
        session
            .grants
            .push(Grant::new(capability::GITHUB, json!({})));
        for (repo, credential) in repos {
            let mut params = json!({"repo": repo, "access": "r"});
            if let Some(credential) = credential {
                params["credential"] = json!(credential);
            }
            session.grants.push(Grant::new(capability::REPO, params));
        }
        session
    }

    fn api_request(method: &str, path: &str, body: serde_json::Value) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            host: "api.github.com".into(),
            path: path.into(),
            headers: Default::default(),
            body: if body.is_null() {
                Vec::new()
            } else {
                body.to_string().into_bytes()
            },
            body_truncated: false,
        }
    }

    /// `gh pr view`'s query, with the repository in its variables.
    fn pr_view(repo: &str) -> HttpRequest {
        let (owner, name) = repo.split_once('/').unwrap();
        api_request(
            "POST",
            "/graphql",
            serde_json::json!({
                "query": "query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!) { \
                          repository(owner: $owner, name: $repo) { pullRequest(number: $pr_number) { title } } }",
                "variables": {"owner": owner, "repo": name, "pr_number": 1},
            }),
        )
    }

    /// The token a decision attaches, if any.
    fn token(decision: Decision) -> Option<String> {
        let Decision::Allow { set_headers, .. } = decision else {
            panic!("refused: {decision:?}");
        };
        set_headers
            .into_iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.trim_start_matches("token ").to_owned())
    }

    /// What the last App token minted was scoped to.
    fn last_minted(fake: &crate::github::tests::Fake) -> serde_json::Value {
        fake.minted.lock().unwrap().last().unwrap().1.clone()
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn graphql_reads_with_the_owners_token_only_on_its_grants(db: sqlx::PgPool) {
        use serde_json::json;

        let (policy, fake, users) = reads_policy(
            db,
            Default::default(),
            &[
                (&["u/mine", "U/Two"], false),
                (&["u/three"], false),
                (&["u/four", "u/secret"], false),
                (&["u/five"], true),
            ],
        )
        .await;
        let session = reading(&[
            ("o/r", None),
            ("u/mine", Some(&users[0])),
            ("u/two", Some(&users[0])),
            ("u/three", Some(&users[1])),
            ("u/four", Some(&users[2])),
            ("u/five", Some(&users[3])),
        ]);
        let decide = |request: HttpRequest| {
            let (policy, session) = (&policy, &session);
            async move { token(policy.decide(session, &request).await.unwrap()) }
        };
        let app_read = json!({"repositories": ["r"], "permissions": {
            "contents": "read", "issues": "read", "metadata": "read", "pull_requests": "read",
        }});

        assert_eq!(
            decide(pr_view("u/mine")).await.as_deref(),
            Some("github_pat_0")
        );
        assert_eq!(
            decide(pr_view("U/Mine")).await.as_deref(),
            Some("github_pat_0")
        );
        let both = api_request(
            "POST",
            "/graphql",
            json!({"query": "{ a: repository(owner: \"u\", name: \"mine\") { id } \
                    b: repository(owner: \"u\", name: \"two\") { id } }"}),
        );
        assert_eq!(decide(both).await.as_deref(), Some("github_pat_0"));

        // An App-granted or ungranted repository, or one named alongside a token-granted one, reads as the App.
        for request in [
            pr_view("o/r"),
            pr_view("x/y"),
            api_request(
                "POST",
                "/graphql",
                json!({"query": "{ repository(owner: \"u\", name: \"mine\") { id \
                        parent { repository(owner: \"x\", name: \"y\") { id } } } }"}),
            ),
        ] {
            assert_eq!(decide(request).await.as_deref(), Some("token-1"));
            assert_eq!(last_minted(&fake), app_read);
        }
        // Repositories granted with two different tokens.
        let mixed = api_request(
            "POST",
            "/graphql",
            json!({"query": "{ a: repository(owner: \"u\", name: \"mine\") { id } \
                    b: repository(owner: \"u\", name: \"three\") { id } }"}),
        );
        assert_eq!(decide(mixed).await.as_deref(), Some("token-1"));
        assert_eq!(last_minted(&fake), app_read);

        // Nothing that reaches past the named repositories reads as the owner.
        for query in [
            "{ viewer { login } }",
            "{ viewer { login } repository(owner: \"u\", name: \"mine\") { id } }",
            "{ search(query: \"secret\", type: REPOSITORY, first: 1) { repositoryCount } }",
            "{ node(id: \"R_1\") { id } }",
            "query Q($o: String!) { repository(owner: $o, name: \"mine\") { id } }",
            "{ ...F } fragment F on Query { repository(owner: \"u\", name: \"mine\") { id } }",
        ] {
            let request = api_request("POST", "/graphql", json!({"query": query}));
            assert_eq!(decide(request).await.as_deref(), Some("token-1"), "{query}");
        }

        // A token that reaches an ungranted repository, or more than it listed, could take a query on from a granted
        // repository to anything it reaches, so GraphQL reads as the App; REST still uses the token.
        for (repo, pat) in [("u/four", "github_pat_2"), ("u/five", "github_pat_3")] {
            assert_eq!(
                decide(pr_view(repo)).await.as_deref(),
                Some("token-1"),
                "{repo}"
            );
            assert_eq!(last_minted(&fake), app_read);
            let rest = api_request(
                "GET",
                &format!("/repos/{repo}/pulls"),
                serde_json::Value::Null,
            );
            assert_eq!(decide(rest).await.as_deref(), Some(pat));
        }

        // REST reads as before.
        let rest = api_request("GET", "/repos/u/mine/pulls", serde_json::Value::Null);
        assert_eq!(decide(rest).await.as_deref(), Some("github_pat_0"));
        let mutation = api_request(
            "POST",
            "/graphql",
            json!({"query": "mutation { addStar(input: {starrableId: \"x\"}) { clientMutationId } }"}),
        );
        assert!(matches!(
            policy.decide(&session, &mutation).await.unwrap(),
            Decision::Deny { .. }
        ));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_session_with_no_app_repository_reads_public_data_as_the_app(db: sqlx::PgPool) {
        use serde_json::json;

        let (policy, fake, users) =
            reads_policy(db, Default::default(), &[(&["u/mine"], false)]).await;
        let u = &users[0];
        let public = json!({"repositories": ["site"], "permissions": {"metadata": "read"}});
        for session in [reading(&[("u/mine", Some(u))]), reading(&[])] {
            for request in [
                pr_view("x/y"),
                api_request("POST", "/graphql", json!({"query": "{ viewer { login } }"})),
                api_request("GET", "/users/x", serde_json::Value::Null),
                api_request("GET", "/repos/x/y", serde_json::Value::Null),
            ] {
                let decision = policy.decide(&session, &request).await.unwrap();
                assert_eq!(token(decision).as_deref(), Some("token-1"));
                assert_eq!(last_minted(&fake), public);
            }
        }
    }

    #[test]
    fn the_proposal_hint_names_what_exists_and_what_the_session_has() {
        let hint = proposal_hint("that's refused", &context(Default::default()));
        assert!(hint.contains("request_repository"));
        for tool in [
            "propose_issues",
            "propose_issue_updates",
            "propose_pull_request_updates",
            "propose_stack",
            "request_code_review",
        ] {
            assert!(hint.contains(tool), "{tool}");
        }
        assert!(!hint.contains("comments (reviews"), "{hint}");

        let hint = proposal_hint(
            "that's refused",
            &context(sloppod_contracts::SessionRole::SubSession { parent: "p".into() }),
        );
        assert!(!hint.contains("request_repository"), "{hint}");
        assert!(hint.contains("your report"), "{hint}");

        let hint = proposal_hint(
            "that's refused",
            &context(sloppod_contracts::SessionRole::Review),
        );
        for tool in ["propose_", "request_"] {
            assert!(!hint.contains(tool), "{hint}");
        }
    }

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
        let order = |path: &str, granted: &[&'static str]| {
            let named: Vec<String> = named_repo(path).into_iter().collect();
            read_token_order(&named, granted)
        };
        assert_eq!(order("/repos/o/b/pulls", &granted), ["o/b", "o/a", "p/c"]);
        assert_eq!(order("/repos/O/B", &granted), ["o/b", "o/a", "p/c"]);
        assert_eq!(order("/repos/x/y/issues", &granted), ["o/a", "o/b", "p/c"]);
        assert_eq!(order("/user", &granted), ["o/a", "o/b", "p/c"]);
        assert!(order("/user", &[]).is_empty());
        assert_eq!(
            read_token_order(&["x/y".into(), "p/c".into()], &granted),
            ["p/c", "o/a", "o/b"]
        );
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

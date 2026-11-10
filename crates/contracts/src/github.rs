//! Reading a GitHub repository: what a path names, and which paths hand over one file rather than the repository.
//!
//! Both places that keep an agent from fetching single files read this: the runtime's request policy refuses the
//! requests, and the in-pod command guard refuses the commands that would make them. A caller that sees whole URLs
//! strips the scheme and host first, so what comes here is a path.

use crate::repo_from_git_path;

/// A path's repository and what follows it, after `prefix` — `"/"` from the root, `"/repos/"` on the API:
/// `<owner>/<name>/…`. `None` when the path doesn't carry both, or either is empty.
pub fn repo_path<'a>(path: &'a str, prefix: &str) -> Option<(String, &'a str)> {
    let rest = path.strip_prefix(prefix)?;
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next()?;
    let name = repo_from_git_path(parts.next()?);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((format!("{owner}/{name}"), parts.next().unwrap_or_default()))
}

/// Where a path is from: the API, `api.github.com`, which names repositories under `/repos/`, or github.com itself,
/// which names them at the root. The two have their own grammar — `blob` is a repository name on one and a file view
/// on the other — so a path is read as the site it's from and no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Api,
    Web,
}

/// The repository a path reads one file from, if it does, read as the site it's from.
///
/// On the API: a file's contents or a listing of them (`/repos/<owner>/<name>/contents/…`), a README
/// (`/repos/<owner>/<name>/readme`), a git blob (`/repos/<owner>/<name>/git/blobs/…`). On github.com: the web view of
/// one file (`/<owner>/<name>/blob/…`) or its raw bytes (`/<owner>/<name>/raw/…`).
///
/// Everything else is `None`: the trees API, issues, pull requests, search, releases, the git paths a clone is made
/// of, and anything the site's own grammar doesn't read that way — a repository named `blob` or `raw` is read through
/// the API as a repository like any other.
pub fn single_file_read(site: Site, path: &str) -> Option<String> {
    match site {
        Site::Api => {
            let (repo, rest) = repo_path(path, "/repos/")?;
            let what = rest.split('/').next().unwrap_or_default();
            (matches!(what, "contents" | "readme")
                || rest == "git/blobs"
                || rest.starts_with("git/blobs/"))
            .then_some(repo)
        }
        Site::Web => {
            let (repo, rest) = repo_path(path, "/")?;
            matches!(rest.split('/').next()?, "blob" | "raw").then_some(repo)
        }
    }
}

/// What an agent that tried to read one file is told: clone the repository instead. One wording, said wherever it's
/// refused, so the agent hears the same thing from the request it made and from the command it ran.
pub fn clone_instead(repo: Option<&str>) -> String {
    let target = match repo {
        Some(repo) => {
            let name = repo.rsplit('/').next().unwrap_or(repo);
            format!("https://github.com/{repo} /workspace/tmp/{name}")
        }
        None => "https://github.com/<owner>/<repo> /workspace/tmp/<name>".to_owned(),
    };
    format!(
        "reading single files from GitHub is refused; clone the repository instead: `git clone --depth 1 {target}` \
         (add `--branch <ref>` for another ref), then read it locally."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_github_path() {
        assert_eq!(
            repo_path("/repos/o/r/pulls/1", "/repos/"),
            Some(("o/r".to_owned(), "pulls/1"))
        );
        assert_eq!(
            repo_path("/o/r.git/info/refs", "/"),
            Some(("o/r".to_owned(), "info/refs"))
        );
        assert_eq!(repo_path("/o/r", "/"), Some(("o/r".to_owned(), "")));
        assert_eq!(repo_path("/o", "/"), None);
        assert_eq!(repo_path("//o/r/x", "/"), None);
        assert_eq!(repo_path("/repos//r/x", "/repos/"), None);
        assert_eq!(repo_path("/search/code", "/repos/"), None);
    }

    #[test]
    fn refuses_the_paths_that_hand_over_one_file() {
        for (site, path, repo) in [
            (Site::Api, "/repos/o/r/contents", "o/r"),
            (Site::Api, "/repos/o/r/contents/src/main.rs", "o/r"),
            (Site::Api, "/repos/o/r/readme", "o/r"),
            (Site::Api, "/repos/o/r/readme/docs", "o/r"),
            (Site::Api, "/repos/o/r/git/blobs", "o/r"),
            (Site::Api, "/repos/o/r/git/blobs/8e2a1c", "o/r"),
            (Site::Web, "/o/r/blob/main/src/main.rs", "o/r"),
            (Site::Web, "/o/r/raw/main/src/main.rs", "o/r"),
        ] {
            assert_eq!(
                single_file_read(site, path).as_deref(),
                Some(repo),
                "{path}"
            );
        }
    }

    /// A repository named `blob` or `raw` is a repository: each site's own grammar decides, and neither reads a path
    /// with the other's (#clone-not-fetch, the review's finding).
    #[test]
    fn reads_the_sites_own_grammar_and_no_other() {
        for path in [
            "/repos/acme/blob",
            "/repos/acme/blob/issues",
            "/repos/acme/blob/git/trees/main",
            "/repos/acme/blob/releases",
            "/repos/acme/raw/pulls",
            "/repos/acme/raw/commits/main",
        ] {
            assert_eq!(single_file_read(Site::Api, path), None, "{path}");
        }
        // The API's own blob endpoint is still refused for such a repository, and the repository it names is the
        // right one: `acme/raw`, not `repos/acme`.
        assert_eq!(
            single_file_read(Site::Api, "/repos/acme/raw/git/blobs/8e2a1c").as_deref(),
            Some("acme/raw")
        );
        // And the API's grammar doesn't reach a web path, nor the web's an API path.
        assert_eq!(single_file_read(Site::Api, "/o/r/blob/main/x"), None);
        assert_eq!(single_file_read(Site::Web, "/repos/o/r/contents/x"), None);
        // A web view of a repository named `blob` is still a web view: the first two segments are the repository.
        assert_eq!(
            single_file_read(Site::Web, "/blob/raw/raw/main/x").as_deref(),
            Some("blob/raw")
        );
    }

    #[test]
    fn leaves_the_rest_of_github_alone() {
        for path in [
            // What a session reads repositories with on the API.
            "/repos/o/r",
            "/repos/o/r/git/trees/main",
            "/repos/o/r/git/trees/main?recursive=1",
            "/repos/o/r/git/commits/abc",
            "/repos/o/r/contentsomething",
            "/repos/o/r/git/blobsomething",
            "/repos/o/r/issues",
            "/repos/o/r/pulls/1/reviews",
            "/repos/o/r/releases",
            "/repos/o/r/actions/runs",
            "/repos/o/r/commits/main",
            "/search/code",
            "/search/repositories",
            "/user",
            "/users/octo",
            "/graphql",
            "/markdown",
        ] {
            assert_eq!(single_file_read(Site::Api, path), None, "{path}");
        }
        for path in [
            // The git paths a clone is made of, and github.com's other views.
            "/o/r.git/info/refs",
            "/o/r/git-upload-pack",
            "/o/r/git-receive-pack",
            "/o/r",
            "/o/r/tree/main",
            "/o/r/archive/main.tar.gz",
            "/o/r/releases/download/v1/x.tar.gz",
            "/o/r/pulls",
        ] {
            assert_eq!(single_file_read(Site::Web, path), None, "{path}");
        }
    }

    #[test]
    fn the_hint_names_the_clone_to_make() {
        assert_eq!(
            clone_instead(Some("futo/kupra")),
            "reading single files from GitHub is refused; clone the repository instead: `git clone --depth 1 \
             https://github.com/futo/kupra /workspace/tmp/kupra` (add `--branch <ref>` for another ref), then read \
             it locally."
        );
        assert!(
            clone_instead(None).contains("https://github.com/<owner>/<repo> /workspace/tmp/<name>")
        );
    }
}

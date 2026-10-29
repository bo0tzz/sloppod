//! What sloppod knows of each built-in capability, in one place (ADR 2): whether the owner can grant it mid-session,
//! what a new session starts with, what a sub-session keeps, and which credential it needs. The owner's
//! own capabilities are definitions (`definitions.rs`); this is the built-ins' side of the same picture, and a
//! built-in that's an MCP server is served like one of those.

use sloppod_contracts::capability;

use crate::credentials::CHATGPT;

/// Where a capability stands in a new session's starting set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Every session has it; it isn't chosen.
    Always,
    /// Offered, and on unless left out.
    Offered,
    /// Not in a starting set as such: it comes another way (a repository chosen, the owner's memini).
    Not,
}

#[derive(Debug)]
pub struct Builtin {
    pub name: &'static str,
    /// The owner can grant it from a session's grants list. Repositories have their own endpoint; memory needs a key
    /// minted when the pod is placed.
    pub grantable: bool,
    pub start: Start,
    /// A sub-session keeps it whatever it's narrowed to: its agent can't run without it.
    pub always_inherited: bool,
    /// A credential kind the owner needs before it's offered at all.
    pub needs: Option<&'static str>,
    /// What it's made of (ADR 2, 2026-10-29). The core reads a session's reach from its grants' parts, not from
    /// which capability they belong to.
    pub parts: &'static [Part],
}

/// One kind of part a capability is made of. The behaviour behind each stays code; which capability is made of
/// which is data.
#[derive(Debug, PartialEq, Eq)]
pub enum Part {
    /// An MCP server, served by the runtime at `/servers/<name>` to the sessions granted it, with no credential.
    Mcp(&'static str),
    /// Inference for the session's agent, with the owner's credential for its provider.
    Model,
    /// GitHub through tokens scoped to the session's repository grants, with push rules and proposals.
    Github,
    /// A memini key minted at placement, the runtime's proxy, and the agent's plugin.
    Memory,
    /// The deployment's package hosts, reached directly, uninspected.
    Egress,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: capability::MODEL,
        grantable: true,
        start: Start::Always,
        always_inherited: true,
        needs: None,
        parts: &[Part::Model],
    },
    Builtin {
        name: capability::DOCS,
        grantable: true,
        start: Start::Always,
        always_inherited: false,
        needs: None,
        // Context7's hosted server (#195), used without an API key, so it's rate limited.
        parts: &[Part::Mcp("https://mcp.context7.com/mcp")],
    },
    Builtin {
        name: capability::GITHUB,
        grantable: true,
        start: Start::Offered,
        always_inherited: false,
        needs: None,
        parts: &[Part::Github],
    },
    Builtin {
        name: capability::PACKAGES,
        grantable: true,
        start: Start::Offered,
        always_inherited: false,
        needs: None,
        parts: &[Part::Egress],
    },
    Builtin {
        name: capability::CODEX,
        grantable: true,
        start: Start::Offered,
        always_inherited: true,
        needs: Some(CHATGPT),
        parts: &[Part::Model],
    },
    Builtin {
        name: capability::REPO,
        grantable: false,
        start: Start::Not,
        always_inherited: false,
        needs: None,
        parts: &[Part::Github],
    },
    Builtin {
        name: capability::MEMORY,
        grantable: false,
        start: Start::Not,
        always_inherited: false,
        needs: None,
        parts: &[Part::Memory],
    },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Names no definition of the owner's can take: the built-ins', and the agent's own MCP servers'.
pub fn reserved(name: &str) -> bool {
    builtin(name).is_some() || matches!(name, "sloppod" | "memini")
}

/// The URL of the MCP server a built-in is, if it's one.
pub fn mcp_url(name: &str) -> Option<&'static str> {
    builtin(name)?.parts.iter().find_map(|part| match part {
        Part::Mcp(url) => Some(*url),
        _ => None,
    })
}

/// Whether a capability has a part of this kind.
pub fn has_part(name: &str, kind: &Part) -> bool {
    builtin(name).is_some_and(|b| b.parts.contains(kind))
}

/// Whether a session keeps a capability of its parent's, whatever it's narrowed to.
pub fn always_inherited(name: &str) -> bool {
    builtin(name).is_some_and(|b| b.always_inherited)
}

/// The built-ins every new session has.
pub fn always() -> impl Iterator<Item = &'static str> {
    BUILTINS
        .iter()
        .filter(|b| b.start == Start::Always)
        .map(|b| b.name)
}

/// The built-ins offered in a starting set, given which credential kinds the owner has.
pub fn offered(has: impl Fn(&str) -> bool) -> Vec<&'static str> {
    BUILTINS
        .iter()
        .filter(|b| b.start == Start::Offered && b.needs.is_none_or(&has))
        .map(|b| b.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_ins_say_where_they_stand() {
        // What each is made of: only packages reaches hosts directly, and docs is an MCP server.
        assert!(has_part("packages", &Part::Egress) && !has_part("github", &Part::Egress));
        assert!(has_part("model", &Part::Model) && has_part("codex", &Part::Model));
        assert!(has_part("repo", &Part::Github) && has_part("memory", &Part::Memory));
        assert_eq!(mcp_url("docs"), Some("https://mcp.context7.com/mcp"));
        assert_eq!(mcp_url("github"), None);
        assert_eq!(always().collect::<Vec<_>>(), ["model", "docs"]);
        assert_eq!(offered(|_| false), ["github", "packages"]);
        assert_eq!(
            offered(|kind| kind == CHATGPT),
            ["github", "packages", "codex"]
        );
        assert!(builtin("packages").unwrap().grantable);
        assert!(!builtin("repo").unwrap().grantable && !builtin("memory").unwrap().grantable);
        assert!(
            always_inherited("model") && always_inherited("codex") && !always_inherited("github")
        );
        assert_eq!(mcp_url("docs"), Some("https://mcp.context7.com/mcp"));
        assert!(mcp_url("github").is_none() && mcp_url("victorialogs").is_none());
        assert!(
            reserved("docs")
                && reserved("sloppod")
                && reserved("memini")
                && !reserved("victorialogs")
        );
    }
}

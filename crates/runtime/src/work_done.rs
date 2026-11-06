//! The issues and pull requests a session has acted on (#205), built from its carried-out proposals and their results,
//! with the state the runtime last saw of each pull request (`pull_watches`). Nothing of it is stored.

use std::collections::HashMap;

use serde_json::Value;
use sloppod_protocol::api::{WorkDone, WorkState};

use crate::db::ProposalRow;
use crate::pulls::pull_url;

#[derive(Default)]
struct Entry {
    pull_request: bool,
    title: Option<String>,
    did: Vec<String>,
    at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Newest first, one entry per item, by the last time anything was done to it.
pub fn work_done(proposals: &[ProposalRow], watches: &[(String, i64, Value)]) -> Vec<WorkDone> {
    let mut items: HashMap<(String, u64), Entry> = HashMap::new();
    let mut proposals: Vec<&ProposalRow> = proposals
        .iter()
        .filter(|p| p.status == sloppod_protocol::api::ProposalStatus::Executed)
        .collect();
    proposals.sort_by_key(|p| p.decided_at.unwrap_or(p.created_at));
    for proposal in proposals {
        let at = proposal.decided_at.unwrap_or(proposal.created_at);
        let Some(result) = &proposal.result else {
            continue;
        };
        let repo = proposal.payload["repo"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        let mut note = |number: u64, pull_request: bool, title: Option<&str>, did: &str| {
            let entry = items.entry((repo.clone(), number)).or_default();
            entry.pull_request |= pull_request;
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                entry.title = Some(title.to_owned());
            }
            for what in did.split(", ").filter(|w| !w.is_empty()) {
                if !entry.did.iter().any(|d| d == what) {
                    entry.did.push(what.to_owned());
                }
            }
            entry.at = Some(at);
        };
        match proposal.kind.as_str() {
            "github.pr.create" => {
                if let Some(number) = result["number"].as_u64() {
                    note(number, true, proposal.payload["title"].as_str(), "opened");
                }
            }
            "github.stack.create" => {
                let layers = proposal.payload["pull_requests"].as_array();
                for (i, pull) in result["pull_requests"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let title = layers
                        .and_then(|l| l.get(i))
                        .and_then(|l| l["title"].as_str());
                    if let Some(number) = pull["number"].as_u64() {
                        note(number, true, title, "opened");
                    }
                }
            }
            "github.issue.create" => {
                for issue in result["issues"].as_array().into_iter().flatten() {
                    if let Some(number) = issue["number"].as_u64() {
                        note(number, false, issue["title"].as_str(), "filed");
                    }
                }
            }
            kind @ ("github.issue.update" | "github.pr.update") => {
                let updates = proposal.payload["updates"].as_array();
                for done in result["updated"].as_array().into_iter().flatten() {
                    let Some(number) = done["number"].as_u64() else {
                        continue;
                    };
                    // The update that was made, by its place in the payload. A result from before it said so can be
                    // matched by number only when one update is for the item: with more, which of them went through
                    // isn't known, and no title is better than one that was never applied.
                    let of_item = || {
                        updates
                            .into_iter()
                            .flatten()
                            .filter(|u| u["number"].as_u64() == Some(number))
                    };
                    let made = match done["index"].as_u64() {
                        Some(index) => updates.and_then(|u| u.get(index as usize)),
                        None if of_item().count() == 1 => of_item().next(),
                        None => None,
                    };
                    let title = made.and_then(|u| u["title"].as_str());
                    note(
                        number,
                        kind == "github.pr.update",
                        title,
                        done["did"].as_str().unwrap_or_default(),
                    );
                }
            }
            _ => {}
        }
    }
    let seen: HashMap<(String, u64), &Value> = watches
        .iter()
        .map(|(repo, number, observed)| ((repo.to_lowercase(), *number as u64), observed))
        .collect();
    let mut done: Vec<(chrono::DateTime<chrono::Utc>, WorkDone)> = items
        .into_iter()
        .filter_map(|((repo, number), entry)| {
            let at = entry.at?;
            let state = entry
                .pull_request
                .then(|| seen.get(&(repo.clone(), number)).and_then(|o| state_of(o)))
                .flatten();
            let url = if entry.pull_request {
                pull_url(&repo, number)
            } else {
                format!("https://github.com/{repo}/issues/{number}")
            };
            Some((
                at,
                WorkDone {
                    kind: if entry.pull_request {
                        "pull_request"
                    } else {
                        "issue"
                    }
                    .to_owned(),
                    repo,
                    number,
                    title: entry.title,
                    url,
                    did: entry.did,
                    at: at.to_rfc3339(),
                    state,
                },
            ))
        })
        .collect();
    done.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| (&a.1.repo, a.1.number).cmp(&(&b.1.repo, b.1.number)))
    });
    done.into_iter().map(|(_, work)| work).collect()
}

fn state_of(observed: &Value) -> Option<WorkState> {
    let state = observed["state"].as_str()?.to_lowercase();
    let checks = observed["checks"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(str::to_lowercase);
    let failing = observed["failing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_owned))
        .collect();
    Some(WorkState {
        state,
        checks,
        failing,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use sloppod_protocol::api::ProposalStatus;

    fn proposal(kind: &str, minutes_ago: i64, payload: Value, result: Value) -> ProposalRow {
        let at = Utc::now() - Duration::minutes(minutes_ago);
        ProposalRow {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            kind: kind.to_owned(),
            status: ProposalStatus::Executed,
            payload,
            result: Some(result),
            error: None,
            reason: None,
            created_at: at,
            decided_at: Some(at),
            decided_by: None,
            review_session_id: None,
        }
    }

    #[test]
    fn what_the_proposals_did_newest_first_with_where_a_pull_request_stands() {
        let proposals = [
            proposal(
                "github.pr.create",
                30,
                json!({"repo": "Bo0tzz/Sloppod", "title": "Fold the sidebar"}),
                json!({"url": "https://github.com/bo0tzz/sloppod/pull/9", "number": 9}),
            ),
            proposal(
                "github.issue.create",
                20,
                json!({"repo": "bo0tzz/sloppod"}),
                json!({"issues": [{"title": "Sidebar is slow", "url": "u", "number": 12}], "failed": []}),
            ),
            proposal(
                "github.pr.update",
                10,
                json!({"repo": "bo0tzz/sloppod", "updates": [{"number": 9, "comment": "Rebased"}]}),
                json!({"updated": [{"number": 9, "did": "commented, closed"}], "failed": []}),
            ),
            proposal(
                "github.issue.update",
                5,
                json!({"repo": "bo0tzz/sloppod", "updates": [{"number": 40, "close": "completed", "title": "Renamed"}]}),
                json!({"updated": [{"number": 40, "did": "closed, retitled"}], "failed": []}),
            ),
        ];
        let watches = [(
            "bo0tzz/sloppod".to_owned(),
            9,
            json!({"state": "OPEN", "checks": "FAILURE", "failing": ["check"]}),
        )];
        let done = work_done(&proposals, &watches);
        let seen: Vec<_> = done
            .iter()
            .map(|w| {
                (
                    w.kind.as_str(),
                    w.number,
                    w.title.as_deref(),
                    w.did.join("|"),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("issue", 40, Some("Renamed"), "closed|retitled".to_owned()),
                (
                    "pull_request",
                    9,
                    Some("Fold the sidebar"),
                    "opened|commented|closed".to_owned()
                ),
                ("issue", 12, Some("Sidebar is slow"), "filed".to_owned()),
            ]
        );
        assert_eq!(done[1].repo, "bo0tzz/sloppod");
        assert_eq!(done[1].url, "https://github.com/bo0tzz/sloppod/pull/9");
        assert_eq!(done[0].url, "https://github.com/bo0tzz/sloppod/issues/40");
        let state = done[1].state.as_ref().unwrap();
        assert_eq!(
            (
                state.state.as_str(),
                state.checks.as_deref(),
                state.failing.as_slice()
            ),
            ("open", Some("failure"), ["check".to_owned()].as_slice())
        );
        assert!(done[0].state.is_none() && done[2].state.is_none());
    }

    #[test]
    fn the_title_is_the_one_an_update_that_went_through_gave() {
        let payload = json!({"repo": "o/r", "updates": [
            {"number": 9, "title": "First"},
            {"number": 9, "comment": "Done"},
            {"number": 9, "title": "Last"},
            {"number": 4, "title": "Never applied"},
            {"number": 4, "comment": "Applied"},
        ]});
        // The first retitle of #4 failed, so only its comment is in the result.
        let result = json!({"updated": [
            {"number": 9, "did": "retitled", "index": 0},
            {"number": 9, "did": "commented", "index": 1},
            {"number": 9, "did": "retitled", "index": 2},
            {"number": 4, "did": "commented", "index": 4},
        ], "failed": [{"number": 4, "error": "no"}]});
        let done = work_done(&[proposal("github.issue.update", 1, payload, result)], &[]);
        let titles: Vec<_> = done
            .iter()
            .map(|w| (w.number, w.title.as_deref()))
            .collect();
        assert_eq!(titles, [(4, None), (9, Some("Last"))]);
    }

    #[test]
    fn a_result_without_places_gives_a_title_only_when_one_update_was_for_the_item() {
        let payload = json!({"repo": "o/r", "updates": [
            {"number": 9, "title": "First"},
            {"number": 9, "title": "Last"},
            {"number": 4, "title": "Never applied"},
            {"number": 4, "comment": "Applied"},
            {"number": 2, "title": "Only"},
        ]});
        let result = json!({"updated": [
            {"number": 9, "did": "retitled"},
            {"number": 4, "did": "commented"},
            {"number": 2, "did": "retitled"},
        ], "failed": []});
        let done = work_done(&[proposal("github.issue.update", 1, payload, result)], &[]);
        let titles: Vec<_> = done
            .iter()
            .map(|w| (w.number, w.title.as_deref()))
            .collect();
        assert_eq!(titles, [(2, Some("Only")), (4, None), (9, None)]);
    }

    #[test]
    fn a_stack_lists_each_pull_request_and_what_isnt_executed_is_left_out() {
        let mut failed = proposal(
            "github.pr.create",
            3,
            json!({"repo": "o/r", "title": "x"}),
            json!({"number": 1}),
        );
        failed.status = ProposalStatus::Failed;
        let stack = proposal(
            "github.stack.create",
            2,
            json!({"repo": "o/r", "pull_requests": [{"title": "Base"}, {"title": "Top"}]}),
            json!({"pull_requests": [{"number": 5}, {"number": 6}]}),
        );
        let done = work_done(&[failed, stack], &[]);
        let seen: Vec<_> = done
            .iter()
            .map(|w| (w.number, w.title.as_deref()))
            .collect();
        assert_eq!(seen, [(5, Some("Base")), (6, Some("Top"))]);
    }
}

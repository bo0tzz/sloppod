//! What updating an existing issue and updating an existing pull request have in common (ADR 3): checking a list of
//! updates, the fields both have, what a reviewer is shown, and the part of making one that GitHub treats alike, since
//! a pull request is an issue to its comments and labels.

use serde_json::{Map, Value, json};
use sloppod_contracts::{Error, Result};

use crate::github::GitHubApp;

pub const MAX_UPDATES: usize = 50;

pub fn labels(update: &Value, field: &str, i: usize) -> Result<Vec<String>> {
    match &update[field] {
        Value::Null => Ok(Vec::new()),
        Value::Array(labels) => labels
            .iter()
            .map(|l| {
                l.as_str()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Option<_>>()
            .ok_or_else(|| Error::new(format!("update {}'s {field} must be label names", i + 1))),
        _ => Err(Error::new(format!(
            "update {}'s {field} must be a list",
            i + 1
        ))),
    }
}

/// What an update can have. Anything else is refused rather than dropped, so a proposal shows all it will do.
pub struct Fields {
    pub all: &'static [&'static str],
    /// Of those, the ones that are text.
    pub text: &'static [&'static str],
    /// And the ones that are true or false.
    pub flags: &'static [&'static str],
}

/// Refuses an update with a field it can't have, or one of the wrong type, before its fields are read.
fn check_fields(update: &Value, i: usize, fields: &Fields) -> Result<()> {
    let Some(given) = update.as_object() else {
        return Err(Error::new(format!("update {} must be an object", i + 1)));
    };
    if let Some(unknown) = given.keys().find(|k| !fields.all.contains(&k.as_str())) {
        return Err(Error::new(format!(
            "update {} has `{unknown}`, which an update can't have; it can have {}",
            i + 1,
            fields.all.join(", ")
        )));
    }
    for field in fields.text {
        if !matches!(
            given.get(*field),
            None | Some(Value::Null | Value::String(_))
        ) {
            return Err(Error::new(format!(
                "update {}'s {field} must be text",
                i + 1
            )));
        }
    }
    for field in fields.flags {
        if !matches!(given.get(*field), None | Some(Value::Null | Value::Bool(_))) {
            return Err(Error::new(format!(
                "update {}'s {field} must be true or false",
                i + 1
            )));
        }
    }
    Ok(())
}

/// Checks a list of updates from the agent or the reviewer, keeping only what GitHub gets. Each names an `item` (an
/// issue, a pull request) and changes at least one thing. `own` reads and checks the fields only that kind has.
pub fn update_list(
    updates: &Value,
    item: &str,
    fields: &Fields,
    own: impl Fn(&Value, usize, u64, &mut Map<String, Value>) -> Result<()>,
) -> Result<Vec<Value>> {
    let Some(updates) = updates.as_array().filter(|u| !u.is_empty()) else {
        return Err(Error::new("give at least one update"));
    };
    if updates.len() > MAX_UPDATES {
        return Err(Error::new(format!(
            "propose at most {MAX_UPDATES} updates at a time"
        )));
    }
    updates
        .iter()
        .enumerate()
        .map(|(i, update)| {
            check_fields(update, i, fields)?;
            let number = update["number"]
                .as_u64()
                .ok_or_else(|| Error::new(format!("update {} needs the {item}'s number", i + 1)))?;
            let mut checked = Map::new();
            checked.insert("number".into(), json!(number));
            for field in ["title", "comment"] {
                if let Some(text) = update[field].as_str().filter(|t| !t.trim().is_empty()) {
                    checked.insert(field.into(), json!(text.trim()));
                }
            }
            // A body replaces the old one, so an empty one is kept: it clears the body.
            if let Some(body) = update["body"].as_str() {
                checked.insert("body".into(), json!(body.trim()));
            }
            own(update, i, number, &mut checked)?;
            for field in ["add_labels", "remove_labels"] {
                let names = labels(update, field, i)?;
                if !names.is_empty() {
                    checked.insert(field.into(), json!(names));
                }
            }
            if checked.len() == 1 {
                return Err(Error::new(format!(
                    "update {} doesn't change #{number}",
                    i + 1
                )));
            }
            Ok(Value::Object(checked))
        })
        .collect()
}

/// The reviewer sends the updates to make, so dropping one leaves it out.
pub fn edit_updates(
    payload: &Value,
    edits: &Value,
    list: impl Fn(&Value) -> Result<Vec<Value>>,
) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(updates) = edits.get("updates") {
        payload["updates"] = json!(list(updates)?);
    }
    Ok(payload)
}

/// The items whose title or body the updates replace, each once, for the reviewer to compare with what they are now.
pub fn rewritten(payload: &Value) -> Vec<u64> {
    let mut numbers: Vec<u64> = payload["updates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|u| u.get("title").is_some() || u.get("body").is_some())
        .filter_map(|u| u["number"].as_u64())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// What an update changes, in a few words, for the result and the agent's notice; `state` is what only its kind
/// changes (closing, reopening), which comes after the comment.
pub fn describe(update: &Value, state: Vec<String>) -> String {
    let mut parts = Vec::new();
    if update.get("comment").is_some() {
        parts.push("commented".to_owned());
    }
    parts.extend(state);
    if update.get("title").is_some() {
        parts.push("retitled".to_owned());
    }
    match update["body"].as_str() {
        Some("") => parts.push("body cleared".to_owned()),
        Some(_) => parts.push("body replaced".to_owned()),
        None => {}
    }
    if update.get("add_labels").is_some() || update.get("remove_labels").is_some() {
        parts.push("relabelled".to_owned());
    }
    parts.join(", ")
}

/// The part of an update GitHub treats alike for an issue and a pull request: the comment, then the labels. Carrying
/// out an approved proposal again (after a restart) repeats only what's harmless to repeat: the comment carries
/// `marker`, the update's own, and isn't posted again if one with it is there, whoever else said the same (#346).
pub async fn comment_and_label(
    app: &GitHubApp,
    repo: &str,
    number: u64,
    update: &Value,
    marker: &str,
) -> anyhow::Result<()> {
    if let Some(comment) = update["comment"].as_str() {
        let comments = app.issue_comments(repo, number).await?;
        if !comments.iter().any(|c| c.contains(marker)) {
            app.comment_on_issue(repo, number, &super::marked(comment, marker))
                .await?;
        }
    }
    let added: Vec<&str> = update["add_labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !added.is_empty() {
        // Per update, after its comment: a label GitHub won't create fails only this update.
        app.ensure_labels(repo, &added).await?;
        app.add_labels(repo, number, &added).await?;
    }
    for label in update["remove_labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        app.remove_label(repo, number, label).await?;
    }
    Ok(())
}

/// Carries out `apply` for each update, given its position, and answers `{updated, failed, message}`; an error only when none went
/// through. `item` is the plural, for the message.
pub async fn execute_all<F, Fut>(
    updates: &[Value],
    items: &str,
    describe: impl Fn(&Value) -> String,
    apply: F,
) -> Result<Value>
where
    F: Fn(usize, u64, Value) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for (item, update) in updates.iter().enumerate() {
        let number = update["number"].as_u64().unwrap_or_default();
        match apply(item, number, update.clone()).await {
            Ok(()) => done.push(json!({"number": number, "did": describe(update)})),
            Err(e) => failed.push(json!({"number": number, "error": format!("{e:#}")})),
        }
    }
    if done.is_empty() {
        return Err(Error::new(format!(
            "no {items} were updated: {}",
            failed[0]["error"].as_str().unwrap_or_default()
        )));
    }
    let mut message = format!(
        "Updated {}.",
        done.iter()
            .map(|u| format!(
                "#{} ({})",
                u["number"],
                u["did"].as_str().unwrap_or_default()
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for failure in &failed {
        message.push_str(&format!(
            " #{} failed: {}.",
            failure["number"],
            failure["error"].as_str().unwrap_or_default()
        ));
    }
    Ok(json!({"updated": done, "failed": failed, "message": message}))
}

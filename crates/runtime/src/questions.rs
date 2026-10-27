//! Questions an agent asks the user without waiting for them (ADR 4): shown like Claude Code's AskUserQuestion, as
//! an ACP form elicitation, and answered with a notice.

use serde_json::{Value, json};

/// The form elicitation that shows `questions` (`{question, header?, options: {label, description?}[],
/// multi_select?}[]`), the same shape claude-agent-acp gives AskUserQuestion, so UIs show both alike.
pub fn elicitation(questions: &Value, session_id: &str) -> Result<Value, String> {
    let questions = questions
        .as_array()
        .filter(|q| (1..=4).contains(&q.len()))
        .ok_or("give one to four questions")?;
    let single = questions.len() == 1;
    let mut properties = serde_json::Map::new();
    for (index, question) in questions.iter().enumerate() {
        let text = question["question"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("every question needs its text")?;
        let options: Vec<Value> = question["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| {
                let label = option["label"].as_str()?.trim();
                let mut choice = json!({"const": label, "title": label});
                if let Some(description) = option["description"].as_str() {
                    choice["description"] = json!(description);
                }
                (!label.is_empty()).then_some(choice)
            })
            .collect();
        if options.len() < 2 {
            return Err("every question needs at least two options".into());
        }
        let title = question["header"].as_str().unwrap_or(text);
        let description = (!single).then_some(text);
        let multi = question["multi_select"].as_bool() == Some(true);
        properties.insert(
            format!("question_{index}"),
            if multi {
                json!({"type": "array", "title": title, "description": description, "items": {"anyOf": options}})
            } else {
                json!({"type": "string", "title": title, "description": description, "oneOf": options})
            },
        );
        properties.insert(
            format!("question_{index}_custom"),
            json!({
                "type": "string",
                "title": "Other",
                "description": "Type your own answer, or add a note to the option you chose above (optional).",
            }),
        );
    }
    Ok(json!({
        "mode": "form",
        "sessionId": session_id,
        "message": if single { questions[0]["question"].clone() } else { json!("Please answer the following questions.") },
        "requestedSchema": {"type": "object", "properties": properties},
    }))
}

/// The notice that tells the agent how the user answered its questions.
pub fn answer_text(questions: &Value, response: &Value) -> String {
    if response["action"] != "accept" {
        return "sloppod: the user dismissed your question without answering it.".into();
    }
    let content = &response["content"];
    let answers: Vec<String> = questions
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, question)| {
            let picked = match &content[format!("question_{index}")] {
                Value::Array(picks) => picks
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::String(pick) => pick.clone(),
                _ => String::new(),
            };
            let custom = content[format!("question_{index}_custom")]
                .as_str()
                .map(str::trim)
                .unwrap_or_default();
            let answer = match (picked.is_empty(), custom.is_empty()) {
                (true, true) => "(no answer)".to_owned(),
                (false, true) => picked,
                (true, false) => custom.to_owned(),
                (false, false) => format!("{picked} (note: {custom})"),
            };
            format!(
                "- {}: {answer}",
                question["question"].as_str().unwrap_or_default()
            )
        })
        .collect();
    format!(
        "sloppod: the user answered your question:\n{}",
        answers.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> Value {
        json!([
            {"question": "Which backoff?", "header": "Backoff", "options": [{"label": "Exponential"}, {"label": "Fixed"}]},
            {"question": "Which tests?", "options": [{"label": "Unit"}, {"label": "End-to-end"}], "multi_select": true},
        ])
    }

    #[test]
    fn shows_questions_as_a_form() {
        let form = elicitation(&questions(), "s").unwrap();
        let properties = &form["requestedSchema"]["properties"];
        assert_eq!(properties["question_0"]["title"], "Backoff");
        assert_eq!(properties["question_0"]["oneOf"][1]["const"], "Fixed");
        assert_eq!(
            properties["question_1"]["items"]["anyOf"][0]["const"],
            "Unit"
        );
        assert_eq!(properties["question_1_custom"]["title"], "Other");
        assert!(elicitation(&json!([]), "s").is_err());
        assert!(
            elicitation(
                &json!([{"question": "Q?", "options": [{"label": "A"}]}]),
                "s"
            )
            .is_err()
        );
    }

    #[test]
    fn writes_answers_for_the_agent() {
        let response = json!({"action": "accept", "content": {
            "question_0": "Exponential", "question_0_custom": "capped at 10 s", "question_1": ["Unit", "End-to-end"],
        }});
        assert_eq!(
            answer_text(&questions(), &response),
            "sloppod: the user answered your question:\n- Which backoff?: Exponential (note: capped at 10 s)\n- \
             Which tests?: Unit, End-to-end"
        );
        assert!(answer_text(&questions(), &json!({"action": "cancel"})).contains("dismissed"));
    }
}

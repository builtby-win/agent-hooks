//! Manual test decider: subscribe with `decide:true`, print every held gate
//! event, read a verdict from stdin, send `prompt.respond`.
//!
//! Usage: `agent-hooks answer --socket /tmp/agent-hooks.sock`
//! Non-interactive: `agent-hooks answer --auto allow|deny[:reason]|ask[:question]|answer:<option-or-text>`.

use agent_hooks_client::AgentHooksClient;
use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, PromptAnswer, PromptDecision, PromptQuestion,
};
use futures::StreamExt;
use tokio::io::{self, AsyncBufReadExt};

/// Gate-capability lifecycles the daemon holds open for a decider verdict.
/// Mirrors the manifests (`capability: gate`): everything else is observe and
/// needs no answer.
fn is_gate(lifecycle: &AgentLifecycle) -> bool {
    matches!(
        lifecycle,
        AgentLifecycle::PreToolUse { .. }
            | AgentLifecycle::PermissionRequest { .. }
            | AgentLifecycle::UserPromptSubmit { .. }
    )
}

/// Rendezvous id deciders answer with: broadcast `event_id`, except
/// `PermissionRequest` which keeps `prompt.prompt_id`. Must match
/// `daemon::hold_prompt_id` or the answer lands on `PromptNotFound`.
fn hold_prompt_id(event: &AgentEvent) -> String {
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => prompt.prompt_id.clone(),
        _ => event.event_id.clone(),
    }
}

fn summarize(event: &AgentEvent) -> String {
    match &event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            let short: String = raw.chars().take(300).collect();
            format!("tool={tool_name} input={short}")
        }
        AgentLifecycle::PermissionRequest { prompt } => {
            let mut parts = vec![format!("kind={:?}", prompt.kind)];
            if let Some(t) = &prompt.tool_name {
                parts.push(format!("tool={t}"));
            }
            if let Some(c) = &prompt.command {
                parts.push(format!("cmd={c}"));
            }
            if let Some(d) = &prompt.description {
                let short: String = d.chars().take(200).collect();
                parts.push(format!("desc={short}"));
            }
            for q in &prompt.questions {
                let opts: Vec<_> = q.options.iter().map(|o| o.label.as_str()).collect();
                parts.push(format!("q[{}]: {} ({})", q.id, q.question, opts.join("|")));
            }
            parts.join(" ")
        }
        AgentLifecycle::UserPromptSubmit { .. } => "user-prompt".to_string(),
        other => format!("{other:?}"),
    }
}

fn parse_auto(spec: &str) -> Option<PromptDecision> {
    let (verb, rest) = match spec.split_once(':') {
        Some((v, r)) => (v, Some(r)),
        None => (spec, None),
    };
    match verb {
        "allow" => Some(PromptDecision::allow()),
        "deny" => Some(PromptDecision::deny(
            rest.filter(|s| !s.is_empty()).map(str::to_string),
        )),
        "ask" => Some(PromptDecision::ask(
            rest.filter(|s| !s.is_empty()).map(str::to_string),
        )),
        _ => None,
    }
}
/// `--auto answer:<text>`: allow, answering every question in the prompt with
/// `text`. An option whose label equals `text` (ASCII case-insensitive) is
/// selected; otherwise `text` goes in as free text.
fn answer_decision(text: &str, questions: &[PromptQuestion]) -> PromptDecision {
    let mut d = PromptDecision::allow();
    d.answers = questions
        .iter()
        .map(|q| {
            let picked = q
                .options
                .iter()
                .find(|o| o.label.eq_ignore_ascii_case(text))
                .map(|o| o.label.clone());
            PromptAnswer {
                question_id: q.id.clone(),
                custom_input: picked.is_none().then(|| text.to_string()),
                selected_options: picked.into_iter().collect(),
            }
        })
        .collect();
    d
}

fn verdict_word(d: &PromptDecision) -> &'static str {
    if d.defer {
        "defer"
    } else if d.ask {
        "ask"
    } else if d.allow {
        "allow"
    } else {
        "deny"
    }
}

fn print_backlog(backlog: &[agent_hooks_protocol::PendingPromptInfo]) {
    if backlog.is_empty() {
        println!("(backlog empty)");
        return;
    }
    for (n, p) in backlog.iter().enumerate() {
        let short_id: String = p.prompt_id.chars().take(8).collect();
        println!(
            "{n}: [{} {} key={} waiters={} parked={} age={}ms id={}] {}",
            p.session_id,
            short_id,
            p.key,
            p.waiters,
            p.parked,
            p.age_ms,
            p.prompt_id,
            p.question.as_deref().unwrap_or("(no text)"),
        );
    }
}

async fn read_verdict(
    lines: &mut io::Lines<io::BufReader<io::Stdin>>,
) -> Option<(Option<usize>, PromptDecision)> {
    eprint!("verdict [a]llow / [d]eny [text] / as[k] [text] / [l]ist / <n> <verdict> > ");
    let line = lines.next_line().await.ok()??;
    let line = line.trim();
    // `<n> <verdict...>` answers backlog item n instead of the live prompt.
    let (target, rest) = match line.split_once(char::is_whitespace) {
        Some((first, r)) if first.parse::<usize>().is_ok() => {
            (first.parse::<usize>().ok(), r.trim())
        }
        _ => (None, line),
    };
    let (verb, text) = match rest.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (rest, ""),
    };
    let text = (!text.is_empty()).then(|| text.to_string());
    let decision = match verb {
        "a" | "allow" => PromptDecision::allow(),
        "d" | "deny" => PromptDecision::deny(text),
        "k" | "ask" => PromptDecision::ask(text),
        "l" | "list" => return None,
        _ => {
            eprintln!("unknown verdict {verb:?} — use a, d, k, l, or <n> <verdict>");
            return None;
        }
    };
    Some((target, decision))
}

pub async fn run(socket: String, auto: Option<String>, line: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    // Fullscreen TUI when interactive; --auto and --line keep the plain pipe.
    if auto.is_none() && !line && std::io::stdout().is_terminal() {
        return super::answer_tui::run_tui(socket).await;
    }
    // `answer:<text>` is resolved per prompt (it needs the question ids).
    let auto_answer: Option<String> = auto
        .as_deref()
        .and_then(|s| s.strip_prefix("answer:"))
        .map(str::to_string);
    let auto_decision: Option<PromptDecision> = match auto.as_deref() {
        None => None,
        Some(_) if auto_answer.is_some() => Some(PromptDecision::allow()),
        Some(spec) => Some(parse_auto(spec).ok_or_else(|| {
            anyhow::anyhow!(
                "bad --auto {spec:?}: want allow|deny[:reason]|ask[:question]|answer:<text>"
            )
        })?),
    };

    let client = AgentHooksClient::connect_to(socket).await?;
    // Backlog first: questions held before this decider subscribed.
    let mut backlog = client.list_pending_prompts().await.unwrap_or_default();
    if !backlog.is_empty() {
        println!("{} question(s) waiting:", backlog.len());
        print_backlog(&backlog);
    }
    if let Some(d) = &auto_decision {
        for p in std::mem::take(&mut backlog) {
            let d = match (&auto_answer, p.prompt.as_ref()) {
                (Some(text), Some(req)) => answer_decision(text, &req.questions),
                _ => d.clone(),
            };
            match client.respond_prompt(&p.prompt_id, d).await {
                Ok(()) => println!("auto-answered {}", p.prompt_id),
                Err(e) => eprintln!("respond failed for {}: {e}", p.prompt_id),
            }
        }
    }
    let mut stream = client.subscribe_decider();
    eprintln!("decider subscribed (decide:true) — waiting for gate events…");

    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(event) = stream.next().await {
        let name = event.lifecycle.name();
        if !is_gate(&event.lifecycle) {
            println!(
                "({} {} {}) {}",
                event.harness,
                event.session_id,
                name,
                summarize(&event)
            );
            continue;
        }
        let prompt_id = hold_prompt_id(&event);
        println!(
            "[{} {} {} id={}] {}",
            event.harness,
            event.session_id,
            name,
            prompt_id,
            summarize(&event)
        );
        let (target, decision) = match &auto_decision {
            Some(d) => {
                let d = match (&auto_answer, &event.lifecycle) {
                    (Some(text), AgentLifecycle::PermissionRequest { prompt }) => {
                        answer_decision(text, &prompt.questions)
                    }
                    _ => d.clone(),
                };
                println!("auto → {}", verdict_word(&d));
                if !d.answers.is_empty() {
                    println!("auto answers → {}", serde_json::to_string(&d.answers)?);
                }
                (None, d)
            }
            None => match read_verdict(&mut lines).await {
                Some(t) => t,
                None => {
                    // `l` or unknown input: refresh and show the backlog.
                    backlog = client.list_pending_prompts().await.unwrap_or_default();
                    print_backlog(&backlog);
                    continue;
                }
            },
        };
        // `<n> <verdict>` answers backlog item n; otherwise the live prompt.
        let answer_id = match target {
            Some(n) => match backlog.get(n) {
                Some(p) => p.prompt_id.clone(),
                None => {
                    eprintln!("no backlog item {n}");
                    continue;
                }
            },
            None => prompt_id,
        };
        match client.respond_prompt(&answer_id, decision).await {
            Ok(()) => println!("answered {answer_id}"),
            Err(e) => eprintln!("respond failed for {answer_id}: {e}"),
        }
        backlog = client.list_pending_prompts().await.unwrap_or_default();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::PromptOption;

    fn question(id: &str, labels: &[&str]) -> PromptQuestion {
        PromptQuestion {
            id: id.into(),
            header: None,
            question: "pick".into(),
            options: labels
                .iter()
                .map(|l| PromptOption {
                    label: (*l).into(),
                    description: None,
                    preview: None,
                })
                .collect(),
            multiple: false,
            custom: true,
            recommended: None,
        }
    }

    #[test]
    fn answer_selects_matching_option_case_insensitively() {
        let d = answer_decision("blue", &[question("q1", &["Red", "Blue"])]);
        assert!(d.allow);
        assert_eq!(d.answers.len(), 1);
        assert_eq!(d.answers[0].selected_options, vec!["Blue".to_string()]);
        assert_eq!(d.answers[0].custom_input, None);
    }

    #[test]
    fn answer_falls_back_to_free_text() {
        let d = answer_decision("green", &[question("q1", &["Red"]), question("q2", &[])]);
        assert_eq!(d.answers.len(), 2);
        for a in &d.answers {
            assert!(a.selected_options.is_empty());
            assert_eq!(a.custom_input.as_deref(), Some("green"));
        }
    }
}

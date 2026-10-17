//! Fullscreen decider TUI: backlog picker → checkbox options + free text.
//!
//! Model: `prompt.list` polled at 1Hz (backlog appears within a second of the
//! hold). A background `subscribe_decider` drain holds `decide:true` so gates
//! actually wait — polling alone would fast-allow. Submit sends
//! `prompt.respond` with `answers[]`; deny sends `custom_input` reason.

use std::collections::HashSet;
use std::io::{stdout, Stdout, Write};
use std::time::{Duration, Instant};

use agent_hooks_client::AgentHooksClient;
use agent_hooks_protocol::{PendingPromptInfo, PromptAnswer, PromptDecision};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::Stylize,
    terminal,
};
use futures::StreamExt;

const HOLD_TIMEOUT: Duration = Duration::from_secs(300);
const REFRESH: Duration = Duration::from_secs(1);

struct Detail {
    prompt_id: String,
    session_id: String,
    key: String,
    title: String,
    questions: Vec<QState>,
    q_cursor: usize,
    opt_cursor: usize,
    focus: Focus,
    deny_reason: String,
    hint: Option<(String, Instant)>,
    externally_resolved: bool,
}

struct QState {
    id: String,
    header: Option<String>,
    text: String,
    options: Vec<OptState>,
    multiple: bool,
    custom: bool,
    picked: HashSet<usize>,
    free: String,
}

struct OptState {
    label: String,
    description: Option<String>,
    recommended: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Options,
    Free,
    Deny,
}

fn hint(detail: &mut Detail, msg: impl Into<String>) {
    detail.hint = Some((msg.into(), Instant::now()));
}

fn build_detail(p: &PendingPromptInfo) -> Detail {
    let mut questions = Vec::new();
    if let Some(prompt) = &p.prompt {
        for (qi, q) in prompt.questions.iter().enumerate() {
            let mut opts = Vec::new();
            for (oi, o) in q.options.iter().enumerate() {
                opts.push(OptState {
                    label: o.label.clone(),
                    description: o.description.clone(),
                    recommended: q.recommended == Some(oi),
                });
            }
            let mut picked = HashSet::new();
            if !q.multiple {
                if let Some(r) = q.recommended {
                    if r < opts.len() {
                        picked.insert(r);
                    }
                }
            }
            questions.push(QState {
                id: q.id.clone(),
                header: q.header.clone(),
                text: q.question.clone(),
                options: opts,
                multiple: q.multiple,
                custom: q.custom,
                picked,
                free: String::new(),
            });
            let _ = qi;
        }
    }
    Detail {
        prompt_id: p.prompt_id.clone(),
        session_id: p.session_id.clone(),
        key: p.key.clone(),
        title: p
            .question
            .clone()
            .unwrap_or_else(|| "(no text)".to_string()),
        questions,
        q_cursor: 0,
        opt_cursor: 0,
        focus: Focus::Options,
        deny_reason: String::new(),
        hint: None,
        externally_resolved: false,
    }
}

fn submit_answers(d: &Detail) -> Vec<PromptAnswer> {
    d.questions
        .iter()
        .map(|q| PromptAnswer {
            question_id: q.id.clone(),
            selected_options: {
                let mut v: Vec<usize> = q.picked.iter().copied().collect();
                v.sort_unstable();
                v.into_iter()
                    .filter_map(|i| q.options.get(i).map(|o| o.label.clone()))
                    .collect()
            },
            custom_input: (!q.free.trim().is_empty()).then(|| q.free.trim().to_string()),
        })
        .collect()
}

/// Every option-bearing question needs a pick or free text before allow.
fn ready_to_allow(d: &Detail) -> bool {
    d.questions
        .iter()
        .all(|q| q.options.is_empty() || !q.picked.is_empty() || !q.free.trim().is_empty())
}

fn age_line(fetched_age_ms: u64, fetched_at: Instant) -> String {
    let age = Duration::from_millis(fetched_age_ms) + fetched_at.elapsed();
    let left = HOLD_TIMEOUT.saturating_sub(age);
    format!(
        "age {}s · hold expires in {}s",
        age.as_secs(),
        left.as_secs()
    )
}

fn draw_list(
    out: &mut Stdout,
    backlog: &[(PendingPromptInfo, Instant)],
    cursor: usize,
) -> anyhow::Result<()> {
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    queue!(
        out,
        crossterm::style::Print(format!(
            "{} — {} waiting · decide:true\n",
            "agent-hooks answer".bold(),
            backlog.len()
        ))
    )?;
    if backlog.is_empty() {
        queue!(
            out,
            crossterm::style::Print(
                "(empty — new gate events appear here within ~1s)\n".dark_grey()
            )
        )?;
    }
    for (n, (p, fetched_at)) in backlog.iter().enumerate() {
        let marker = if n == cursor {
            ">".yellow()
        } else {
            " ".stylize()
        };
        let id8: String = p.prompt_id.chars().take(8).collect();
        queue!(
            out,
            crossterm::style::Print(format!(
                "{} [{}] {} {} key={} waiters={}{}\n    {}\n",
                marker,
                n,
                p.session_id,
                id8.dark_grey(),
                p.key,
                p.waiters,
                if p.parked > 0 {
                    format!(" +{} parked", p.parked)
                } else {
                    String::new()
                },
                p.question.as_deref().unwrap_or("(no text)"),
            ))
        )?;
        let _ = fetched_at;
    }
    queue!(
        out,
        crossterm::style::Print("\n↑↓/jk move · enter open · r refresh · q quit\n".dark_grey())
    )?;
    out.flush()?;
    Ok(())
}

fn draw_detail(
    out: &mut Stdout,
    d: &Detail,
    fetched_age_ms: u64,
    fetched_at: Instant,
) -> anyhow::Result<()> {
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    queue!(
        out,
        crossterm::style::Print(format!(
            "{} [{}] {}\n{}\n{}\n",
            "question".bold(),
            d.key,
            d.session_id.as_str().dark_grey(),
            age_line(fetched_age_ms, fetched_at).dark_grey(),
            d.title,
        ))
    )?;
    if d.externally_resolved {
        queue!(
            out,
            crossterm::style::Print("resolved elsewhere — esc back to list\n".yellow())
        )?;
    }
    if d.questions.is_empty() {
        queue!(
            out,
            crossterm::style::Print("\n(no sub-questions — plain tool approval)\n".dark_grey())
        )?;
    }
    for (qi, q) in d.questions.iter().enumerate() {
        let active = qi == d.q_cursor;
        let qmark = if active { ">".yellow() } else { " ".stylize() };
        queue!(
            out,
            crossterm::style::Print(format!(
                "\n{} Q{}: {} {}\n",
                qmark,
                qi + 1,
                q.header.as_deref().unwrap_or(""),
                q.text.as_str().bold(),
            ))
        )?;
        if q.options.is_empty() {
            queue!(
                out,
                crossterm::style::Print("    (free-text only)\n".dark_grey())
            )?;
        }
        for (oi, o) in q.options.iter().enumerate() {
            let picked = q.picked.contains(&oi);
            let boxmark = if q.multiple {
                if picked {
                    "[x]"
                } else {
                    "[ ]"
                }
            } else if picked {
                "(•)"
            } else {
                "( )"
            };
            let cursor_mark = if active && oi == d.opt_cursor && d.focus == Focus::Options {
                "▶"
            } else {
                " "
            };
            let rec = if o.recommended { " ★" } else { "" };
            let mut line = format!("  {cursor_mark} {boxmark} {}{rec}", o.label);
            if let Some(desc) = &o.description {
                line.push_str(&format!(" — {}", desc.chars().take(80).collect::<String>()));
            }
            if active && oi == d.opt_cursor && d.focus == Focus::Options {
                queue!(out, crossterm::style::Print(format!("{line}\n").reverse()))?;
            } else {
                queue!(out, crossterm::style::Print(format!("{line}\n")))?;
            }
        }
        if q.custom {
            let editing = active && d.focus == Focus::Free;
            let caret = if editing { "▌" } else { "" };
            let shown = if q.free.is_empty() && !editing {
                "(empty)".dark_grey().to_string()
            } else {
                format!("{}{}", q.free, caret)
            };
            if editing {
                queue!(
                    out,
                    crossterm::style::Print(format!("    note: {shown}\n").reverse())
                )?;
            } else {
                queue!(out, crossterm::style::Print(format!("    note: {shown}\n")))?;
            }
        }
    }
    if d.focus == Focus::Deny {
        queue!(
            out,
            crossterm::style::Print(format!("\ndeny reason: {}▌\n", d.deny_reason).reverse())
        )?;
    }
    if let Some((msg, at)) = &d.hint {
        if at.elapsed() < Duration::from_secs(4) {
            queue!(out, crossterm::style::Print(format!("\n{msg}\n").yellow()))?;
        }
    }
    let keys = match d.focus {
        Focus::Options => "↑↓ move · space toggle/pick · tab next question · e note · enter submit-allow · d deny · esc list",
        Focus::Free => "typing note · esc done · enter submit-allow",
        Focus::Deny => "typing deny reason · esc cancel · enter send deny",
    };
    queue!(
        out,
        crossterm::style::Print(format!("\n{keys}\n").dark_grey())
    )?;
    out.flush()?;
    Ok(())
}

fn backlog_age(backlog: &[(PendingPromptInfo, Instant)], idx: usize) -> (u64, Instant) {
    backlog
        .get(idx)
        .map(|(p, at)| (p.age_ms, *at))
        .unwrap_or((0, Instant::now()))
}

pub async fn run_tui(socket: String) -> anyhow::Result<()> {
    let client = AgentHooksClient::connect_to(socket).await?;
    // Hold decide:true so gates actually wait. Poll-only would fast-allow.
    let drain_client = client.clone();
    let mut drain = drain_client.subscribe_decider();
    tokio::spawn(async move { while drain.next().await.is_some() {} });

    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    terminal::enable_raw_mode()?;
    let cleanup = || {
        let mut o = stdout();
        let _ = terminal::disable_raw_mode();
        let _ = execute!(o, cursor::Show, terminal::LeaveAlternateScreen);
    };

    let result = run_loop(&client, &mut out).await;
    cleanup();
    result
}

async fn refresh(client: &AgentHooksClient) -> Vec<(PendingPromptInfo, Instant)> {
    let now = Instant::now();
    client
        .list_pending_prompts()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p, now))
        .collect()
}

/// Guard + send allow-with-answers. True when the prompt resolved (caller
/// pops back to the list and refreshes).
async fn try_submit_allow(client: &AgentHooksClient, d: &mut Detail) -> bool {
    if d.externally_resolved {
        hint(d, "already resolved elsewhere");
        return false;
    }
    if !ready_to_allow(d) {
        hint(
            d,
            "pick an option or write a note for every question (or deny)",
        );
        return false;
    }
    let mut decision = PromptDecision::allow();
    decision.answers = submit_answers(d);
    match client.respond_prompt(&d.prompt_id, decision).await {
        Ok(()) => true,
        Err(e) => {
            hint(d, format!("respond failed: {e}"));
            false
        }
    }
}
async fn run_loop(client: &AgentHooksClient, out: &mut Stdout) -> anyhow::Result<()> {
    let mut backlog = refresh(client).await;
    let mut cursor: usize = 0;
    let mut detail: Option<(Detail, usize)> = None;
    let mut last_refresh = Instant::now();

    loop {
        // Sync detail against the fresh backlog (external resolution, age).
        if last_refresh.elapsed() >= REFRESH {
            backlog = refresh(client).await;
            last_refresh = Instant::now();
            if let Some((d, _)) = &mut detail {
                d.externally_resolved = !backlog.iter().any(|(p, _)| p.prompt_id == d.prompt_id);
            }
            if cursor >= backlog.len() {
                cursor = backlog.len().saturating_sub(1);
            }
        }

        if let Some((d, idx)) = &detail {
            let (age, at) = backlog_age(&backlog, *idx);
            draw_detail(out, d, age, at)?;
        } else {
            draw_list(out, &backlog, cursor)?;
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if let Some((d, idx)) = &mut detail {
            let idx = *idx;
            match d.focus {
                Focus::Free => match key.code {
                    KeyCode::Esc => d.focus = Focus::Options,
                    KeyCode::Backspace => {
                        d.questions[d.q_cursor].free.pop();
                    }
                    KeyCode::Enter => {
                        // Submit allow with current picks + notes.
                        if try_submit_allow(client, d).await {
                            detail = None;
                            backlog = refresh(client).await;
                            last_refresh = Instant::now();
                        }
                    }
                    KeyCode::Char(c) => d.questions[d.q_cursor].free.push(c),
                    _ => {}
                },
                Focus::Deny => match key.code {
                    KeyCode::Esc => {
                        d.focus = Focus::Options;
                        d.deny_reason.clear();
                    }
                    KeyCode::Backspace => {
                        d.deny_reason.pop();
                    }
                    KeyCode::Enter => {
                        if d.externally_resolved {
                            hint(d, "already resolved elsewhere");
                        } else {
                            let reason = (!d.deny_reason.trim().is_empty())
                                .then(|| d.deny_reason.trim().to_string());
                            match client
                                .respond_prompt(&d.prompt_id, PromptDecision::deny(reason))
                                .await
                            {
                                Ok(()) => {
                                    detail = None;
                                    backlog = refresh(client).await;
                                    last_refresh = Instant::now();
                                }
                                Err(e) => hint(d, format!("respond failed: {e}")),
                            }
                        }
                    }
                    KeyCode::Char(c) => d.deny_reason.push(c),
                    _ => {}
                },
                Focus::Options => match key.code {
                    KeyCode::Esc => detail = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        d.opt_cursor = d.opt_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let n = d
                            .questions
                            .get(d.q_cursor)
                            .map(|q| q.options.len())
                            .unwrap_or(0);
                        if d.opt_cursor + 1 < n.max(1) {
                            d.opt_cursor += 1;
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        if !d.questions.is_empty() {
                            d.q_cursor = (d.q_cursor + 1) % d.questions.len();
                            d.opt_cursor = 0;
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(q) = d.questions.get_mut(d.q_cursor) {
                            if d.opt_cursor < q.options.len() {
                                if q.multiple {
                                    if !q.picked.insert(d.opt_cursor) {
                                        q.picked.remove(&d.opt_cursor);
                                    }
                                } else {
                                    q.picked.clear();
                                    q.picked.insert(d.opt_cursor);
                                }
                            }
                        }
                    }
                    KeyCode::Char('e') => {
                        if d.questions
                            .get(d.q_cursor)
                            .map(|q| q.custom)
                            .unwrap_or(false)
                        {
                            d.focus = Focus::Free;
                        } else if let Some(q) = d.questions.get(d.q_cursor) {
                            if q.options.is_empty() {
                                d.focus = Focus::Free;
                            } else {
                                hint(
                                    d,
                                    "this question takes no free text — pick an option or deny",
                                );
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        if try_submit_allow(client, d).await {
                            detail = None;
                            backlog = refresh(client).await;
                            last_refresh = Instant::now();
                        }
                    }
                    KeyCode::Char('d') => {
                        d.focus = Focus::Deny;
                    }
                    KeyCode::Enter => {
                        if try_submit_allow(client, d).await {
                            detail = None;
                            backlog = refresh(client).await;
                            last_refresh = Instant::now();
                        }
                    }
                    _ => {}
                },
            }
            let _ = idx;
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('r') => {
                    backlog = refresh(client).await;
                    last_refresh = Instant::now();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if cursor + 1 < backlog.len() {
                        cursor += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some((p, _)) = backlog.get(cursor) {
                        detail = Some((build_detail(p), cursor));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

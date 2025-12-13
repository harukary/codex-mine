use std::sync::Arc;

use async_trait::async_trait;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentInvocationFinishedEvent;
use codex_protocol::protocol::SubAgentInvocationStartedEvent;
use codex_protocol::protocol::SubAgentInvocationStatus;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::subagents::SubAgentDefinition;
use codex_protocol::subagents::SubAgentMode;
use codex_protocol::user_input::UserInput;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::codex::TurnContext;
use crate::codex_delegate::run_codex_conversation_one_shot;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;

#[derive(Clone)]
pub(crate) struct SubAgentTask {
    definition: SubAgentDefinition,
}

impl SubAgentTask {
    pub(crate) fn new(definition: SubAgentDefinition) -> Self {
        Self { definition }
    }
}

#[async_trait]
impl SessionTask for SubAgentTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let subagent = self.definition.clone();
        let mode = subagent.mode.unwrap_or_default();

        let started = EventMsg::SubAgentInvocationStarted(SubAgentInvocationStartedEvent {
            subagent: subagent.clone(),
        });
        session
            .clone_session()
            .send_event(ctx.as_ref(), started)
            .await;

        let ctx_for_task = Arc::clone(&ctx);
        let outcome = run_subagent(
            &session,
            ctx_for_task,
            subagent.clone(),
            input,
            cancellation_token,
            mode,
        )
        .await;

        let finished = EventMsg::SubAgentInvocationFinished(SubAgentInvocationFinishedEvent {
            subagent,
            status: outcome.status,
            output: outcome.output.clone(),
            error: outcome.error.clone(),
        });
        session
            .clone_session()
            .send_event(ctx.as_ref(), finished)
            .await;

        outcome.output
    }

    async fn abort(&self, session: Arc<SessionTaskContext>, ctx: Arc<TurnContext>) {
        let finished = EventMsg::SubAgentInvocationFinished(SubAgentInvocationFinishedEvent {
            subagent: self.definition.clone(),
            status: SubAgentInvocationStatus::Cancelled,
            output: None,
            error: None,
        });
        session
            .clone_session()
            .send_event(ctx.as_ref(), finished)
            .await;
    }
}

struct SubAgentOutcome {
    status: SubAgentInvocationStatus,
    output: Option<String>,
    error: Option<String>,
}

async fn run_subagent(
    session: &Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    definition: SubAgentDefinition,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
    mode: SubAgentMode,
) -> SubAgentOutcome {
    let mut sub_agent_config = ctx.client.config().as_ref().clone();
    sub_agent_config.sandbox_policy = sandbox_policy_for_mode(mode, &ctx.sandbox_policy);
    sub_agent_config.approval_policy = AskForApproval::Never;
    sub_agent_config.base_instructions = Some(definition.system_prompt.clone());
    sub_agent_config.developer_instructions = None;
    sub_agent_config.user_instructions = None;
    sub_agent_config.project_doc_max_bytes = 0;

    let subagent_source = SubAgentSource::Other(definition.name.clone());
    let io = match run_codex_conversation_one_shot(
        sub_agent_config,
        session.auth_manager(),
        session.models_manager(),
        input,
        session.clone_session(),
        ctx.clone(),
        cancellation_token.child_token(),
        None,
        subagent_source,
    )
    .await
    {
        Ok(io) => io,
        Err(err) => {
            return SubAgentOutcome {
                status: SubAgentInvocationStatus::Failed,
                output: None,
                error: Some(err.to_string()),
            };
        }
    };

    let mut output: Option<String> = None;
    let mut status = SubAgentInvocationStatus::Completed;

    loop {
        select! {
            _ = cancellation_token.cancelled() => {
                status = SubAgentInvocationStatus::Cancelled;
                break;
            }
            event = io.next_event() => {
                let Ok(event) = event else {
                    status = SubAgentInvocationStatus::Cancelled;
                    break;
                };
                match event.msg {
                    EventMsg::AgentMessage(ev) => {
                        output = Some(ev.message);
                    }
                    EventMsg::TaskComplete(_) => {
                        break;
                    }
                    EventMsg::TurnAborted(_) => {
                        status = SubAgentInvocationStatus::Cancelled;
                        break;
                    }
                    EventMsg::Error(err) => {
                        status = SubAgentInvocationStatus::Failed;
                        output = None;
                        return SubAgentOutcome {
                            status,
                            output,
                            error: Some(err.message),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    SubAgentOutcome {
        status,
        output,
        error: None,
    }
}

fn sandbox_policy_for_mode(mode: SubAgentMode, default: &SandboxPolicy) -> SandboxPolicy {
    match mode {
        SubAgentMode::ReadOnly => SandboxPolicy::new_read_only_policy(),
        SubAgentMode::FullAuto => {
            let mut policy = SandboxPolicy::new_workspace_write_policy();
            let (default_exclude_tmpdir, default_exclude_slash_tmp) = match default {
                SandboxPolicy::WorkspaceWrite {
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                    ..
                } => (*exclude_tmpdir_env_var, *exclude_slash_tmp),
                _ => (false, false),
            };

            if let SandboxPolicy::WorkspaceWrite {
                ref mut network_access,
                ref mut exclude_tmpdir_env_var,
                ref mut exclude_slash_tmp,
                ..
            } = policy
            {
                *network_access = default.has_full_network_access();
                *exclude_tmpdir_env_var = default_exclude_tmpdir;
                *exclude_slash_tmp = default_exclude_slash_tmp;
            }

            policy
        }
        SubAgentMode::DangerFullAccess => SandboxPolicy::DangerFullAccess,
    }
}

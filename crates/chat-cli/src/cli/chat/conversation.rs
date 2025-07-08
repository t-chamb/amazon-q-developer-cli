use std::collections::{
    HashMap,
    HashSet,
    VecDeque,
};
use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::style::Color;
use crossterm::{
    execute,
    style,
};
use serde::{
    Deserialize,
    Serialize,
};
use tracing::{
    debug,
    error,
    warn,
};

use super::consts::{
    DUMMY_TOOL_NAME,
    MAX_CHARS,
    MAX_CONVERSATION_STATE_HISTORY_LEN,
    MAX_USER_MESSAGE_SIZE,
};
use super::context::ContextManager;
use super::message::{
    AssistantMessage,
    ToolUseResult,
    ToolUseResultBlock,
    UserMessage,
    UserMessageContent,
    build_env_state,
};
use super::token_counter::{
    CharCount,
    CharCounter,
};
use super::tool_manager::ToolManager;
use super::tools::{
    InputSchema,
    QueuedTool,
    ToolOrigin,
    ToolSpec,
};
use super::util::{
    serde_value_to_document,
    truncate_safe,
};
use crate::api_client::model::{
    AssistantResponseMessage,
    ChatMessage,
    ConversationState as FigConversationState,
    ImageBlock,
    Tool,
    ToolInputSchema,
    ToolResult,
    ToolResultContentBlock,
    ToolSpecification,
    ToolUse,
    UserInputMessage,
    UserInputMessageContext,
};
use crate::cli::agent::Agents;
use crate::cli::chat::ChatError;
use crate::cli::chat::cli::hooks::{
    Hook,
    HookTrigger,
};
use crate::mcp_client::Prompt;
use crate::os::Os;

const CONTEXT_ENTRY_START_HEADER: &str = "--- CONTEXT ENTRY BEGIN ---\n";
const CONTEXT_ENTRY_END_HEADER: &str = "--- CONTEXT ENTRY END ---\n\n";

/// Tracks state related to an ongoing conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    /// Randomly generated on creation.
    conversation_id: String,
    /// The next user message to be sent as part of the conversation. Required to be [Some] before
    /// calling [Self::as_sendable_conversation_state].
    next_message: Option<UserMessage>,
    history: VecDeque<(UserMessage, AssistantMessage)>,
    /// The range in the history sendable to the backend (start inclusive, end exclusive).
    valid_history_range: (usize, usize),
    /// Similar to history in that stores user and assistant responses, except that it is not used
    /// in message requests. Instead, the responses are expected to be in human-readable format,
    /// e.g user messages prefixed with '> '. Should also be used to store errors posted in the
    /// chat.
    pub transcript: VecDeque<String>,
    pub tools: HashMap<ToolOrigin, Vec<Tool>>,
    /// Context manager for handling sticky context files
    pub context_manager: Option<ContextManager>,
    /// Tool manager for handling tool and mcp related activities
    #[serde(skip)]
    pub tool_manager: ToolManager,
    /// Cached value representing the length of the user context message.
    context_message_length: Option<usize>,
    /// Stores the latest conversation summary created by /compact
    latest_summary: Option<String>,
    #[serde(skip)]
    pub agents: Agents,
    /// Model explicitly selected by the user in this conversation state via `/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ConversationState {
    pub async fn new(
        conversation_id: &str,
        agents: Agents,
        tool_config: HashMap<String, ToolSpec>,
        tool_manager: ToolManager,
        current_model_id: Option<String>,
    ) -> Self {
        let context_manager = if let Some(agent) = agents.get_active() {
            ContextManager::from_agent(agent, None).ok()
        } else {
            None
        };

        Self {
            conversation_id: conversation_id.to_string(),
            next_message: None,
            history: VecDeque::new(),
            valid_history_range: Default::default(),
            transcript: VecDeque::with_capacity(MAX_CONVERSATION_STATE_HISTORY_LEN),
            tools: tool_config
                .into_values()
                .fold(HashMap::<ToolOrigin, Vec<Tool>>::new(), |mut acc, v| {
                    let tool = Tool::ToolSpecification(ToolSpecification {
                        name: v.name,
                        description: v.description,
                        input_schema: v.input_schema.into(),
                    });
                    acc.entry(v.tool_origin)
                        .and_modify(|tools| tools.push(tool.clone()))
                        .or_insert(vec![tool]);
                    acc
                }),
            context_manager,
            tool_manager,
            context_message_length: None,
            latest_summary: None,
            agents,
            model: current_model_id,
        }
    }

    pub fn latest_summary(&self) -> Option<&str> {
        self.latest_summary.as_deref()
    }

    pub fn history(&self) -> &VecDeque<(UserMessage, AssistantMessage)> {
        &self.history
    }

    /// Clears the conversation history and optionally the summary.
    pub fn clear(&mut self, preserve_summary: bool) {
        self.next_message = None;
        self.history.clear();
        if !preserve_summary {
            self.latest_summary = None;
        }
    }

    /// Appends a collection prompts into history and returns the last message in the collection.
    /// It asserts that the collection ends with a prompt that assumes the role of user.
    pub fn append_prompts(&mut self, mut prompts: VecDeque<Prompt>) -> Option<String> {
        debug_assert!(self.next_message.is_none(), "next_message should not exist");
        debug_assert!(prompts.back().is_some_and(|p| p.role == crate::mcp_client::Role::User));
        let last_msg = prompts.pop_back()?;
        let (mut candidate_user, mut candidate_asst) = (None::<UserMessage>, None::<AssistantMessage>);
        while let Some(prompt) = prompts.pop_front() {
            let Prompt { role, content } = prompt;
            match role {
                crate::mcp_client::Role::User => {
                    let user_msg = UserMessage::new_prompt(content.to_string());
                    candidate_user.replace(user_msg);
                },
                crate::mcp_client::Role::Assistant => {
                    let assistant_msg = AssistantMessage::new_response(None, content.into());
                    candidate_asst.replace(assistant_msg);
                },
            }
            if candidate_asst.is_some() && candidate_user.is_some() {
                let asst = candidate_asst.take().unwrap();
                let user = candidate_user.take().unwrap();
                self.append_assistant_transcript(&asst);
                self.history.push_back((user, asst));
            }
        }
        Some(last_msg.content.to_string())
    }

    pub fn next_user_message(&self) -> Option<&UserMessage> {
        self.next_message.as_ref()
    }

    pub fn reset_next_user_message(&mut self) {
        self.next_message = None;
    }

    pub async fn set_next_user_message(&mut self, input: String) {
        debug_assert!(self.next_message.is_none(), "next_message should not exist");
        if let Some(next_message) = self.next_message.as_ref() {
            warn!(?next_message, "next_message should not exist");
        }

        let input = if input.is_empty() {
            warn!("input must not be empty when adding new messages");
            "Empty prompt".to_string()
        } else {
            input
        };

        let msg = UserMessage::new_prompt(input);
        self.next_message = Some(msg);
    }

    /// Sets the response message according to the currently set [Self::next_message].
    pub fn push_assistant_message(&mut self, os: &mut Os, message: AssistantMessage) {
        debug_assert!(self.next_message.is_some(), "next_message should exist");
        let next_user_message = self.next_message.take().expect("next user message should exist");

        self.append_assistant_transcript(&message);
        self.history.push_back((next_user_message, message));

        if let Ok(cwd) = std::env::current_dir() {
            os.database.set_conversation_by_path(cwd, self).ok();
        }
    }

    /// Returns the conversation id.
    pub fn conversation_id(&self) -> &str {
        self.conversation_id.as_ref()
    }

    /// Returns the message id associated with the last assistant message, if present.
    ///
    /// This is equivalent to `utterance_id` in the Q API.
    pub fn message_id(&self) -> Option<&str> {
        self.history.back().and_then(|(_, msg)| msg.message_id())
    }

    /// Updates the history so that, when non-empty, the following invariants are in place:
    /// 1. The history length is `<= MAX_CONVERSATION_STATE_HISTORY_LEN`. Oldest messages are
    ///    dropped.
    /// 2. The first message is from the user, and does not contain tool results. Oldest messages
    ///    are dropped.
    /// 3. If the last message from the assistant contains tool results, and a next user message is
    ///    set without tool results, then the user message will have "cancelled" tool results.
    pub fn enforce_conversation_invariants(&mut self) {
        // First set the valid range as the entire history - this will be truncated as necessary
        // later below.
        self.valid_history_range = (0, self.history.len());

        // Trim the conversation history by finding the second oldest message from the user without
        // tool results - this will be the new oldest message in the history.
        //
        // Note that we reserve extra slots for [ConversationState::context_messages].
        if (self.history.len() * 2) > MAX_CONVERSATION_STATE_HISTORY_LEN - 6 {
            match self
                .history
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, (m, _))| -> bool { !m.has_tool_use_results() })
                .map(|v| v.0)
            {
                Some(i) => {
                    debug!("removing the first {i} user/assistant response pairs in the history");
                    self.valid_history_range.0 = i;
                },
                None => {
                    debug!("no valid starting user message found in the history, clearing");
                    self.valid_history_range = (0, 0);
                    // Edge case: if the next message contains tool results, then we have to just
                    // abandon them.
                    if self.next_message.as_ref().is_some_and(|m| m.has_tool_use_results()) {
                        debug!("abandoning tool results");
                        self.next_message = Some(UserMessage::new_prompt(
                            "The conversation history has overflowed, clearing state".to_string(),
                        ));
                    }
                },
            }
        }

        // If the last message from the assistant contains tool uses AND next_message is set, we need to
        // ensure that next_message contains tool results.
        if let (Some((_, AssistantMessage::ToolUse { tool_uses, .. })), Some(user_msg)) = (
            self.history
                .range_mut(self.valid_history_range.0..self.valid_history_range.1)
                .last(),
            &mut self.next_message,
        ) {
            if !user_msg.has_tool_use_results() {
                debug!(
                    "last assistant message contains tool uses, but next message is set and does not contain tool results. setting tool results as cancelled"
                );
                *user_msg = UserMessage::new_cancelled_tool_uses(
                    user_msg.prompt().map(|p| p.to_string()),
                    tool_uses.iter().map(|t| t.id.as_str()),
                );
            }
        }

        self.enforce_tool_use_history_invariants();
    }

    /// Here we also need to make sure that the tool result corresponds to one of the tools
    /// in the list. Otherwise we will see validation error from the backend. There are three
    /// such circumstances where intervention would be needed:
    /// 1. The model had decided to call a tool with its partial name AND there is only one such
    ///    tool, in which case we would automatically resolve this tool call to its correct name.
    ///    This will NOT result in an error in its tool result. The intervention here is to
    ///    substitute the partial name with its full name.
    /// 2. The model had decided to call a tool with its partial name AND there are multiple tools
    ///    it could be referring to, in which case we WILL return an error in the tool result. The
    ///    intervention here is to substitute the ambiguous, partial name with a dummy.
    /// 3. The model had decided to call a tool that does not exist. The intervention here is to
    ///    substitute the non-existent tool name with a dummy.
    pub fn enforce_tool_use_history_invariants(&mut self) {
        let tool_names: HashSet<_> = self
            .tools
            .values()
            .flat_map(|tools| {
                tools.iter().map(|tool| match tool {
                    Tool::ToolSpecification(tool_specification) => tool_specification.name.as_str(),
                })
            })
            .filter(|name| *name != DUMMY_TOOL_NAME)
            .collect();

        for (_, assistant) in &mut self.history {
            if let AssistantMessage::ToolUse { tool_uses, .. } = assistant {
                for tool_use in tool_uses {
                    if tool_names.contains(tool_use.name.as_str()) {
                        continue;
                    }

                    if tool_names.contains(tool_use.orig_name.as_str()) {
                        tool_use.name = tool_use.orig_name.clone();
                        tool_use.args = tool_use.orig_args.clone();
                        continue;
                    }

                    let names: Vec<&str> = tool_names
                        .iter()
                        .filter_map(|name| {
                            if name.ends_with(&tool_use.name) {
                                Some(*name)
                            } else {
                                None
                            }
                        })
                        .collect();

                    // There's only one tool use matching, so we can just replace it with the
                    // found name.
                    if names.len() == 1 {
                        tool_use.name = (*names.first().unwrap()).to_string();
                        continue;
                    }

                    // Otherwise, we have to replace it with a dummy.
                    tool_use.name = DUMMY_TOOL_NAME.to_string();
                }
            }
        }
    }

    pub fn add_tool_results(&mut self, tool_results: Vec<ToolUseResult>) {
        debug_assert!(self.next_message.is_none());
        self.next_message = Some(UserMessage::new_tool_use_results(tool_results));
    }

    pub fn add_tool_results_with_images(&mut self, tool_results: Vec<ToolUseResult>, images: Vec<ImageBlock>) {
        debug_assert!(self.next_message.is_none());
        self.next_message = Some(UserMessage::new_tool_use_results_with_images(tool_results, images));
    }

    /// Sets the next user message with "cancelled" tool results.
    pub fn abandon_tool_use(&mut self, tools_to_be_abandoned: &[QueuedTool], deny_input: String) {
        self.next_message = Some(UserMessage::new_cancelled_tool_uses(
            Some(deny_input),
            tools_to_be_abandoned.iter().map(|t| t.id.as_str()),
        ));
    }

    /// Returns a [FigConversationState] capable of being sent by [api_client::StreamingClient].
    ///
    /// Params:
    /// - `run_hooks` - whether hooks should be executed and included as context
    pub async fn as_sendable_conversation_state(
        &mut self,
        os: &Os,
        stderr: &mut impl Write,
        run_hooks: bool,
    ) -> Result<FigConversationState, ChatError> {
        debug_assert!(self.next_message.is_some());
        self.enforce_conversation_invariants();
        self.history.drain(self.valid_history_range.1..);
        self.history.drain(..self.valid_history_range.0);

        let context = self.backend_conversation_state(os, run_hooks, stderr).await?;
        if !context.dropped_context_files.is_empty() {
            execute!(
                stderr,
                style::SetForegroundColor(Color::DarkYellow),
                style::Print("\nSome context files are dropped due to size limit, please run "),
                style::SetForegroundColor(Color::DarkGreen),
                style::Print("/context show "),
                style::SetForegroundColor(Color::DarkYellow),
                style::Print("to learn more.\n"),
                style::SetForegroundColor(style::Color::Reset)
            )
            .ok();
        }

        Ok(context
            .into_fig_conversation_state()
            .expect("unable to construct conversation state"))
    }

    pub async fn update_state(&mut self, force_update: bool) {
        let needs_update = self.tool_manager.has_new_stuff.load(Ordering::Acquire) || force_update;
        if !needs_update {
            return;
        }
        self.tool_manager.update().await;
        // TODO: make this more targeted so we don't have to clone the entire list of tools
        self.tools = self
            .tool_manager
            .schema
            .values()
            .fold(HashMap::<ToolOrigin, Vec<Tool>>::new(), |mut acc, v| {
                let tool = Tool::ToolSpecification(ToolSpecification {
                    name: v.name.clone(),
                    description: v.description.clone(),
                    input_schema: v.input_schema.clone().into(),
                });
                acc.entry(v.tool_origin.clone())
                    .and_modify(|tools| tools.push(tool.clone()))
                    .or_insert(vec![tool]);
                acc
            });
        self.tool_manager.has_new_stuff.store(false, Ordering::Release);
        // We call this in [Self::enforce_conversation_invariants] as well. But we need to call it
        // here as well because when it's being called in [Self::enforce_conversation_invariants]
        // it is only checking the last entry.
        self.enforce_tool_use_history_invariants();
    }

    /// Returns a conversation state representation which reflects the exact conversation to send
    /// back to the model.
    pub async fn backend_conversation_state(
        &mut self,
        os: &Os,
        run_hooks: bool,
        output: &mut impl Write,
    ) -> Result<BackendConversationState<'_>, ChatError> {
        self.update_state(false).await;
        self.enforce_conversation_invariants();

        // Run hooks and add to conversation start and next user message.
        let mut conversation_start_context = None;
        if let (true, Some(cm)) = (run_hooks, self.context_manager.as_mut()) {
            let hook_results = cm.run_hooks(output).await?;
            conversation_start_context = Some(format_hook_context(hook_results.iter(), HookTrigger::ConversationStart));

            // add per prompt content to next_user_message if available
            if let Some(next_message) = self.next_message.as_mut() {
                next_message.additional_context = format_hook_context(hook_results.iter(), HookTrigger::PerPrompt);
            }
        }

        let (context_messages, dropped_context_files) = self.context_messages(os, conversation_start_context).await;

        Ok(BackendConversationState {
            conversation_id: self.conversation_id.as_str(),
            next_user_message: self.next_message.as_ref(),
            history: self
                .history
                .range(self.valid_history_range.0..self.valid_history_range.1),
            context_messages,
            dropped_context_files,
            tools: &self.tools,
            model_id: self.model.as_deref(),
        })
    }

    pub async fn truncate_large_user_messages(&mut self) {
        for (user_message, _) in &mut self.history {
            user_message.truncate_safe(25_000);
        }
    }

    /// Returns a [FigConversationState] capable of replacing the history of the current
    /// conversation with a summary generated by the model.
    pub async fn create_summary_request(
        &mut self,
        os: &Os,
        custom_prompt: Option<impl AsRef<str>>,
    ) -> Result<FigConversationState, ChatError> {
        let summary_content = match custom_prompt {
            Some(custom_prompt) => {
                // Make the custom instructions much more prominent and directive
                format!(
                    "[SYSTEM NOTE: This is an automated summarization request, not from the user]\n\n\
                            FORMAT REQUIREMENTS: Create a structured, concise summary in bullet-point format. DO NOT respond conversationally. DO NOT address the user directly.\n\n\
                            IMPORTANT CUSTOM INSTRUCTION: {}\n\n\
                            Your task is to create a structured summary document containing:\n\
                            1) A bullet-point list of key topics/questions covered\n\
                            2) Bullet points for all significant tools executed and their results\n\
                            3) Bullet points for any code or technical information shared\n\
                            4) A section of key insights gained\n\n\
                            FORMAT THE SUMMARY IN THIRD PERSON, NOT AS A DIRECT RESPONSE. Example format:\n\n\
                            ## CONVERSATION SUMMARY\n\
                            * Topic 1: Key information\n\
                            * Topic 2: Key information\n\n\
                            ## TOOLS EXECUTED\n\
                            * Tool X: Result Y\n\n\
                            Remember this is a DOCUMENT not a chat response. The custom instruction above modifies what to prioritize.\n\
                            FILTER OUT CHAT CONVENTIONS (greetings, offers to help, etc).",
                    custom_prompt.as_ref()
                )
            },
            None => {
                // Default prompt
                "[SYSTEM NOTE: This is an automated summarization request, not from the user]\n\n\
                        FORMAT REQUIREMENTS: Create a structured, concise summary in bullet-point format. DO NOT respond conversationally. DO NOT address the user directly.\n\n\
                        Your task is to create a structured summary document containing:\n\
                        1) A bullet-point list of key topics/questions covered\n\
                        2) Bullet points for all significant tools executed and their results\n\
                        3) Bullet points for any code or technical information shared\n\
                        4) A section of key insights gained\n\n\
                        FORMAT THE SUMMARY IN THIRD PERSON, NOT AS A DIRECT RESPONSE. Example format:\n\n\
                        ## CONVERSATION SUMMARY\n\
                        * Topic 1: Key information\n\
                        * Topic 2: Key information\n\n\
                        ## TOOLS EXECUTED\n\
                        * Tool X: Result Y\n\n\
                        Remember this is a DOCUMENT not a chat response.\n\
                        FILTER OUT CHAT CONVENTIONS (greetings, offers to help, etc).".to_string()
            },
        };

        let conv_state = self.backend_conversation_state(os, false, &mut vec![]).await?;
        let history = flatten_history(conv_state.history);

        let user_input_message_context = UserInputMessageContext {
            env_state: Some(build_env_state()),
            git_state: None,
            tool_results: None,
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(self.tools.values().flatten().cloned().collect::<Vec<Tool>>())
            },
        };

        let mut summary_message = UserInputMessage {
            content: summary_content,
            user_input_message_context: Some(user_input_message_context),
            user_intent: None,
            images: None,
            model_id: self.model.clone(),
        };

        // If the last message contains tool uses, then add cancelled tool results to the summary
        // message.
        if let Some(ChatMessage::AssistantResponseMessage(AssistantResponseMessage {
            tool_uses: Some(tool_uses),
            ..
        })) = history.last()
        {
            self.set_cancelled_tool_results(&mut summary_message, tool_uses);
        }

        Ok(FigConversationState {
            conversation_id: Some(self.conversation_id.clone()),
            user_input_message: summary_message,
            history: Some(history),
        })
    }

    pub fn replace_history_with_summary(&mut self, summary: String) {
        self.history.drain(..(self.history.len().saturating_sub(1)));
        self.latest_summary = Some(summary);
        // If the last message contains tool results, then we add the results to the content field
        // instead. This is required to avoid validation errors.
        // TODO: this can break since the max user content size is less than the max tool response
        // size! Alternative could be to set the last tool use as part of the context messages.
        if let Some((user, _)) = self.history.back_mut() {
            if let Some(tool_results) = user.tool_use_results() {
                let tool_content: Vec<String> = tool_results
                    .iter()
                    .flat_map(|tr| {
                        tr.content.iter().map(|c| match c {
                            ToolUseResultBlock::Json(document) => serde_json::to_string(&document)
                                .map_err(|err| error!(?err, "failed to serialize tool result"))
                                .unwrap_or_default(),
                            ToolUseResultBlock::Text(s) => s.clone(),
                        })
                    })
                    .collect::<_>();
                let mut tool_content = tool_content.join(" ");
                if tool_content.is_empty() {
                    // To avoid validation errors with empty content, we need to make sure
                    // something is set.
                    tool_content.push_str("<tool result redacted>");
                }
                let prompt = truncate_safe(&tool_content, MAX_USER_MESSAGE_SIZE).to_string();
                user.content = UserMessageContent::Prompt { prompt };
            }
        }
    }

    pub fn current_profile(&self) -> Option<&str> {
        if let Some(cm) = self.context_manager.as_ref() {
            Some(cm.current_profile.as_str())
        } else {
            None
        }
    }

    /// Returns pairs of user and assistant messages to include as context in the message history
    /// including both summaries and context files if available, and the dropped context files.
    ///
    /// TODO:
    /// - Either add support for multiple context messages if the context is too large to fit inside
    ///   a single user message, or handle this case more gracefully. For now, always return 2
    ///   messages.
    /// - Cache this return for some period of time.
    async fn context_messages(
        &mut self,
        os: &Os,
        conversation_start_context: Option<String>,
    ) -> (Option<Vec<(UserMessage, AssistantMessage)>>, Vec<(String, String)>) {
        let mut context_content = String::new();
        let mut dropped_context_files = Vec::new();
        if let Some(summary) = &self.latest_summary {
            context_content.push_str(CONTEXT_ENTRY_START_HEADER);
            context_content.push_str("This summary contains ALL relevant information from our previous conversation including tool uses, results, code analysis, and file operations. YOU MUST reference this information when answering questions and explicitly acknowledge specific details from the summary when they're relevant to the current question.\n\n");
            context_content.push_str("SUMMARY CONTENT:\n");
            context_content.push_str(summary);
            context_content.push('\n');
            context_content.push_str(CONTEXT_ENTRY_END_HEADER);
        }

        // Add context files if available
        if let Some(context_manager) = self.context_manager.as_mut() {
            match context_manager.collect_context_files_with_limit(os).await {
                Ok((files_to_use, files_dropped)) => {
                    if !files_dropped.is_empty() {
                        dropped_context_files.extend(files_dropped);
                    }

                    if !files_to_use.is_empty() {
                        context_content.push_str(CONTEXT_ENTRY_START_HEADER);
                        for (filename, content) in files_to_use {
                            context_content.push_str(&format!("[{}]\n{}\n", filename, content));
                        }
                        context_content.push_str(CONTEXT_ENTRY_END_HEADER);
                    }
                },
                Err(e) => {
                    warn!("Failed to get context files: {}", e);
                },
            }
        }

        if let Some(context) = conversation_start_context {
            context_content.push_str(&context);
        }

        if !context_content.is_empty() {
            self.context_message_length = Some(context_content.len());
            let user_msg = UserMessage::new_prompt(context_content);
            let assistant_msg = AssistantMessage::new_response(None, "I will fully incorporate this information when generating my responses, and explicitly acknowledge relevant parts of the summary when answering questions.".into());
            (Some(vec![(user_msg, assistant_msg)]), dropped_context_files)
        } else {
            (None, dropped_context_files)
        }
    }

    /// The length of the user message used as context, if any.
    pub fn context_message_length(&self) -> Option<usize> {
        self.context_message_length
    }

    /// Calculate the total character count in the conversation
    pub async fn calculate_char_count(&mut self, os: &Os) -> Result<CharCount, ChatError> {
        Ok(self
            .backend_conversation_state(os, false, &mut vec![])
            .await?
            .char_count())
    }

    /// Get the current token warning level
    pub async fn get_token_warning_level(&mut self, os: &Os) -> Result<TokenWarningLevel, ChatError> {
        let total_chars = self.calculate_char_count(os).await?;

        Ok(if *total_chars >= MAX_CHARS {
            TokenWarningLevel::Critical
        } else {
            TokenWarningLevel::None
        })
    }

    pub fn append_user_transcript(&mut self, message: &str) {
        self.append_transcript(format!("> {}", message.replace("\n", "> \n")));
    }

    pub fn append_assistant_transcript(&mut self, message: &AssistantMessage) {
        let tool_uses = message.tool_uses().map_or("none".to_string(), |tools| {
            tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>().join(",")
        });
        self.append_transcript(format!("{}\n[Tool uses: {tool_uses}]", message.content()));
    }

    pub fn append_transcript(&mut self, message: String) {
        if self.transcript.len() >= MAX_CONVERSATION_STATE_HISTORY_LEN {
            self.transcript.pop_front();
        }
        self.transcript.push_back(message);
    }

    /// Mutates `msg` so that it will contain an appropriate [UserInputMessageContext] that
    /// contains "cancelled" tool results for `tool_uses`.
    fn set_cancelled_tool_results(&self, msg: &mut UserInputMessage, tool_uses: &[ToolUse]) {
        match msg.user_input_message_context.as_mut() {
            Some(os) => {
                if os.tool_results.as_ref().is_none_or(|r| r.is_empty()) {
                    debug!(
                        "last assistant message contains tool uses, but next message is set and does not contain tool results. setting tool results as cancelled"
                    );
                    os.tool_results = Some(
                        tool_uses
                            .iter()
                            .map(|tool_use| ToolResult {
                                tool_use_id: tool_use.tool_use_id.clone(),
                                content: vec![ToolResultContentBlock::Text(
                                    "Tool use was cancelled by the user".to_string(),
                                )],
                                status: crate::api_client::model::ToolResultStatus::Error,
                            })
                            .collect::<Vec<_>>(),
                    );
                }
            },
            None => {
                debug!(
                    "last assistant message contains tool uses, but next message is set and does not contain tool results. setting tool results as cancelled"
                );
                let tool_results = tool_uses
                    .iter()
                    .map(|tool_use| ToolResult {
                        tool_use_id: tool_use.tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text(
                            "Tool use was cancelled by the user".to_string(),
                        )],
                        status: crate::api_client::model::ToolResultStatus::Error,
                    })
                    .collect::<Vec<_>>();
                let user_input_message_context = UserInputMessageContext {
                    env_state: Some(build_env_state()),
                    tool_results: Some(tool_results),
                    tools: if self.tools.is_empty() {
                        None
                    } else {
                        Some(self.tools.values().flatten().cloned().collect::<Vec<_>>())
                    },
                    ..Default::default()
                };
                msg.user_input_message_context = Some(user_input_message_context);
            },
        }
    }

    // pub async fn send_state(&mut self) -> Result<String, std::io::Error> {
    //     let pid = std::process::id();

    //     // Get relevant state information
    //     let profile = self.current_profile().unwrap_or("unknown profile");
    //     let backend_state = self.backend_conversation_state(false, true).await;
    //     let data = backend_state.calculate_conversation_size();
    //     let context_token_count: usize = *data.context_messages;
    //     let assistant_token_count: usize = *data.assistant_messages;
    //     let user_token_count: usize = *data.user_messages;
    //     let total_token_used: usize =
    //     *data.context_messages + *data.assistant_messages + *data.user_messages;
    //     let context_window_percent = (total_token_used as f32 / CONTEXT_WINDOW_SIZE as f32) * 100.0;

    //     let mut state_map = HashMap::new();
    //     state_map.insert("profile", profile.to_string());
    //     state_map.insert("tokens_used", total_token_used.to_string());
    //     state_map.insert("context_window", context_window_percent.to_string());

    //     match serde_json::to_string(&state_map) {
    //     Ok(json) => Ok(json),
    //     Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e))
    //     }
    // }
}

/// Represents a conversation state that can be converted into a [FigConversationState] (the type
/// used by the API client). Represents borrowed data, and reflects an exact [FigConversationState]
/// that can be generated from [ConversationState] at any point in time.
///
/// This is intended to provide us ways to accurately assess the exact state that is sent to the
/// model without having to needlessly clone and mutate [ConversationState] in strange ways.
pub type BackendConversationState<'a> = BackendConversationStateImpl<
    'a,
    std::collections::vec_deque::Iter<'a, (UserMessage, AssistantMessage)>,
    Option<Vec<(UserMessage, AssistantMessage)>>,
>;

/// See [BackendConversationState]
#[derive(Debug, Clone)]
pub struct BackendConversationStateImpl<'a, T, U> {
    pub conversation_id: &'a str,
    pub next_user_message: Option<&'a UserMessage>,
    pub history: T,
    pub context_messages: U,
    pub dropped_context_files: Vec<(String, String)>,
    pub tools: &'a HashMap<ToolOrigin, Vec<Tool>>,
    pub model_id: Option<&'a str>,
}

impl
    BackendConversationStateImpl<
        '_,
        std::collections::vec_deque::Iter<'_, (UserMessage, AssistantMessage)>,
        Option<Vec<(UserMessage, AssistantMessage)>>,
    >
{
    fn into_fig_conversation_state(self) -> eyre::Result<FigConversationState> {
        let history = flatten_history(self.context_messages.unwrap_or_default().iter().chain(self.history));
        let mut user_input_message: UserInputMessage = self
            .next_user_message
            .cloned()
            .map(UserMessage::into_user_input_message)
            .ok_or(eyre::eyre!("next user message is not set"))?;
        user_input_message.model_id = self.model_id.map(str::to_string);
        if let Some(os) = user_input_message.user_input_message_context.as_mut() {
            os.tools = Some(self.tools.values().flatten().cloned().collect::<Vec<_>>());
        }

        Ok(FigConversationState {
            conversation_id: Some(self.conversation_id.to_string()),
            user_input_message,
            history: Some(history),
        })
    }

    pub fn calculate_conversation_size(&self) -> ConversationSize {
        let mut user_chars = 0;
        let mut assistant_chars = 0;
        let mut context_chars = 0;

        // Count the chars used by the messages in the history.
        // this clone is cheap
        let history = self.history.clone();
        for (user, assistant) in history {
            user_chars += *user.char_count();
            assistant_chars += *assistant.char_count();
        }

        // Add any chars from context messages, if available.
        context_chars += self
            .context_messages
            .as_ref()
            .map(|v| {
                v.iter().fold(0, |acc, (user, assistant)| {
                    acc + *user.char_count() + *assistant.char_count()
                })
            })
            .unwrap_or_default();

        ConversationSize {
            context_messages: context_chars.into(),
            user_messages: user_chars.into(),
            assistant_messages: assistant_chars.into(),
        }
    }
}

/// Reflects a detailed accounting of the context window utilization for a given conversation.
#[derive(Debug, Clone, Copy)]
pub struct ConversationSize {
    pub context_messages: CharCount,
    pub user_messages: CharCount,
    pub assistant_messages: CharCount,
}

/// Converts a list of user/assistant message pairs into a flattened list of ChatMessage.
fn flatten_history<'a, T>(history: T) -> Vec<ChatMessage>
where
    T: Iterator<Item = &'a (UserMessage, AssistantMessage)>,
{
    history.fold(Vec::new(), |mut acc, (user, assistant)| {
        acc.push(ChatMessage::UserInputMessage(user.clone().into_history_entry()));
        acc.push(ChatMessage::AssistantResponseMessage(assistant.clone().into()));
        acc
    })
}

/// Character count warning levels for conversation size
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenWarningLevel {
    /// No warning, conversation is within normal limits
    None,
    /// Critical level - at single warning threshold (600K characters)
    Critical,
}

impl From<InputSchema> for ToolInputSchema {
    fn from(value: InputSchema) -> Self {
        Self {
            json: Some(serde_value_to_document(value.0).into()),
        }
    }
}

fn format_hook_context<'a>(hook_results: impl IntoIterator<Item = &'a (Hook, String)>, trigger: HookTrigger) -> String {
    let mut context_content = String::new();

    context_content.push_str(CONTEXT_ENTRY_START_HEADER);
    context_content.push_str("This section (like others) contains important information that I want you to use in your responses. I have gathered this context from valuable programmatic script hooks. You must follow any requests and consider all of the information in this section");
    if trigger == HookTrigger::ConversationStart {
        context_content.push_str(" for the entire conversation");
    }
    context_content.push_str("\n\n");

    for (hook, output) in hook_results.into_iter().filter(|(h, _)| h.trigger == trigger) {
        context_content.push_str(&format!("'{}': {output}\n\n", &hook.name));
    }
    context_content.push_str(CONTEXT_ENTRY_END_HEADER);
    context_content
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::super::message::AssistantToolUse;
    use super::*;
    use crate::api_client::model::{
        AssistantResponseMessage,
        ToolResultStatus,
    };
    use crate::cli::agent::{
        Agent,
        Agents,
    };
    use crate::cli::chat::tool_manager::ToolManager;

    const AMAZONQ_FILENAME: &str = "AmazonQ.md";

    struct NullWriter;

    impl Write for NullWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn assert_conversation_state_invariants(state: FigConversationState, assertion_iteration: usize) {
        if let Some(Some(msg)) = state.history.as_ref().map(|h| h.first()) {
            assert!(
                matches!(msg, ChatMessage::UserInputMessage(_)),
                "{assertion_iteration}: First message in the history must be from the user, instead found: {:?}",
                msg
            );
        }
        if let Some(Some(msg)) = state.history.as_ref().map(|h| h.last()) {
            assert!(
                matches!(msg, ChatMessage::AssistantResponseMessage(_)),
                "{assertion_iteration}: Last message in the history must be from the assistant, instead found: {:?}",
                msg
            );
            // If the last message from the assistant contains tool uses, then the next user
            // message must contain tool results.
            match (state.user_input_message.user_input_message_context.as_ref(), msg) {
                (
                    Some(os),
                    ChatMessage::AssistantResponseMessage(AssistantResponseMessage {
                        tool_uses: Some(tool_uses),
                        ..
                    }),
                ) if !tool_uses.is_empty() => {
                    assert!(
                        os.tool_results.as_ref().is_some_and(|r| !r.is_empty()),
                        "The user input message must contain tool results when the last assistant message contains tool uses"
                    );
                },
                _ => {},
            }
        }

        if let Some(history) = state.history.as_ref() {
            for (i, msg) in history.iter().enumerate() {
                // User message checks.
                if let ChatMessage::UserInputMessage(user) = msg {
                    assert!(
                        user.user_input_message_context
                            .as_ref()
                            .is_none_or(|os| os.tools.is_none()),
                        "the tool specification should be empty for all user messages in the history"
                    );

                    // Check that messages with tool results are immediately preceded by an
                    // assistant message with tool uses.
                    if user
                        .user_input_message_context
                        .as_ref()
                        .is_some_and(|os| os.tool_results.as_ref().is_some_and(|r| !r.is_empty()))
                    {
                        match history.get(i.checked_sub(1).unwrap_or_else(|| {
                            panic!(
                                "{assertion_iteration}: first message in the history should not contain tool results"
                            )
                        })) {
                            Some(ChatMessage::AssistantResponseMessage(assistant)) => {
                                assert!(assistant.tool_uses.is_some());
                            },
                            _ => panic!(
                                "expected an assistant response message with tool uses at index: {}",
                                i - 1
                            ),
                        }
                    }
                }
            }
        }

        let actual_history_len = state.history.unwrap_or_default().len();
        assert!(
            actual_history_len <= MAX_CONVERSATION_STATE_HISTORY_LEN,
            "history should not extend past the max limit of {}, instead found length {}",
            MAX_CONVERSATION_STATE_HISTORY_LEN,
            actual_history_len
        );

        let os = state
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("user input message context must exist");
        assert!(
            os.tools.is_some(),
            "Currently, the tool spec must be included in the next user message"
        );
    }

    #[tokio::test]
    async fn test_conversation_state_history_handling_truncation() {
        let mut os = Os::new().await.unwrap();
        let agents = Agents::default();
        let mut output = NullWriter;

        let mut tool_manager = ToolManager::default();
        let mut conversation = ConversationState::new(
            "fake_conv_id",
            agents,
            tool_manager.load_tools(&mut os, &mut output).await.unwrap(),
            tool_manager,
            None,
        )
        .await;

        // First, build a large conversation history. We need to ensure that the order is always
        // User -> Assistant -> User -> Assistant ...and so on.
        conversation.set_next_user_message("start".to_string()).await;
        for i in 0..=(MAX_CONVERSATION_STATE_HISTORY_LEN + 100) {
            let s = conversation
                .as_sendable_conversation_state(&os, &mut vec![], true)
                .await
                .unwrap();
            assert_conversation_state_invariants(s, i);
            conversation.push_assistant_message(&mut os, AssistantMessage::new_response(None, i.to_string()));
            conversation.set_next_user_message(i.to_string()).await;
        }
    }

    #[tokio::test]
    async fn test_conversation_state_history_handling_with_tool_results() {
        let mut os = Os::new().await.unwrap();
        let agents = Agents::default();

        // Build a long conversation history of tool use results.
        let mut tool_manager = ToolManager::default();
        let tool_config = tool_manager.load_tools(&mut os, &mut vec![]).await.unwrap();
        let mut conversation = ConversationState::new(
            "fake_conv_id",
            agents.clone(),
            tool_config.clone(),
            tool_manager.clone(),
            None,
        )
        .await;
        conversation.set_next_user_message("start".to_string()).await;
        for i in 0..=(MAX_CONVERSATION_STATE_HISTORY_LEN + 100) {
            let s = conversation
                .as_sendable_conversation_state(&os, &mut vec![], true)
                .await
                .unwrap();
            assert_conversation_state_invariants(s, i);

            conversation.push_assistant_message(
                &mut os,
                AssistantMessage::new_tool_use(None, i.to_string(), vec![AssistantToolUse {
                    id: "tool_id".to_string(),
                    name: "tool name".to_string(),
                    args: serde_json::Value::Null,
                    ..Default::default()
                }]),
            );
            conversation.add_tool_results(vec![ToolUseResult {
                tool_use_id: "tool_id".to_string(),
                content: vec![],
                status: ToolResultStatus::Success,
            }]);
        }

        // Build a long conversation history of user messages mixed in with tool results.
        let mut conversation =
            ConversationState::new("fake_conv_id", agents, tool_config.clone(), tool_manager.clone(), None).await;
        conversation.set_next_user_message("start".to_string()).await;
        for i in 0..=(MAX_CONVERSATION_STATE_HISTORY_LEN + 100) {
            let s = conversation
                .as_sendable_conversation_state(&os, &mut vec![], true)
                .await
                .unwrap();
            assert_conversation_state_invariants(s, i);
            if i % 3 == 0 {
                conversation.push_assistant_message(
                    &mut os,
                    AssistantMessage::new_tool_use(None, i.to_string(), vec![AssistantToolUse {
                        id: "tool_id".to_string(),
                        name: "tool name".to_string(),
                        args: serde_json::Value::Null,
                        ..Default::default()
                    }]),
                );
                conversation.add_tool_results(vec![ToolUseResult {
                    tool_use_id: "tool_id".to_string(),
                    content: vec![],
                    status: ToolResultStatus::Success,
                }]);
            } else {
                conversation.push_assistant_message(&mut os, AssistantMessage::new_response(None, i.to_string()));
                conversation.set_next_user_message(i.to_string()).await;
            }
        }
    }

    #[tokio::test]
    async fn test_conversation_state_with_context_files() {
        let mut os = Os::new().await.unwrap();
        let agents = {
            let mut agents = Agents::default();
            let mut agent = Agent::default();
            agent.included_files.push(AMAZONQ_FILENAME.to_string());
            agents.agents.insert("TestAgent".to_string(), agent);
            agents.switch("TestAgent").expect("Agent switch failed");
            agents
        };
        os.fs.write(AMAZONQ_FILENAME, "test context").await.unwrap();
        let mut output = NullWriter;

        let mut tool_manager = ToolManager::default();
        let mut conversation = ConversationState::new(
            "fake_conv_id",
            agents,
            tool_manager.load_tools(&mut os, &mut output).await.unwrap(),
            tool_manager,
            None,
        )
        .await;

        // First, build a large conversation history. We need to ensure that the order is always
        // User -> Assistant -> User -> Assistant ...and so on.
        conversation.set_next_user_message("start".to_string()).await;
        for i in 0..=(MAX_CONVERSATION_STATE_HISTORY_LEN + 100) {
            let s = conversation
                .as_sendable_conversation_state(&os, &mut vec![], true)
                .await
                .unwrap();

            // Ensure that the first two messages are the fake context messages.
            let hist = s.history.as_ref().unwrap();
            let user = &hist[0];
            let assistant = &hist[1];
            match (user, assistant) {
                (ChatMessage::UserInputMessage(user), ChatMessage::AssistantResponseMessage(_)) => {
                    assert!(
                        user.content.contains("test context"),
                        "expected context message to contain context file, instead found: {}",
                        user.content
                    );
                },
                _ => panic!("Expected the first two messages to be from the user and the assistant"),
            }

            assert_conversation_state_invariants(s, i);

            conversation.push_assistant_message(&mut os, AssistantMessage::new_response(None, i.to_string()));
            conversation.set_next_user_message(i.to_string()).await;
        }
    }

    #[tokio::test]
    async fn test_conversation_state_additional_context() {
        let mut os = Os::new().await.unwrap();
        let conversation_start_context = "conversation start context";
        let prompt_context = "prompt context";
        let agents = {
            let mut agents = Agents::default();
            let create_hooks = serde_json::json!({
                "test_conversation_start": {
                    "trigger": "conversation_start",
                    "type": "inline",
                    "command": format!("echo {}", conversation_start_context)
                }
            });
            let prompt_hooks = serde_json::json!({
                "test_per_prompt": {
                    "trigger": "per_prompt",
                    "type": "inline",
                    "command": format!("echo {}", prompt_context)
                }
            });
            let agent = Agent {
                create_hooks,
                prompt_hooks,
                ..Default::default()
            };
            agents.agents.insert("TestAgent".to_string(), agent);
            agents.switch("TestAgent").expect("Agent switch failed");
            agents
        };
        let mut output = NullWriter;

        let mut tool_manager = ToolManager::default();
        let mut conversation = ConversationState::new(
            "fake_conv_id",
            agents,
            tool_manager.load_tools(&mut os, &mut output).await.unwrap(),
            tool_manager,
            None,
        )
        .await;

        // Simulate conversation flow
        conversation.set_next_user_message("start".to_string()).await;
        for i in 0..=5 {
            let s = conversation
                .as_sendable_conversation_state(&os, &mut vec![], true)
                .await
                .unwrap();
            let hist = s.history.as_ref().unwrap();
            #[allow(clippy::match_wildcard_for_single_variants)]
            match &hist[0] {
                ChatMessage::UserInputMessage(user) => {
                    assert!(
                        user.content.contains(conversation_start_context),
                        "expected to contain '{conversation_start_context}', instead found: {}",
                        user.content
                    );
                },
                _ => panic!("Expected user message."),
            }
            assert!(
                s.user_input_message.content.contains(prompt_context),
                "expected to contain '{prompt_context}', instead found: {}",
                s.user_input_message.content
            );

            conversation.push_assistant_message(&mut os, AssistantMessage::new_response(None, i.to_string()));
            conversation.set_next_user_message(i.to_string()).await;
        }
    }
}

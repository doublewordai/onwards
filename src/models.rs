use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TextDetail {
    pub text: String,
}

#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
pub struct ImageDetail {
    pub image_url: ImageUrl,
}

#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum ContentObject {
    #[serde(rename = "text")]
    Text(TextDetail),
    #[serde(rename = "image_url")]
    ImageUrl(ImageDetail),
}

#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
#[serde(untagged)]
pub enum MessageContent {
    String(String),
    ContentArray(Vec<ContentObject>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PredictedOutputArrayPart {
    /// The type of the content part.
    pub r#type: String,
    /// The text content.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum PredictedOutputContent {
    String(String),
    Array(Vec<PredictedOutputArrayPart>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PredictedOutputType {
    Content,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PredictedOutput {
    /// The type of the predicted content you want to provide.
    pub r#type: PredictedOutputType,
    /// The content that should be matched when generating a model response.
    /// If generated tokens would match this content, the entire model response can be returned much more quickly.
    pub content: PredictedOutputContent,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Audio,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Wav,
    Mp3,
    Flac,
    Opus,
    Pcm16,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Voice {
    Alloy,
    Ash,
    Ballad,
    Coral,
    Echo,
    Sage,
    Shimmer,
    Verse,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioParameters {
    /// The voice the model uses to respond.
    pub voice: Voice,
    /// Specifies the output audio format.
    pub format: AudioFormat,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JsonSchema {
    /// A description of what the response format is for, used by the model to determine how to respond in the format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The name of the response format. Must be a-z, A-Z, 0-9, or contain underscores and dashes, with a maximum length of 64.
    pub name: String,
    /// The schema for the response format, described as a JSON Schema object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Whether to enable strict schema adherence when generating the output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl Default for JsonSchema {
    fn default() -> Self {
        JsonSchema {
            description: Some("A empty JSON Schema object".to_string()),
            name: "empty_schema".to_string(),
            schema: Some(serde_json::json!("{}")),
            strict: Some(true),
        }
    }
}

impl TryInto<String> for JsonSchema {
    type Error = serde_json::Error;
    fn try_into(self) -> Result<String, Self::Error> {
        serde_json::to_string(&self.schema)
    }
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionToolChoiceFunctionName {
    /// Name of the function.
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionToolChoiceFunction {
    /// The type of the tool. Currently, only 'function' is supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ChatCompletionToolType>,
    /// Name of the function.
    pub function: ChatCompletionToolChoiceFunctionName,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
#[serde(untagged)]
pub enum ChatCompletionToolChoice {
    None,
    Auto,
    Required,
    #[allow(clippy::enum_variant_names)]
    ChatCompletionToolChoiceFunction(ChatCompletionToolChoiceFunction),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChatCompletionToolType {
    Function,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum ChatCompletionResponseFormat {
    Text,
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema {
        json_schema: JsonSchema,
    },
    #[serde(rename = "regex_string")]
    RegexString {
        regex_string: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionFunction {
    /// Name of the function.
    pub name: String,
    /// Optional description of the function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The parameters the function takes. The model will generate JSON inputs for these parameters.
    pub parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionTool {
    /// The type of the tool. Currently, only 'function' is supported.
    pub r#type: ChatCompletionToolType,
    /// The name of the function to call.
    pub function: ChatCompletionFunction,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum StopToken {
    String(String),
    Array(Vec<String>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionStreamOptions {
    /// If set, an additional chunk will be streamed before the data: [DONE] message.
    pub include_usage: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionParameters {
    /// A list of messages comprising the conversation so far.
    pub messages: Vec<ChatMessage>,
    /// ID of the model to use. DEVIATION: we deviate here from the openai spec, we allow this to
    /// be unset (and the default model on the server to be used)
    pub model: Option<String>,
    /// Whether or not to store the output of this chat completion request for use in our model distillation or evals products.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Constrains effort on reasoning for reasoning models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Developer-defined tags and values used for filtering completions in the dashboard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on their existing frequency in the text so far,
    /// decreasing the model's likelihood to repeat the same line verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// Modify the likelihood of specified tokens appearing in the completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<HashMap<String, i32>>,
    /// Whether to return log probabilities of the output tokens or not.
    /// If true, returns the log probabilities of each output token returned in the 'content' of 'message'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// An integer between 0 and 5 specifying the number of most likely tokens to return at each token position,
    /// each with an associated log probability. 'logprobs' must be set to 'true' if this parameter is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    /// Max completion tokens, deprecated (still used by vllm)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// An upper bound for the number of tokens that can be generated for a completion, including visible output tokens and reasoning tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// How many chat completion choices to generate for each input message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Output types that you would like the model to generate for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<Modality>>,
    /// Configuration for a Predicted Output, which can greatly improve response times when large parts of the model response are known ahead of time.
    /// This is most common when you are regenerating a file with only minor changes to most of the content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prediction: Option<PredictedOutput>,
    /// Parameters for audio output. Required when audio output is requested with modalities: ["audio"].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioParameters>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on whether they appear in the text so far,
    /// increasing the model's likelihood to talk about new topics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// An object specifying the format that the model must output.
    /// Compatible with GPT-4o, GPT-4o mini, GPT-4 Turbo and all GPT-3.5 Turbo models newer than gpt-3.5-turbo-1106.
    /// Setting to { "type": "json_schema", "json_schema": {...} } enables Structured Outputs which ensures the model will match your supplied JSON schema.
    /// Setting to { "type": "json_object" } enables JSON mode, which ensures the message the model generates is valid JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ChatCompletionResponseFormat>,
    /// This feature is in Beta. If specified, our system will make a best effort to sample deterministically,
    /// such that repeated requests with the same seed and parameters should return the same result.
    /// Determinism is not guaranteed, and you should refer to the system_fingerprint response parameter to monitor changes in the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    /// Up to 4 sequences where the API will stop generating further tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopToken>,
    /// If set, partial messages will be sent, like in ChatGPT. Tokens will be sent as data-only server-sent events
    /// as they become available, with the stream terminated by a data: [DONE] message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Options for streaming response. Only set this when you set stream: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<ChatCompletionStreamOptions>,
    /// What sampling temperature to use, between 0 and 2. Higher values like 0.8 will make the output more random,
    /// while lower values like 0.2 will make it more focused and deterministic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// An alternative to sampling with temperature, called nucleus sampling, where the model considers the results of the tokens with top_p probability mass.
    /// So 0.1 means only the tokens comprising the top 10% probability mass are considered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// A list of tools the model may call. Currently, only functions are supported as a tool.
    /// Use this to provide a list of functions the model may generate JSON inputs for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatCompletionTool>>,
    /// Controls which (if any) tool is called by the model. none means the model will not call any tool and instead generates a message.
    /// auto means the model can pick between generating a message or calling one or more tools.
    /// required means the model must call one or more tools.
    /// Specifying a particular tool via {"type": "function", "function": {"name": "my_function"}} forces the model to call that tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatCompletionToolChoice>,
    /// Whether to enable parallel function calling during tool use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// A unique identifier representing your end-user, which can help OpenAI to monitor and detect abuse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct CompletionTokensDetails {
    /// Tokens generated by the model for reasoning.
    pub reasoning_tokens: u32,
    /// Audio input tokens generated by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    /// When using Predicted Outputs, the number of tokens in the prediction that appeared in the completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u32>,
    /// When using Predicted Outputs, the number of tokens in the prediction that did not appear in the completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct PromptTokensDetails {
    /// Audio input tokens present in the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    /// Cached tokens present in the prompt.
    pub cached_tokens: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct Usage {
    /// Number of tokens in the prompt.
    pub prompt_tokens: u32,
    /// Number of tokens in the completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    /// Number of tokens in the entire response.
    pub total_tokens: u32,
    /// Breakdown of tokens used in the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Breakdown of tokens used in a completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct InputAudioData {
    /// Base64 encoded audio data.
    pub data: String,
    /// The format of the encoded audio data. Currently supports "wav" and "mp3".
    pub format: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ChatMessageAudioContentPart {
    /// The type of the content part. Always input_audio.
    pub r#type: String,
    /// The input audio data.
    pub input_audio: InputAudioData,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageUrlDetail {
    Auto,
    High,
    Low,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ImageUrlType {
    /// Either a URL of the image or the base64 encoded image data.
    pub url: String,
    /// Specifies the detail level of the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<ImageUrlDetail>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ChatMessageImageContentPart {
    /// The type of the content part.
    pub r#type: String,
    /// The text content.
    pub image_url: ImageUrlType,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ChatMessageTextContentPart {
    /// The type of the content part.
    pub r#type: String,
    /// The text content.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum ChatMessageContentPart {
    Text(ChatMessageTextContentPart),
    Image(ChatMessageImageContentPart),
    Audio(ChatMessageAudioContentPart),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum ChatMessageContent {
    Text(String),
    ContentPart(Vec<ChatMessageContentPart>),
    None,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct AudioDataIdParameter {
    /// Unique identifier for a previous audio response from the model.
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct Function {
    /// The name of the function to call.
    pub name: String,
    /// The arguments to call the function with, as generated by the model in JSON format.
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct DeltaFunction {
    /// The name of the function to call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The arguments to call the function with, as generated by the model in JSON format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ToolCall {
    /// The ID of the tool call.
    pub id: String,
    /// The type of the tool. Currently, only function is supported.
    pub r#type: String,
    /// The function that the model called.
    pub function: Function,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct DeltaToolCall {
    /// The index of the tool call in the list of tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// /// The ID of the tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The type of the tool. Currently, only 'function' is supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// The function that the model called.
    pub function: DeltaFunction,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum DeltaChatMessage {
    Developer {
        /// The contents of the developer message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    System {
        /// The contents of the system message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    User {
        /// The contents of the user message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        /// The contents of the assistant message. Required unless tool_calls is specified.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ChatMessageContent>,
        /// The reasoning content by the assistant. (DeepSeek API only)
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        /// The refusal message by the assistant.
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The tool calls generated by the model, such as function calls.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<DeltaToolCall>>,
    },
    Tool {
        /// The contents of the tool message.
        content: String,
        /// Tool call that this message is responding to.
        tool_call_id: String,
    },
    #[serde(untagged)]
    Untagged {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ChatMessageContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<DeltaToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    Developer {
        /// The contents of the developer message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    System {
        /// The contents of the system message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    User {
        /// The contents of the user message.
        content: ChatMessageContent,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        /// The contents of the assistant message. Required unless tool_calls is specified.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ChatMessageContent>,
        /// The reasoning content by the assistant. (DeepSeek API only)
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        /// The refusal message by the assistant.
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        /// An optional name for the participant. Provides the model information to differentiate between participants of the same role.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Data about a previous audio response from the model.
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<AudioDataIdParameter>,
        /// The tool calls generated by the model, such as function calls.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        /// The contents of the tool message.
        content: String,
        /// Tool call that this message is responding to.
        tool_call_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// API returned complete message, or a message terminated by one of the stop sequences provided via the stop parameter.
    #[serde(rename = "stop", alias = "STOP")]
    StopSequenceReached,
    /// Incomplete model output due to max_tokens parameter or token limit.
    #[serde(rename = "length", alias = "MAX_TOKENS")]
    TokenLimitReached,
    /// Omitted content due to a flag from our content filters.
    #[serde(
        rename = "content_filter",
        alias = "SAFETY",
        alias = "SPII",
        alias = "PROHIBITED_CONTENT",
        alias = "BLOCKLIST",
        alias = "RECITATION"
    )]
    ContentFilterFlagged,
    /// The model decided to call one or more tools.
    ToolCalls,
    /// The model reached a natural stopping point. [Claude]
    EndTurn,
    /// The finish reason is unspecified. [Gemini]
    #[serde(rename = "FINISH_REASON_UNSPECIFIED	")]
    #[allow(clippy::enum_variant_names)]
    FinishReasonUnspecified,
    #[serde(rename = "MALFORMED_FUNCTION_CALL")]
    MalformedFunctionCall,
    #[serde(rename = "OTHER")]
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct LogProbsContent {
    /// The token.
    pub token: String,
    /// The log probability of this token, if it is within the top 20 most likely tokens.
    /// Otherwise, the value -9999.0 is used to signify that the token is very unlikely.
    pub logprob: f32,
    /// A list of integers representing the UTF-8 bytes representation of the token.
    pub bytes: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct LogProbs {
    /// A list of message content tokens with log probability information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<LogProbsContent>>,
    /// A list of message refusal tokens with log probability information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Vec<LogProbsContent>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ChatCompletionChoice {
    /// The index of the choice in the list of choices.
    pub index: u32,
    /// A chat completion message generated by the model.
    pub message: ChatMessage,
    /// The reason the model stopped generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    /// Log probability information for the choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct ChatCompletionChunkChoice {
    /// The index of the choice in the list of choices.
    pub index: Option<u32>,
    /// A chat completion delta generated by streamed model responses.
    pub delta: DeltaChatMessage,
    /// The reason the model stopped generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    /// Log probability information for the choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChatCompletionResponse {
    /// A unique identifier for the chat completion.
    #[serde(default)]
    pub id: String,
    /// A list of chat completion choices. Can be more than one if n is greater than 1.
    pub choices: Vec<ChatCompletionChoice>,
    /// The Unix timestamp (in seconds) of when the chat completion was created.
    #[serde(default)]
    pub created: u32,
    /// The model used for the chat completion.
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// This fingerprint represents the backend configuration that the model runs with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    /// The object type, which is always chat.completion.
    #[serde(default)]
    pub object: String,
    /// Usage statistics for the completion request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Default for ChatCompletionResponse {
    fn default() -> Self {
        Self {
            id: format!("cmpl-{}", Uuid::new_v4()),
            created: Utc::now().timestamp() as u32,
            object: "text_completion".to_string(),
            model: "primary".to_string(),
            choices: vec![],
            service_tier: None,
            system_fingerprint: None,
            usage: None,
        }
    }
}

/// This struct is used to represent the streaming chunk which is compatible with the openai api
#[derive(Clone, Deserialize, Serialize, ToSchema)]
pub struct ChatCompletionChunkResponse {
    /// A unique identifier for the chat completion. Each chunk has the same ID.
    pub id: String,
    /// A list of chat completion choices. Can be more than one if n is greater than 1.
    pub choices: Vec<ChatCompletionChunkChoice>,
    /// The Unix timestamp (in seconds) of when the chat completion was created. Each chunk has the same timestamp.
    pub created: u32,
    /// The model to generate the completion.
    pub model: String,
    /// This fingerprint represents the backend configuration that the model runs with.
    /// Can be used in conjunction with the seed request parameter to understand when backend changes have been made that might impact determinism.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    /// The object type, which is always chat.completion.chunk.
    pub object: String,
    // An optional field that will only be present when you set stream_options: {"include_usage": true} in your request. When present, it contains a null value except for the last chunk which contains the token usage statistics for the entire request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Default for ChatCompletionChunkResponse {
    fn default() -> Self {
        Self {
            id: format!("cmpl-{}", Uuid::new_v4()),
            created: Utc::now().timestamp() as u32,
            object: "chat.completion.chunk".to_string(),
            model: "primary".to_string(),
            choices: vec![],
            system_fingerprint: None,
            usage: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, ToSchema)]
pub struct EmbeddingParameters {
    /// Input text to embed, encoded as a string or array of tokens.
    /// To embed multiple inputs in a single request, pass an array of strings or array of token arrays.
    pub input: EmbeddingInput,
    /// ID of the model to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The format to return the embeddings in. Can be either float or base64.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<EmbeddingEncodingFormat>,
    /// The number of dimensions the resulting output embeddings should have. Only supported in 'text-embedding-3' and later models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// A unique identifier representing your end-user, which can help OpenAI to monitor and detect abuse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct EmbeddingResponse {
    /// The object type, which is always "embedding".
    pub object: String,
    /// A list of embedding objects.
    pub data: Vec<Embedding>,
    /// The model used to generate the embeddings.
    pub model: String,
    /// Object containing usage information for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// The string that will be turned into an embedding.
    String(String),
    /// The array of strings that will be turned into an embedding.
    StringArray(Vec<String>),
    /// The array of integers that will be turned into an embedding.
    IntegerArray(Vec<u32>),
    /// The array of arrays containing integers that will be turned into an embedding.
    IntegerArrayArray(Vec<Vec<u32>>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum EmbeddingOutput {
    Float(Vec<f64>),
    Base64(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
pub struct Embedding {
    /// The index of the embedding in the list of embeddings.
    pub index: u32,
    /// The embedding vector, which is a list of floats. Or String when encoding format is set to 'base64'.
    pub embedding: EmbeddingOutput,
    /// The object type, which is always "embedding".
    pub object: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingEncodingFormat {
    Float,
    Base64,
}

impl Default for EmbeddingInput {
    fn default() -> Self {
        EmbeddingInput::String(String::new())
    }
}

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, TS, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(rename_all = "kebab-case")]
#[derive(Default)]
pub enum SubAgentMode {
    #[default]
    ReadOnly,
    FullAuto,
    DangerFullAccess,
}


#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct SubAgentDefinition {
    pub name: String,
    pub path: PathBuf,
    pub system_prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_blocked: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SubAgentMode>,
}

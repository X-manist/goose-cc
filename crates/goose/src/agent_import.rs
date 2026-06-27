//! Import coding-agent environments into goose.
//!
//! This module handles the portable parts of an agent environment:
//! MCP/tool configuration, model/profile hints, project instructions/memory,
//! and JSONL session transcripts. It intentionally keeps source-specific
//! parsing heuristic and explicit so new agents can be added without touching
//! the session storage or MCP runtime.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use serde_yaml::Mapping;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::agents::extension::Envs;
use crate::agents::ExtensionConfig;
use crate::config::extensions::{self, ExtensionEntry};
use crate::config::paths::Paths;
use crate::config::providers::{self, ProviderEntry};
use crate::config::Config;
use crate::session::import_formats;
use crate::session::session_manager::{SessionManager, SessionType};

const AGENT_IMPORT_CONFIG_KEY: &str = "agent_import";
const EXTENSION_TIMEOUT_SECONDS: u64 = 300;
const MAX_MEMORY_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentImportSource {
    Codex,
    ClaudeCode,
    Cursor,
    Aider,
    Pi,
}

impl AgentImportSource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Cursor => "cursor",
            Self::Aider => "aider",
            Self::Pi => "pi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Aider => "Aider",
            Self::Pi => "Pi",
        }
    }

    pub fn default_source_root(self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(match self {
            Self::Codex => home.join(".codex"),
            Self::ClaudeCode => home.join(".claude"),
            Self::Cursor => home.join(".cursor"),
            Self::Aider => home,
            Self::Pi => home.join(".pi"),
        })
    }
}

impl FromStr for AgentImportSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" | "openai-codex" | "openai_codex" => Ok(Self::Codex),
            "claude" | "claude-code" | "claude_code" => Ok(Self::ClaudeCode),
            "cursor" | "cursor-agent" | "cursor_agent" => Ok(Self::Cursor),
            "aider" => Ok(Self::Aider),
            "pi" | "pi-mono" | "pi_mono" => Ok(Self::Pi),
            other => Err(anyhow!(
                "unsupported agent source '{other}'. Supported: codex, claude-code, cursor, aider, pi"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentImportOptions {
    pub source: AgentImportSource,
    pub source_root: PathBuf,
    pub working_dir: Option<PathBuf>,
    pub include_sessions: bool,
    pub session_limit: Option<usize>,
    pub apply: bool,
    pub activate_model: bool,
}

impl AgentImportOptions {
    pub fn new(source: AgentImportSource, source_root: PathBuf) -> Self {
        Self {
            source,
            source_root,
            working_dir: None,
            include_sessions: true,
            session_limit: Some(20),
            apply: true,
            activate_model: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentImportReport {
    pub source: AgentImportSource,
    pub source_root: PathBuf,
    pub working_dir: Option<PathBuf>,
    pub dry_run: bool,
    pub extensions: Vec<ImportedExtension>,
    pub config_values: Vec<ImportedConfigValue>,
    pub memories: Vec<ImportedMemory>,
    pub sessions: Vec<ImportedSession>,
    pub warnings: Vec<String>,
}

impl AgentImportReport {
    fn new(options: &AgentImportOptions) -> Self {
        Self {
            source: options.source,
            source_root: options.source_root.clone(),
            working_dir: options.working_dir.clone(),
            dry_run: !options.apply,
            extensions: Vec::new(),
            config_values: Vec::new(),
            memories: Vec::new(),
            sessions: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportedExtension {
    pub key: String,
    pub name: String,
    pub transport: String,
    pub source_path: PathBuf,
    pub enabled: bool,
    pub secret_env_keys: Vec<String>,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportedConfigValue {
    pub key: String,
    pub value: Value,
    pub source_path: Option<PathBuf>,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportedMemory {
    pub category: String,
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    pub is_global: bool,
    pub bytes: usize,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportedSession {
    pub source_path: PathBuf,
    pub format: String,
    pub imported_id: Option<String>,
    pub name: Option<String>,
    pub applied: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentImportBundle {
    extensions: Vec<ExtensionPlan>,
    provider_profile: Option<ProviderProfile>,
    source_settings: BTreeMap<String, Value>,
    memory: Vec<MemoryImport>,
    session_files: Vec<PathBuf>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ExtensionPlan {
    entry: ExtensionEntry,
    source_path: PathBuf,
    secret_envs: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct ProviderProfile {
    provider: String,
    model: String,
    source_path: PathBuf,
}

#[derive(Debug, Clone)]
struct MemoryImport {
    category: String,
    data: String,
    tags: Vec<String>,
    is_global: bool,
    source_path: PathBuf,
}

pub fn discover_agent_environment(options: &AgentImportOptions) -> Result<AgentImportReport> {
    let bundle = build_import_bundle(options)?;
    Ok(report_from_bundle(options, &bundle))
}

pub async fn import_agent_environment(
    config: &Config,
    session_manager: &SessionManager,
    options: &AgentImportOptions,
) -> Result<AgentImportReport> {
    let bundle = build_import_bundle(options)?;
    let mut report = report_from_bundle(options, &bundle);

    if !options.apply {
        return Ok(report);
    }

    apply_import_metadata(config, options, &bundle)?;
    mark_metadata_applied(&mut report);

    if let Some(profile) = &bundle.provider_profile {
        let should_activate =
            options.activate_model || providers::get_active_provider(config).is_none();
        if should_activate {
            providers::set_active_provider(config, &profile.provider, &profile.model)?;
            report.config_values.push(ImportedConfigValue {
                key: "active_provider".to_string(),
                value: json!(profile.provider),
                source_path: Some(profile.source_path.clone()),
                applied: true,
            });
        } else {
            providers::set_provider_entry(
                config,
                &profile.provider,
                &ProviderEntry {
                    enabled: true,
                    model: profile.model.clone(),
                    configured: true,
                },
            )?;
        }
        let provider_key = format!("providers.{}.model", profile.provider);
        if !mark_config_value_applied(&mut report, &provider_key) {
            report.config_values.push(ImportedConfigValue {
                key: provider_key,
                value: json!(profile.model),
                source_path: Some(profile.source_path.clone()),
                applied: true,
            });
        }
    }

    for plan in &bundle.extensions {
        for (key, value) in &plan.secret_envs {
            config.set_secret(key, value)?;
        }
        extensions::set_extension_with_config(config, plan.entry.clone());
    }
    for ext in &mut report.extensions {
        ext.applied = true;
    }

    for (idx, memory) in bundle.memory.iter().enumerate() {
        let target_path = append_memory_import(options, memory)?;
        if let Some(item) = report.memories.get_mut(idx) {
            item.target_path = target_path;
            item.applied = true;
        }
    }

    for session in &mut report.sessions {
        match fs::read_to_string(&session.source_path)
            .with_context(|| format!("failed to read session {}", session.source_path.display()))
        {
            Ok(content) => match session_manager
                .import_session(&content, Some(SessionType::User))
                .await
            {
                Ok(imported) => {
                    session.imported_id = Some(imported.id);
                    session.name = Some(imported.name);
                    session.applied = true;
                }
                Err(err) => {
                    session.error = Some(err.to_string());
                    report.warnings.push(format!(
                        "Failed to import session {}: {err}",
                        session.source_path.display()
                    ));
                }
            },
            Err(err) => {
                session.error = Some(err.to_string());
                report.warnings.push(format!(
                    "Failed to import session {}: {err}",
                    session.source_path.display()
                ));
            }
        }
    }

    Ok(report)
}

fn mark_metadata_applied(report: &mut AgentImportReport) {
    for item in &mut report.config_values {
        if item.key.starts_with(AGENT_IMPORT_CONFIG_KEY) {
            item.applied = true;
        }
    }
}

fn mark_config_value_applied(report: &mut AgentImportReport, key: &str) -> bool {
    let mut found = false;
    for item in &mut report.config_values {
        if item.key == key {
            item.applied = true;
            found = true;
        }
    }
    found
}

fn build_import_bundle(options: &AgentImportOptions) -> Result<AgentImportBundle> {
    let mut bundle = AgentImportBundle {
        extensions: Vec::new(),
        provider_profile: None,
        source_settings: BTreeMap::new(),
        memory: Vec::new(),
        session_files: Vec::new(),
        warnings: Vec::new(),
    };

    if !options.source_root.exists() {
        bundle.warnings.push(format!(
            "Source root does not exist: {}",
            options.source_root.display()
        ));
    }

    discover_config(options, &mut bundle)?;
    discover_memory(options, &mut bundle)?;
    if options.include_sessions {
        discover_sessions(options, &mut bundle)?;
    }
    dedupe_extensions(&mut bundle.extensions);
    Ok(bundle)
}

fn report_from_bundle(
    options: &AgentImportOptions,
    bundle: &AgentImportBundle,
) -> AgentImportReport {
    let mut report = AgentImportReport::new(options);
    report.warnings = bundle.warnings.clone();

    report.extensions = bundle
        .extensions
        .iter()
        .map(|plan| ImportedExtension {
            key: plan.entry.config.key(),
            name: plan.entry.config.name(),
            transport: extension_transport(&plan.entry.config).to_string(),
            source_path: plan.source_path.clone(),
            enabled: plan.entry.enabled,
            secret_env_keys: plan.secret_envs.keys().cloned().collect(),
            applied: false,
        })
        .collect();

    if let Some(profile) = &bundle.provider_profile {
        report.config_values.push(ImportedConfigValue {
            key: format!("providers.{}.model", profile.provider),
            value: json!(profile.model),
            source_path: Some(profile.source_path.clone()),
            applied: false,
        });
    }

    if !bundle.source_settings.is_empty() {
        report.config_values.push(ImportedConfigValue {
            key: format!("{AGENT_IMPORT_CONFIG_KEY}.{}", options.source.key()),
            value: json!(bundle.source_settings),
            source_path: None,
            applied: false,
        });
    }

    report.memories = bundle
        .memory
        .iter()
        .map(|memory| ImportedMemory {
            category: memory.category.clone(),
            source_path: memory.source_path.clone(),
            target_path: memory_target_path(options, memory),
            is_global: memory.is_global,
            bytes: memory.data.len(),
            applied: false,
        })
        .collect();

    report.sessions = bundle
        .session_files
        .iter()
        .map(|path| {
            let format = fs::read_to_string(path)
                .ok()
                .map(|content| import_format_label(import_formats::detect_format(&content)))
                .unwrap_or("unknown")
                .to_string();
            ImportedSession {
                source_path: path.clone(),
                format,
                imported_id: None,
                name: None,
                applied: false,
                error: None,
            }
        })
        .collect();

    report
}

fn apply_import_metadata(
    config: &Config,
    options: &AgentImportOptions,
    bundle: &AgentImportBundle,
) -> Result<()> {
    let source_key = options.source.key().to_string();
    let mut metadata = Mapping::new();
    metadata.insert(
        serde_yaml::Value::String("source".to_string()),
        serde_yaml::Value::String(options.source.label().to_string()),
    );
    metadata.insert(
        serde_yaml::Value::String("source_root".to_string()),
        serde_yaml::Value::String(options.source_root.to_string_lossy().to_string()),
    );
    if let Some(working_dir) = &options.working_dir {
        metadata.insert(
            serde_yaml::Value::String("working_dir".to_string()),
            serde_yaml::Value::String(working_dir.to_string_lossy().to_string()),
        );
    }
    metadata.insert(
        serde_yaml::Value::String("imported_at".to_string()),
        serde_yaml::Value::String(Utc::now().to_rfc3339()),
    );
    metadata.insert(
        serde_yaml::Value::String("settings".to_string()),
        serde_yaml::to_value(&bundle.source_settings)?,
    );

    config.update_param::<Mapping, _, _>(AGENT_IMPORT_CONFIG_KEY, |mut raw| {
        raw.insert(
            serde_yaml::Value::String(source_key),
            serde_yaml::Value::Mapping(metadata),
        );
        raw
    })?;
    Ok(())
}

fn discover_config(options: &AgentImportOptions, bundle: &mut AgentImportBundle) -> Result<()> {
    let paths = config_candidate_paths(options);
    for path in paths {
        if !path.exists() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                bundle
                    .warnings
                    .push(format!("Failed to read config {}: {err}", path.display()));
                continue;
            }
        };

        let lower = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if lower.ends_with(".toml") {
            bundle
                .extensions
                .extend(parse_codex_toml_mcp(&path, &content));
            discover_toml_model_profile(options.source, &path, &content, bundle);
        } else if lower.ends_with(".json") || lower == ".mcp.json" {
            match parse_json_or_yaml(&path, &content) {
                Ok(value) => {
                    bundle
                        .extensions
                        .extend(parse_mcp_servers_from_value(&path, &value));
                    discover_value_model_profile(options.source, &path, &value, bundle);
                }
                Err(err) => bundle
                    .warnings
                    .push(format!("Failed to parse config {}: {err}", path.display())),
            }
        } else if lower.ends_with(".yml") || lower.ends_with(".yaml") {
            match parse_json_or_yaml(&path, &content) {
                Ok(value) => {
                    bundle
                        .extensions
                        .extend(parse_mcp_servers_from_value(&path, &value));
                    discover_value_model_profile(options.source, &path, &value, bundle);
                }
                Err(err) => bundle
                    .warnings
                    .push(format!("Failed to parse config {}: {err}", path.display())),
            }
        }
    }
    Ok(())
}

fn config_candidate_paths(options: &AgentImportOptions) -> Vec<PathBuf> {
    let root = &options.source_root;
    let mut paths = Vec::new();
    match options.source {
        AgentImportSource::Codex => {
            paths.push(root.join("config.toml"));
            paths.push(root.join("mcp.json"));
        }
        AgentImportSource::ClaudeCode => {
            paths.push(root.join("settings.json"));
            paths.push(root.join("settings.local.json"));
            if let Some(parent) = root.parent() {
                paths.push(parent.join(".claude.json"));
            }
            paths.push(root.join("mcp.json"));
        }
        AgentImportSource::Cursor => {
            paths.push(root.join("mcp.json"));
            paths.push(root.join("settings.json"));
        }
        AgentImportSource::Aider => {
            paths.push(root.join(".aider.conf.yml"));
            paths.push(root.join(".aider.conf.yaml"));
        }
        AgentImportSource::Pi => {
            paths.push(root.join("config.json"));
            paths.push(root.join("mcp.json"));
        }
    }

    if let Some(working_dir) = &options.working_dir {
        paths.push(working_dir.join(".mcp.json"));
        paths.push(working_dir.join(".cursor").join("mcp.json"));
        paths.push(working_dir.join(".aider.conf.yml"));
        paths.push(working_dir.join(".aider.conf.yaml"));
        paths.push(working_dir.join(".claude").join("settings.json"));
    }

    dedupe_paths(paths)
}

fn parse_json_or_yaml(path: &Path, content: &str) -> Result<Value> {
    let lower = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if lower.ends_with(".yml") || lower.ends_with(".yaml") {
        let yaml: serde_yaml::Value = serde_yaml::from_str(content)?;
        Ok(serde_json::to_value(yaml)?)
    } else {
        Ok(serde_json::from_str(content)?)
    }
}

fn parse_mcp_servers_from_value(path: &Path, value: &Value) -> Vec<ExtensionPlan> {
    let mut plans = Vec::new();
    let mut maps = Vec::new();
    collect_mcp_server_maps(value, &mut maps);
    for map in maps {
        if let Some(obj) = map.as_object() {
            for (name, server) in obj {
                if let Some(plan) = extension_plan_from_server(path, name, server) {
                    plans.push(plan);
                }
            }
        }
    }
    plans
}

fn collect_mcp_server_maps<'a>(value: &'a Value, maps: &mut Vec<&'a Value>) {
    match value {
        Value::Object(obj) => {
            for key in ["mcpServers", "mcp_servers", "servers"] {
                if let Some(candidate) = obj.get(key) {
                    if candidate.is_object() {
                        maps.push(candidate);
                    }
                }
            }
            if let Some(mcp) = obj.get("mcp") {
                collect_mcp_server_maps(mcp, maps);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_mcp_server_maps(item, maps);
            }
        }
        _ => {}
    }
}

fn extension_plan_from_server(path: &Path, name: &str, server: &Value) -> Option<ExtensionPlan> {
    let obj = server.as_object()?;
    let enabled = !obj
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Imported from {}", path.display()));
    let timeout = obj
        .get("timeout")
        .or_else(|| obj.get("timeout_seconds"))
        .and_then(Value::as_u64)
        .or(Some(EXTENSION_TIMEOUT_SECONDS));

    let env_input = obj
        .get("env")
        .or_else(|| obj.get("envs"))
        .and_then(Value::as_object);
    let (envs, mut env_keys, secret_envs) = normalize_envs(env_input);
    env_keys.extend(string_array(
        obj.get("env_keys").or_else(|| obj.get("envKeys")),
    ));
    env_keys.sort();
    env_keys.dedup();

    if let Some(command) = obj
        .get("command")
        .or_else(|| obj.get("cmd"))
        .and_then(Value::as_str)
    {
        let args = match obj.get("args").or_else(|| obj.get("arguments")) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(Value::String(s)) => shell_words::split(s).unwrap_or_else(|_| vec![s.clone()]),
            _ => Vec::new(),
        };
        let entry = ExtensionEntry {
            enabled,
            config: ExtensionConfig::Stdio {
                name: name.to_string(),
                description,
                cmd: command.to_string(),
                args,
                envs: Envs::new(envs),
                env_keys,
                timeout,
                bundled: None,
                available_tools: Vec::new(),
            },
        };
        return Some(ExtensionPlan {
            entry,
            source_path: path.to_path_buf(),
            secret_envs,
        });
    }

    if let Some(uri) = obj
        .get("url")
        .or_else(|| obj.get("uri"))
        .or_else(|| obj.get("endpoint"))
        .and_then(Value::as_str)
    {
        let headers = obj
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let entry = ExtensionEntry {
            enabled,
            config: ExtensionConfig::StreamableHttp {
                name: name.to_string(),
                description,
                uri: uri.to_string(),
                envs: Envs::new(envs),
                env_keys,
                headers,
                timeout,
                socket: obj
                    .get("socket")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                bundled: None,
                available_tools: Vec::new(),
            },
        };
        return Some(ExtensionPlan {
            entry,
            source_path: path.to_path_buf(),
            secret_envs,
        });
    }

    None
}

fn parse_codex_toml_mcp(path: &Path, content: &str) -> Vec<ExtensionPlan> {
    let mut current: Option<(String, HashMap<String, String>)> = None;
    let mut servers = Vec::new();

    for raw_line in content.lines() {
        let line = strip_toml_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(server) = current.take() {
                servers.push(server);
            }
            let section = line.trim_matches(&['[', ']'][..]);
            current = toml_mcp_section_name(section).map(|name| (name, HashMap::new()));
            continue;
        }
        if let Some((_, values)) = current.as_mut() {
            if let Some((key, value)) = line.split_once('=') {
                values.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    if let Some(server) = current.take() {
        servers.push(server);
    }

    servers
        .into_iter()
        .filter_map(|(name, values)| {
            let command = values
                .get("command")
                .or_else(|| values.get("cmd"))
                .and_then(|v| parse_toml_string(v))?;
            let args = values
                .get("args")
                .or_else(|| values.get("arguments"))
                .map(|v| parse_toml_string_array(v))
                .unwrap_or_default();
            let env_raw = values
                .get("env")
                .or_else(|| values.get("envs"))
                .map(|v| parse_toml_inline_table(v))
                .unwrap_or_default();
            let env_value_map = env_raw
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect::<serde_json::Map<_, _>>();
            let (envs, env_keys, secret_envs) = normalize_envs(Some(&env_value_map));
            Some(ExtensionPlan {
                entry: ExtensionEntry {
                    enabled: true,
                    config: ExtensionConfig::Stdio {
                        name,
                        description: format!("Imported from {}", path.display()),
                        cmd: command,
                        args,
                        envs: Envs::new(envs),
                        env_keys,
                        timeout: Some(EXTENSION_TIMEOUT_SECONDS),
                        bundled: None,
                        available_tools: Vec::new(),
                    },
                },
                source_path: path.to_path_buf(),
                secret_envs,
            })
        })
        .collect()
}

fn toml_mcp_section_name(section: &str) -> Option<String> {
    for prefix in ["mcp_servers.", "mcpServers."] {
        if let Some(name) = section.strip_prefix(prefix) {
            return Some(name.trim_matches('"').to_string());
        }
    }
    None
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut prev = '\0';
    for (idx, ch) in line.char_indices() {
        if ch == '"' && prev != '\\' {
            in_string = !in_string;
        }
        if ch == '#' && !in_string {
            return &line[..idx];
        }
        prev = ch;
    }
    line
}

fn parse_toml_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches(',');
    if let Some(stripped) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(stripped.replace("\\\"", "\""));
    }
    if let Some(stripped) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Some(stripped.to_string());
    }
    Some(trimmed.to_string()).filter(|s| !s.is_empty())
}

fn parse_toml_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return parse_toml_string(trimmed).into_iter().collect();
    };
    split_quoted_csv(inner)
        .into_iter()
        .filter_map(|part| parse_toml_string(&part))
        .collect()
}

fn parse_toml_inline_table(raw: &str) -> HashMap<String, String> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return HashMap::new();
    };
    split_quoted_csv(inner)
        .into_iter()
        .filter_map(|part| {
            let (k, v) = part.split_once('=')?;
            let key = k.trim().trim_matches('"').trim_matches('\'').to_string();
            let value = parse_toml_string(v)?;
            Some((key, value))
        })
        .collect()
}

fn split_quoted_csv(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_double = false;
    let mut in_single = false;
    let mut prev = '\0';
    for ch in input.chars() {
        if ch == '"' && !in_single && prev != '\\' {
            in_double = !in_double;
        } else if ch == '\'' && !in_double && prev != '\\' {
            in_single = !in_single;
        }
        if ch == ',' && !in_double && !in_single {
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
        prev = ch;
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

fn normalize_envs(
    env_input: Option<&serde_json::Map<String, Value>>,
) -> (
    HashMap<String, String>,
    Vec<String>,
    HashMap<String, String>,
) {
    let mut envs = HashMap::new();
    let mut env_keys = Vec::new();
    let mut secret_envs = HashMap::new();
    let Some(input) = env_input else {
        return (envs, env_keys, secret_envs);
    };

    for (key, value) in input {
        let Some(value) = value.as_str().map(str::to_string) else {
            continue;
        };
        if let Some(referenced_key) = env_reference_key(&value) {
            env_keys.push(referenced_key);
        } else if is_sensitive_env_key(key) {
            env_keys.push(key.clone());
            secret_envs.insert(key.clone(), value);
        } else {
            envs.insert(key.clone(), value);
        }
    }
    env_keys.sort();
    env_keys.dedup();
    (envs, env_keys, secret_envs)
}

fn env_reference_key(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        return Some(inner.to_string()).filter(|s| !s.is_empty());
    }
    if let Some(inner) = value.strip_prefix('$') {
        return Some(inner.to_string()).filter(|s| !s.is_empty());
    }
    None
}

fn is_sensitive_env_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["key", "token", "secret", "password", "credential", "auth"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.to_string()],
        _ => Vec::new(),
    }
}

fn discover_toml_model_profile(
    source: AgentImportSource,
    path: &Path,
    content: &str,
    bundle: &mut AgentImportBundle,
) {
    let model = toml_root_string(content, "model");
    let provider = toml_root_string(content, "model_provider")
        .or_else(|| toml_root_string(content, "provider"))
        .or_else(|| default_provider_for_source(source).map(str::to_string));
    if let Some(model) = model {
        bundle
            .source_settings
            .insert("model".to_string(), json!(model));
        if let Some(provider) = provider {
            bundle
                .source_settings
                .insert("provider".to_string(), json!(provider.clone()));
            bundle.provider_profile = Some(ProviderProfile {
                provider,
                model,
                source_path: path.to_path_buf(),
            });
        }
    }
    for key in ["approval_policy", "sandbox_mode", "model_reasoning_effort"] {
        if let Some(value) = toml_root_string(content, key) {
            bundle.source_settings.insert(key.to_string(), json!(value));
        }
    }
}

fn toml_root_string(content: &str, key: &str) -> Option<String> {
    for raw_line in content.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.starts_with('[') {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return parse_toml_string(v);
            }
        }
    }
    None
}

fn discover_value_model_profile(
    source: AgentImportSource,
    path: &Path,
    value: &Value,
    bundle: &mut AgentImportBundle,
) {
    let model = first_string_value(value, &["model", "defaultModel", "default_model"]);
    let provider = first_string_value(value, &["provider", "modelProvider", "model_provider"])
        .or_else(|| default_provider_for_source(source).map(str::to_string))
        .or_else(|| {
            model
                .as_deref()
                .and_then(|model| model.split_once('/').map(|(prefix, _)| prefix.to_string()))
        });
    if let Some(model) = model {
        bundle
            .source_settings
            .insert("model".to_string(), json!(model));
        if let Some(provider) = provider {
            let (provider, model) = normalize_provider_model(&provider, &model);
            bundle
                .source_settings
                .insert("provider".to_string(), json!(provider.clone()));
            bundle.provider_profile = Some(ProviderProfile {
                provider,
                model,
                source_path: path.to_path_buf(),
            });
        }
    }
}

fn first_string_value(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(value) = obj.get(*key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

fn default_provider_for_source(source: AgentImportSource) -> Option<&'static str> {
    match source {
        AgentImportSource::Codex => Some("openai"),
        AgentImportSource::ClaudeCode => Some("anthropic"),
        AgentImportSource::Cursor => Some("cursor-agent"),
        AgentImportSource::Aider => None,
        AgentImportSource::Pi => None,
    }
}

fn normalize_provider_model(provider: &str, model: &str) -> (String, String) {
    if let Some((prefix, rest)) = model.split_once('/') {
        return (prefix.to_string(), rest.to_string());
    }
    (provider.to_string(), model.to_string())
}

fn discover_memory(options: &AgentImportOptions, bundle: &mut AgentImportBundle) -> Result<()> {
    let mut paths = memory_candidate_paths(options);
    if options.source == AgentImportSource::Codex {
        paths.extend(collect_files_limited(
            &options.source_root.join("memories"),
            &["md", "txt"],
            64,
        ));
    }
    if let Some(working_dir) = &options.working_dir {
        paths.extend(collect_files_limited(
            &working_dir.join(".cursor").join("rules"),
            &["md", "mdc", "txt"],
            64,
        ));
    }

    for path in dedupe_paths(paths) {
        if !path.exists() {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                bundle
                    .warnings
                    .push(format!("Failed to stat memory {}: {err}", path.display()));
                continue;
            }
        };
        if metadata.len() > MAX_MEMORY_FILE_BYTES {
            bundle.warnings.push(format!(
                "Skipping large memory file {} ({} bytes)",
                path.display(),
                metadata.len()
            ));
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                bundle
                    .warnings
                    .push(format!("Failed to read memory {}: {err}", path.display()));
                continue;
            }
        };
        if content.trim().is_empty() {
            continue;
        }
        let is_global = options.working_dir.is_none();
        let category = format!("imported_{}", options.source.key());
        let data = format!(
            "Imported from {} ({})\n\n{}",
            options.source.label(),
            path.display(),
            content.trim()
        );
        bundle.memory.push(MemoryImport {
            category,
            data,
            tags: vec![
                "imported-agent".to_string(),
                options.source.key().to_string(),
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("memory")
                    .to_string(),
            ],
            is_global,
            source_path: path,
        });
    }
    Ok(())
}

fn memory_candidate_paths(options: &AgentImportOptions) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let root = &options.source_root;
    match options.source {
        AgentImportSource::Codex => {
            paths.push(root.join("AGENTS.md"));
            paths.push(root.join("memory.md"));
        }
        AgentImportSource::ClaudeCode => {
            paths.push(root.join("CLAUDE.md"));
            paths.push(root.join("memory.md"));
        }
        AgentImportSource::Cursor => {
            paths.push(root.join("rules.md"));
            paths.push(root.join(".cursorrules"));
        }
        AgentImportSource::Aider => {
            paths.push(root.join(".aider.chat.history.md"));
            paths.push(root.join("CONVENTIONS.md"));
        }
        AgentImportSource::Pi => {
            paths.push(root.join("memory.md"));
        }
    }
    if let Some(working_dir) = &options.working_dir {
        paths.push(working_dir.join("AGENTS.md"));
        paths.push(working_dir.join("CLAUDE.md"));
        paths.push(working_dir.join(".cursorrules"));
        paths.push(working_dir.join("CONVENTIONS.md"));
        paths.push(working_dir.join(".aider.chat.history.md"));
    }
    paths
}

fn append_memory_import(options: &AgentImportOptions, memory: &MemoryImport) -> Result<PathBuf> {
    let target_path = memory_target_path(options, memory);
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut entry = String::new();
    if !memory.tags.is_empty() {
        entry.push_str("# ");
        entry.push_str(&memory.tags.join(" "));
        entry.push('\n');
    }
    entry.push_str(&memory.data);
    entry.push_str("\n\n");

    let existing = fs::read_to_string(&target_path).unwrap_or_default();
    if !existing.contains(&memory.source_path.to_string_lossy().to_string()) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&target_path)?
            .write_all(entry.as_bytes())?;
    }
    Ok(target_path)
}

fn memory_target_path(options: &AgentImportOptions, memory: &MemoryImport) -> PathBuf {
    if memory.is_global {
        Paths::config_dir()
            .join("memory")
            .join(format!("{}.txt", memory.category))
    } else {
        options
            .working_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".goose")
            .join("memory")
            .join(format!("{}.txt", memory.category))
    }
}

fn discover_sessions(options: &AgentImportOptions, bundle: &mut AgentImportBundle) -> Result<()> {
    let mut roots = Vec::new();
    match options.source {
        AgentImportSource::Codex => roots.push(options.source_root.join("sessions")),
        AgentImportSource::ClaudeCode => roots.push(options.source_root.join("projects")),
        AgentImportSource::Cursor => roots.push(options.source_root.join("sessions")),
        AgentImportSource::Aider => {}
        AgentImportSource::Pi => {
            roots.push(options.source_root.join("agent").join("sessions"));
            roots.push(options.source_root.join("sessions"));
        }
    }
    let mut files = Vec::new();
    for root in roots {
        files.extend(collect_files_limited(&root, &["jsonl"], 4096));
    }
    files.sort();
    if let Some(limit) = options.session_limit {
        if files.len() > limit {
            bundle.warnings.push(format!(
                "Found {} sessions; importing first {}. Use --all-sessions or a larger --session-limit to import more.",
                files.len(),
                limit
            ));
            files.truncate(limit);
        }
    }
    bundle.session_files = dedupe_paths(files);
    Ok(())
}

fn collect_files_limited(root: &Path, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files_inner(root, extensions, limit, &mut out);
    out
}

fn collect_files_inner(root: &Path, extensions: &[&str], limit: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= limit || !root.exists() {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_files_inner(&path, extensions, limit, out);
        } else if file_type.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if extensions.iter().any(|candidate| *candidate == ext) {
                out.push(path);
            }
        }
    }
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

fn dedupe_extensions(plans: &mut Vec<ExtensionPlan>) {
    let mut seen = HashSet::new();
    plans.retain(|plan| seen.insert(plan.entry.config.key()));
}

fn extension_transport(config: &ExtensionConfig) -> &'static str {
    match config {
        ExtensionConfig::Sse { .. } => "sse",
        ExtensionConfig::Stdio { .. } => "stdio",
        ExtensionConfig::Builtin { .. } => "builtin",
        ExtensionConfig::Platform { .. } => "platform",
        ExtensionConfig::StreamableHttp { .. } => "streamable_http",
        ExtensionConfig::Frontend { .. } => "frontend",
        ExtensionConfig::InlinePython { .. } => "inline_python",
    }
}

fn import_format_label(format: import_formats::ImportFormat) -> &'static str {
    match format {
        import_formats::ImportFormat::Goose => "goose",
        import_formats::ImportFormat::ClaudeCode => "claude_code",
        import_formats::ImportFormat::Codex => "codex",
        import_formats::ImportFormat::Pi => "pi",
    }
}

use std::io::Write;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{NamedTempFile, TempDir};

    fn test_config() -> (Config, NamedTempFile, NamedTempFile) {
        let config_file = NamedTempFile::new().unwrap();
        let secrets_file = NamedTempFile::new().unwrap();
        let config =
            Config::new_with_file_secrets(config_file.path(), secrets_file.path()).unwrap();
        (config, config_file, secrets_file)
    }

    fn codex_session_fixture() -> &'static str {
        r#"{"timestamp":"2026-05-22T13:37:22Z","type":"session_meta","payload":{"id":"codex-session","cwd":"/tmp/project"}}
{"timestamp":"2026-05-22T13:37:23Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"continue my work"}]}}
{"timestamp":"2026-05-22T13:37:24Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#
    }

    #[test]
    fn dry_run_discovers_codex_tools_memory_and_sessions() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("codex");
        let project = temp.path().join("project");
        fs::create_dir_all(source.join("sessions/2026/06/25")).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            source.join("config.toml"),
            r#"
model = "gpt-5"
model_provider = "openai"

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { "API_TOKEN" = "secret", "LOG_LEVEL" = "debug" }
"#,
        )
        .unwrap();
        fs::write(project.join("AGENTS.md"), "Use this repo convention.").unwrap();
        fs::write(
            source.join("sessions/2026/06/25/rollout.jsonl"),
            codex_session_fixture(),
        )
        .unwrap();

        let mut options = AgentImportOptions::new(AgentImportSource::Codex, source);
        options.working_dir = Some(project);
        options.apply = false;

        let report = discover_agent_environment(&options).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.extensions.len(), 1);
        assert_eq!(report.extensions[0].key, "filesystem");
        assert_eq!(report.extensions[0].secret_env_keys, vec!["API_TOKEN"]);
        assert_eq!(report.memories.len(), 1);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].format, "codex");
        assert!(report
            .config_values
            .iter()
            .any(|item| item.key == "providers.openai.model"));
    }

    #[tokio::test]
    async fn apply_import_writes_extension_memory_secret_and_session() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("codex");
        let project = temp.path().join("project");
        fs::create_dir_all(source.join("sessions/2026/06/25")).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            source.join("config.toml"),
            r#"
model = "gpt-5"
model_provider = "openai"

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { "API_TOKEN" = "secret", "LOG_LEVEL" = "debug" }
"#,
        )
        .unwrap();
        fs::write(project.join("AGENTS.md"), "Use this repo convention.").unwrap();
        fs::write(
            source.join("sessions/2026/06/25/rollout.jsonl"),
            codex_session_fixture(),
        )
        .unwrap();

        let (config, _config_file, _secrets_file) = test_config();
        let session_manager = SessionManager::new(temp.path().join("goose-data"));

        let mut options = AgentImportOptions::new(AgentImportSource::Codex, source);
        options.working_dir = Some(project.clone());
        options.activate_model = true;

        let report = import_agent_environment(&config, &session_manager, &options)
            .await
            .unwrap();
        assert_eq!(report.extensions.len(), 1);
        assert!(report.extensions[0].applied);
        assert_eq!(report.sessions.len(), 1);
        assert!(report.sessions[0].applied);

        let extensions: Mapping = config.get_param("extensions").unwrap();
        assert!(extensions
            .get(serde_yaml::Value::String("filesystem".to_string()))
            .is_some());
        let secret: String = config.get_secret("API_TOKEN").unwrap();
        assert_eq!(secret, "secret");

        let memory_path = project.join(".goose/memory/imported_codex.txt");
        let memory = fs::read_to_string(memory_path).unwrap();
        assert!(memory.contains("Use this repo convention."));

        let sessions = session_manager.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "continue my work");
    }

    #[test]
    fn parses_cursor_mcp_json() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("cursor");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("mcp.json"),
            r#"{
  "mcpServers": {
    "browser": {
      "url": "http://127.0.0.1:3000/mcp",
      "headers": {"X-Test": "ok"}
    }
  }
}"#,
        )
        .unwrap();
        let mut options = AgentImportOptions::new(AgentImportSource::Cursor, source);
        options.apply = false;
        options.include_sessions = false;

        let report = discover_agent_environment(&options).unwrap();
        assert_eq!(report.extensions.len(), 1);
        assert_eq!(report.extensions[0].key, "browser");
        assert_eq!(report.extensions[0].transport, "streamable_http");
    }
}

use anyhow::{Context, Result};
use goose::agent_import::{
    import_agent_environment, AgentImportOptions, AgentImportReport, AgentImportSource,
};
use goose::config::Config;
use goose::session::SessionManager;
use std::path::PathBuf;
use std::str::FromStr;

pub struct AgentImportCliOptions {
    pub source: String,
    pub source_root: Option<PathBuf>,
    pub working_dir: Option<PathBuf>,
    pub dry_run: bool,
    pub include_sessions: bool,
    pub session_limit: Option<usize>,
    pub activate_model: bool,
    pub format: String,
}

pub async fn handle_agent_import(opts: AgentImportCliOptions) -> Result<()> {
    let source = AgentImportSource::from_str(&opts.source)?;
    let source_root = opts
        .source_root
        .or_else(|| source.default_source_root())
        .with_context(|| {
            format!(
                "Could not determine a default source root for {}",
                source.label()
            )
        })?;

    let mut import_options = AgentImportOptions::new(source, source_root);
    import_options.working_dir = opts.working_dir;
    import_options.include_sessions = opts.include_sessions;
    import_options.session_limit = opts.session_limit;
    import_options.apply = !opts.dry_run;
    import_options.activate_model = opts.activate_model;

    let report = import_agent_environment(
        Config::global(),
        &SessionManager::instance(),
        &import_options,
    )
    .await?;

    match opts.format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        "text" => print_text_report(&report),
        other => anyhow::bail!("Unsupported output format: {other}. Use text or json."),
    }

    Ok(())
}

fn print_text_report(report: &AgentImportReport) {
    let mode = if report.dry_run {
        "Dry run"
    } else {
        "Imported"
    };
    println!(
        "{} {} environment from {}",
        mode,
        report.source.label(),
        report.source_root.display()
    );
    if let Some(working_dir) = &report.working_dir {
        println!("Working directory: {}", working_dir.display());
    }

    println!("\nTools/extensions: {}", report.extensions.len());
    for ext in &report.extensions {
        let status = if ext.applied { "applied" } else { "planned" };
        let secrets = if ext.secret_env_keys.is_empty() {
            String::new()
        } else {
            format!("; secrets: {}", ext.secret_env_keys.join(", "))
        };
        println!(
            "  - {} ({}, {}, {}{})",
            ext.name, ext.key, ext.transport, status, secrets
        );
    }

    println!("\nConfig values: {}", report.config_values.len());
    for item in &report.config_values {
        let status = if item.applied { "applied" } else { "planned" };
        println!("  - {} = {} ({})", item.key, item.value, status);
    }

    println!("\nMemories: {}", report.memories.len());
    for memory in &report.memories {
        let status = if memory.applied { "applied" } else { "planned" };
        let scope = if memory.is_global { "global" } else { "local" };
        println!(
            "  - {} -> {} ({} bytes, {}, {})",
            memory.source_path.display(),
            memory.target_path.display(),
            memory.bytes,
            scope,
            status
        );
    }

    println!("\nSessions: {}", report.sessions.len());
    for session in &report.sessions {
        let status = if let Some(error) = &session.error {
            format!("failed: {error}")
        } else if session.applied {
            format!(
                "imported as {}",
                session.imported_id.as_deref().unwrap_or("unknown")
            )
        } else {
            "planned".to_string()
        };
        println!(
            "  - {} ({}, {})",
            session.source_path.display(),
            session.format,
            status
        );
    }

    if !report.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &report.warnings {
            println!("  - {warning}");
        }
    }
}

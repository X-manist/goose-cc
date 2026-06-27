use goose::agents::mcp_client::McpClientTrait;
use goose::agents::platform_extensions::office::OfficeClient;
use goose::agents::{extension::PlatformExtensionContext, ToolCallContext};
use goose::session::SessionManager;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn object(value: serde_json::Value) -> JsonObject {
    value.as_object().expect("json object").clone()
}

fn write_result(path: &Path, result: &CallToolResult) -> anyhow::Result<()> {
    fs::write(path, serde_json::to_string_pretty(result)?)?;
    Ok(())
}

async fn call(
    client: &OfficeClient,
    ctx: &ToolCallContext,
    name: &str,
    args: serde_json::Value,
    out_path: &Path,
) -> anyhow::Result<()> {
    let result = client
        .call_tool(ctx, name, Some(object(args)), CancellationToken::new())
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    write_result(out_path, &result)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out_dir = absolute_path(PathBuf::from(
        args.next()
            .unwrap_or_else(|| "debug/14-office-tools-e2e".to_string()),
    ))?;
    let pdf_path =
        absolute_path(PathBuf::from(args.next().unwrap_or_else(|| {
            out_dir.join("fixtures/sample.pdf").display().to_string()
        })))?;

    fs::create_dir_all(out_dir.join("tool-results"))?;
    fs::create_dir_all(out_dir.join("goose-home"))?;

    let session_manager = Arc::new(SessionManager::new(out_dir.join("goose-home")));
    let client = OfficeClient::new(PlatformExtensionContext {
        extension_manager: None,
        session_manager,
        session: None,
        use_login_shell_path: false,
    })?;
    let ctx = ToolCallContext::new(
        "office-e2e-session".to_string(),
        Some(out_dir.clone()),
        None,
    );

    let deck_dir = out_dir.join("generated/product-ai-deck");
    call(
        &client,
        &ctx,
        "ppt_create",
        json!({
            "title": "AI 工具产品路线图",
            "subtitle": "PDF 阅读与高质量 PPT 生成",
            "output_dir": deck_dir,
            "overwrite": true,
            "theme": "executive",
            "slides": [
                {
                    "title": "AI 工具产品路线图",
                    "subtitle": "更可靠的文档理解与演示生成",
                    "layout": "title"
                },
                {
                    "title": "为什么需要内置 Office 工具",
                    "bullets": [
                        "PDF 内容先结构化读取，再让模型分析",
                        "PPT 先生成受控版式，再截图检查",
                        "大输出进入 session artifact，由 context-core 回读"
                    ],
                    "layout": "bullets",
                    "notes": "强调端到端稳定性，而不是只堆工具数量。"
                },
                {
                    "title": "质量门禁",
                    "subtitle": "先 lint，再截图，再导出",
                    "bullets": [
                        "限制每页文字密度和项目符号数量",
                        "固定 16:9 安全边距，减少遮挡",
                        "中文字体栈覆盖 macOS 与 Windows"
                    ],
                    "layout": "section"
                }
            ]
        }),
        &out_dir.join("tool-results/ppt_create.json"),
    )
    .await?;

    call(
        &client,
        &ctx,
        "ppt_lint",
        json!({ "deck_dir": deck_dir }),
        &out_dir.join("tool-results/ppt_lint.json"),
    )
    .await?;

    call(
        &client,
        &ctx,
        "ppt_export",
        json!({
            "deck_dir": deck_dir,
            "format": "html",
            "output": out_dir.join("generated/product-ai-deck-export.html")
        }),
        &out_dir.join("tool-results/ppt_export_html.json"),
    )
    .await?;

    call(
        &client,
        &ctx,
        "pdf_read",
        json!({
            "path": pdf_path,
            "start_page": 1,
            "end_page": 1,
            "max_chars": 4000
        }),
        &out_dir.join("tool-results/pdf_read.json"),
    )
    .await?;

    call(
        &client,
        &ctx,
        "pdf_search",
        json!({
            "path": pdf_path,
            "query": "Office",
            "max_matches": 10
        }),
        &out_dir.join("tool-results/pdf_search.json"),
    )
    .await?;

    call(
        &client,
        &ctx,
        "pdf_render",
        json!({
            "path": pdf_path,
            "page": 1,
            "dpi": 160,
            "include_image": false
        }),
        &out_dir.join("tool-results/pdf_render.json"),
    )
    .await?;

    println!(
        "office e2e complete\nout_dir={}\ndeck_html={}\n",
        out_dir.display(),
        out_dir
            .join("generated/product-ai-deck/slides.html")
            .display()
    );
    Ok(())
}

fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

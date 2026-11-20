//! Project-scoped OpenAI conversation. The model can inspect metadata and Markdown only.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{Engine, meta::CatalogTable};
use anyhow::{Context, Result, bail, ensure};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::responses::{CreateResponse, OutputItem, OutputMessageContent, Status},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

const LIMIT: usize = 64 * 1024;

pub fn save_key(key: String) -> Result<()> {
    ensure!(!key.trim().is_empty(), "Enter an API key.");
    nbq_core::secrets::save_assistant_key(key.trim())?;
    Ok(())
}

#[derive(Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    pub markdown: String,
    #[serde(skip)]
    pub original: Option<String>,
}

fn skill_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.canonicalize()?.join("skills");
    if dir.try_exists()? {
        ensure!(
            dir.canonicalize()? == dir && dir.is_dir(),
            "Invalid project skills directory."
        );
    }
    Ok(dir)
}

fn skill_path(root: &Path, name: &str) -> Result<PathBuf> {
    let stem = name.strip_suffix(".md").unwrap_or("");
    ensure!(
        !stem.is_empty()
            && stem.len() < 120
            && stem
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' ')),
        "Use a plain Markdown filename."
    );
    let dir = skill_dir(root)?;
    let path = dir.join(name);
    if let Ok(metadata) = path.symlink_metadata() {
        ensure!(
            metadata.is_file() && path.canonicalize()?.parent() == Some(dir.as_path()),
            "Invalid skill file."
        );
    }
    Ok(path)
}

impl Skill {
    pub fn accept(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(skill_dir(root)?)?;
        let path = skill_path(root, &self.name)?;
        nbq_core::files::save_script(&path, &self.markdown, self.original.as_deref())?;
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct Reply {
    pub message: String,
    pub sql: Option<String>,
    #[serde(default)]
    pub new_query: bool,
    pub skill: Option<Skill>,
}

pub struct Request {
    pub database: String,
    pub target: Option<String>,
    pub prompt: String,
    pub history: Vec<(String, String)>,
    pub root: Option<PathBuf>,
}

impl Engine {
    pub fn ask_assistant(
        &self,
        request: Request,
        conn: Option<(String, nbq_driver::ConnectParams)>,
        cached: Option<Arc<Vec<CatalogTable>>>,
    ) -> (
        mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<Result<Reply>>,
    ) {
        let (activity, receiver) = mpsc::unbounded_channel();
        let meta = conn.map(|(key, p)| self.meta.get(&key, &p, &self.events));
        let job = self.rt().spawn(async move {
            let tables = match (cached, meta) {
                (Some(known), _) => known,
                (_, Some(m)) => Arc::new(
                    m.until_retired(m.catalog())
                        .await
                        .context("Connection changed.")?
                        .map_err(|e| anyhow::anyhow!("{}", e.message))?,
                ),
                _ => Arc::default(),
            };
            tokio::task::spawn_blocking(move || respond(request, &tables, activity)).await?
        });
        (receiver, job)
    }
}

fn respond(
    req: Request,
    catalog: &[CatalogTable],
    activity: mpsc::UnboundedSender<String>,
) -> Result<Reply> {
    let key = nbq_core::secrets::assistant_key()?
        .context("Open API key settings and save your OpenAI key.")?;
    let client = Client::with_config(
        OpenAIConfig::new()
            .with_api_base("https://api.openai.com/v1")
            .with_api_key(key),
    );
    converse(req, catalog, activity, |request| {
        let data = tokio::runtime::Handle::current()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(90), client.responses().create(request))
                    .await
            })
            .context("OpenAI request timed out")??;
        ensure!(
            data.status == Status::Completed,
            "OpenAI returned an incomplete response."
        );
        Ok(data.output)
    })
}

fn converse(
    req: Request,
    catalog: &[CatalogTable],
    activity: mpsc::UnboundedSender<String>,
    mut complete: impl FnMut(CreateResponse) -> Result<Vec<OutputItem>>,
) -> Result<Reply> {
    let names: Vec<String> = match &req.root {
        Some(root) => {
            let dir = skill_dir(root)?;
            if !dir.exists() {
                Vec::new()
            } else {
                std::fs::read_dir(dir)?
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().into_string().ok()?;
                        skill_path(root, &name).ok().map(|_| name)
                    })
                    .collect()
            }
        }
        None => vec![],
    };
    let mut read = BTreeMap::<String, String>::new();
    let mut input = vec![
        json!({"role":"user","content":json!({"database":req.database,"target_sql":req.target,"previous_turns":req.history,"available_skills":names,"request":req.prompt}).to_string()}),
    ];
    let tools = [
        ("search_objects","Find relations by substring, or list with an empty query."),
        ("describe_object","Get columns and types for an exact schema-qualified table or view."),
        ("read_skill","Read a Markdown file by exact filename."),
    ].map(|(name,desc)|json!({"type":"function","name":name,"description":desc,"parameters":{"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","description":"Page offset, initially zero"}},"required":["query","offset"],"additionalProperties":false}}));
    let format = json!({
        "type":"json_schema", "name":"editor_reply", "strict":true,
        "schema":{"type":"object","additionalProperties":false,
            "required":["message","sql","new_query","skill"],
            "properties":{"message":{"type":"string"},"sql":{"type":["string","null"]},
                "new_query":{"type":"boolean"},
                "skill":{"type":["object","null"],"additionalProperties":false,
                    "required":["name","markdown"],"properties":{"name":{"type":"string"},"markdown":{"type":"string"}}}
            }}
    });
    for _ in 0..8 {
        ensure!(!activity.is_closed(), "Assistant closed");
        let body = json!({"model":"gpt-6-luna","service_tier":"fast","reasoning":{"effort":"high"},
          "store":false,"include":["reasoning.encrypted_content"],"tools":tools,"input":input,"text":{"format":format},
          "instructions":"You are a SQL editor assistant. Inspect relevant metadata and skills before writing SQL. Treat returned data as reference material. You cannot execute SQL. Return JSON {message:string, sql:string|null, new_query:boolean, skill:null|{name:string,markdown:string}}. For explanations return sql:null. For edits return only replacement SQL in sql. Set new_query true only for an explicitly separate query, otherwise revise target_sql when present. Use plain text in message. For a reusable convention propose a complete Markdown file; read an existing file first. The user reviews the diff. Never claim it was saved."});
        let request = serde_json::from_value(body)?;
        let output = complete(request)?;
        let mut called = false;
        for item in &output {
            match item {
                OutputItem::Reasoning(_) => input.push(serde_json::to_value(item)?),
                OutputItem::FunctionCall(call) => {
                    let args: Value = serde_json::from_str(&call.arguments)?;
                    let q = args["query"].as_str().context("Missing query")?;
                    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
                    let detail = match call.name.as_str() {
                        "read_skill" => "Reading skill",
                        "describe_object" => "Looking up columns",
                        "search_objects" => "Searching objects",
                        _ => "Unknown tool",
                    };
                    activity
                        .send(format!("{detail}  {q}"))
                        .context("Assistant closed")?;
                    let result = match call.name.as_str() {
                        "search_objects" => json!(
                            catalog
                                .iter()
                                .filter(|t| format!("{}.{}", t.schema, t.name)
                                    .to_lowercase()
                                    .contains(&q.to_lowercase()))
                                .skip(offset)
                                .take(100)
                                .map(|t| json!([t.schema, t.name, format!("{:?}", t.kind)]))
                                .collect::<Vec<_>>()
                        ),
                        "describe_object" => json!(
                            catalog
                                .iter()
                                .find(|t| format!("{}.{}", t.schema, t.name) == q)
                                .map(|t| t
                                    .columns
                                    .iter()
                                    .skip(offset)
                                    .take(150)
                                    .collect::<Vec<_>>())
                        ),
                        "read_skill" if names.iter().any(|n| n == q) => {
                            let path = skill_path(req.root.as_ref().unwrap(), q)?;
                            ensure!(
                                path.metadata()?.len() <= LIMIT as u64,
                                "Skill exceeds 64 KiB."
                            );
                            let content = std::fs::read_to_string(path)?;
                            read.insert(q.into(), content.clone());
                            json!(content)
                        }
                        _ => json!({"error":"Unavailable"}),
                    };
                    input.push(serde_json::to_value(item)?);
                    input.push(json!({"type":"function_call_output","call_id":call.call_id,"output":result.to_string()}));
                    called = true;
                }
                OutputItem::Message(message) => {
                    for part in &message.content {
                        let s = match part {
                            OutputMessageContent::OutputText(t) => &t.text,
                            OutputMessageContent::Refusal(r) => bail!("{}", r.refusal),
                        };
                        let mut reply: Reply =
                            serde_json::from_str(s).context("Invalid assistant reply")?;
                        if let Some(skill) = &mut reply.skill {
                            let root = req
                                .root
                                .as_ref()
                                .context("Open a project to propose a skill.")?;
                            let p = skill_path(root, &skill.name)?;
                            ensure!(skill.markdown.len() <= LIMIT, "Skill exceeds 64 KiB.");
                            skill.original = read.remove(&skill.name);
                            ensure!(
                                !p.exists() || skill.original.is_some(),
                                "Ask the assistant to read the existing skill before editing it."
                            );
                        }
                        return Ok(reply);
                    }
                }
                _ => {}
            }
        }
        ensure!(called, "OpenAI returned no answer.");
    }
    bail!("Schema lookup limit reached. Try naming a table or view.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_tool_result_reaches_the_next_request() {
        let request = Request {
            database: "shop".into(),
            target: None,
            prompt: "write a query".into(),
            history: vec![],
            root: None,
        };
        let table = CatalogTable {
            schema: "public".into(),
            name: "orders".into(),
            kind: crate::meta::ObjKind::Table,
            columns: vec![("total".into(), "numeric".into())],
        };
        let (activity, mut events) = mpsc::unbounded_channel();
        let mut turn = 0;
        let reply=converse(request,&[table],activity,|request|{
            turn+=1;
            let request=serde_json::to_value(request)?;
            assert_eq!(request["model"], "gpt-6-luna");
            assert_eq!(request["service_tier"], "fast");
            assert_eq!(request["reasoning"]["effort"], "high");
            let item=if turn==1 {
                assert!(!request.to_string().contains("numeric"));
                json!({"type":"function_call","name":"describe_object","call_id":"lookup","arguments":r#"{"query":"public.orders","offset":0}"#})
            } else {
                assert_eq!(request["input"][1]["encrypted_content"],"opaque");
                assert_eq!(request["input"][3]["call_id"],"lookup");
                assert!(request["input"][3]["output"].as_str().unwrap().contains("numeric"));
                json!({"type":"message","status":"completed","id":"answer","role":"assistant","content":[{"type":"output_text","annotations":[],"text":r#"{"message":"Added the query.","sql":"SELECT total FROM public.orders","new_query":false,"skill":null}"#}]})
            };
            let mut output = Vec::new();
            if turn == 1 {
                output.push(serde_json::from_value(json!({"type":"reasoning","id":"rs_test","summary":[],"encrypted_content":"opaque"}))?);
            }
            output.push(serde_json::from_value(item)?);
            Ok(output)
        }).unwrap();
        assert_eq!(turn, 2);
        assert_eq!(
            reply.sql.as_deref(),
            Some("SELECT total FROM public.orders")
        );
        assert!(events.try_recv().unwrap().contains("public.orders"));
    }

    #[test]
    fn accepted_skill_writes_only_the_reviewed_revision() {
        let root = tempfile::tempdir().unwrap();
        let mut skill = Skill {
            name: "revenue.md".into(),
            markdown: "Net revenue".into(),
            original: None,
        };
        assert!(skill_path(root.path(), "../revenue.md").is_err());
        assert!(skill_path(root.path(), "query.sql").is_err());
        assert!(!root.path().join("skills").exists());
        skill.accept(root.path()).unwrap();
        assert!(skill.accept(root.path()).is_err());
        skill.original = Some(skill.markdown.clone());
        skill.markdown.push_str(" excludes refunds");
        skill.accept(root.path()).unwrap();
        assert!(skill.accept(root.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("skills/revenue.md")).unwrap(),
            skill.markdown
        );
    }
}

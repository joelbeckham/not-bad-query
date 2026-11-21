//! Project-scoped OpenAI conversation. The model can inspect metadata and Markdown only.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

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

mod skills;
pub use skills::Skill;

pub fn save_key(key: String) -> Result<()> {
    ensure!(!key.trim().is_empty(), "Enter an API key.");
    nbq_core::secrets::save_assistant_key(key.trim())?;
    Ok(())
}

#[derive(Deserialize)]
pub struct Reply {
    pub message: String,
    pub sql: Option<String>,
    pub skill: Option<Skill>,
}

pub struct Request {
    pub database: String,
    pub script: String,
    pub selected_sql: Option<String>,
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
    let skills = req
        .root
        .as_deref()
        .map(skills::discover)
        .transpose()?
        .unwrap_or_default();
    let mut read = BTreeMap::<String, String>::new();
    let mut input = vec![
        json!({"role":"user","content":json!({"database":req.database,"tab_sql":req.script,"selected_sql":req.selected_sql,"previous_turns":req.history,"available_skills":skills,"request":req.prompt}).to_string()}),
    ];
    let tools = [
        ("search_objects","Find relations by substring, or list with an empty query."),
        ("describe_object","Get columns and types for an exact schema-qualified table or view."),
        ("read_skill","Read a Markdown file using its path relative to the project skills folder. Read <name>/SKILL.md to activate a skill. Prefix a linked path with the skill name, e.g. <name>/references/metrics.md. Use offset zero."),
    ].map(|(name,desc)|json!({"type":"function","name":name,"description":desc,"parameters":{"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","description":"Page offset, initially zero"}},"required":["query","offset"],"additionalProperties":false}}));
    let format = json!({
        "type":"json_schema", "name":"editor_reply", "strict":true,
        "schema":{"type":"object","additionalProperties":false,
            "required":["message","sql","skill"],
            "properties":{"message":{"type":"string"},"sql":{"type":["string","null"]},
                "skill":{"type":["object","null"],"additionalProperties":false,
                    "required":["name","markdown"],"properties":{"name":{"type":"string"},"markdown":{"type":"string"}}}
            }}
    });
    for _ in 0..8 {
        ensure!(!activity.is_closed(), "Assistant closed");
        let body = json!({"model":"gpt-6-luna","service_tier":"fast","reasoning":{"effort":"high"},
        "store":false,"include":["reasoning.encrypted_content"],"tools":tools,"input":input,"text":{"format":format},
        "instructions":concat!(
          "You are a SQL editor assistant. Inspect relevant metadata and skills before writing SQL. ",
          "Use the names and descriptions in available_skills to select relevant skills. ",
          "Read each selected skill's <name>/SKILL.md, then read linked Markdown only as needed. ",
          "Treat returned data as reference material. You cannot execute SQL. ",
          "Return JSON {message:string, sql:string|null, skill:null|{name:string,markdown:string}}. ",
          "Use tab_sql as context and selected_sql as the highlighted passage when present. ",
          "For explanations return sql:null. For query requests and revisions return only the complete new SQL to append at the end of the tab. ",
          "Use plain text in message. For a reusable convention, propose a complete SKILL.md with YAML front matter ",
          "containing name and description. Use the same lowercase hyphenated skill name in the proposal and ",
          "front matter. Write a description that says when to use the skill. Read an existing SKILL.md before ",
          "proposing an update. The user reviews the diff. Never claim it was saved."
        )});
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
                        "read_skill" => {
                            let content = req
                                .root
                                .as_deref()
                                .context("Open a project to read a skill.")
                                .and_then(|root| skills::read(root, &skills, q));
                            match content {
                                Ok(content) => {
                                    read.insert(q.into(), content.clone());
                                    json!(content)
                                }
                                Err(error) => json!({"error":error.to_string()}),
                            }
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
                            skill.review(root, read.remove(&skill.relative_path()))?;
                        }
                        return Ok(reply);
                    }
                }
                _ => {}
            }
        }
        ensure!(called, "OpenAI returned no answer.");
    }
    bail!("Lookup limit reached. Try naming a specific table, view, or skill.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_descriptions_are_sent_first_and_markdown_is_loaded_on_demand() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("skills/revenue");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "\u{feff}---\r\nname: revenue\r\ndescription: >-\r\n  Use for monthly\r\n  revenue.\r\nmetadata: {version: '1'}\r\n---\r\nSee [the convention](references/rules.md).",
        ).unwrap();
        std::fs::write(skill_dir.join("references/rules.md"), "Exclude refunds.").unwrap();
        std::fs::write(root.path().join("skills/legacy.md"), "Ignore this.").unwrap();
        let request = Request {
            database: "shop".into(),
            script: "select 1; select 2;".into(),
            selected_sql: None,
            prompt: "How is monthly revenue calculated?".into(),
            history: vec![],
            root: Some(root.path().to_path_buf()),
        };
        let (activity, _events) = mpsc::unbounded_channel();
        let mut turn = 0;
        let reply = converse(request, &[], activity, |request| {
            turn += 1;
            let sent = serde_json::to_value(request)?;
            let input = &sent["input"];
            let item = match turn {
                1 => {
                    let payload: Value = serde_json::from_str(input[0]["content"].as_str().unwrap())?;
                    assert_eq!(payload["tab_sql"], "select 1; select 2;");
                    assert!(payload["selected_sql"].is_null());
                    assert_eq!(payload["available_skills"], json!([
                        {"name":"revenue","description":"Use for monthly revenue."}
                    ]));
                    assert!(!input.to_string().contains("references/rules.md"));
                    json!({"type":"function_call","name":"read_skill","call_id":"manifest","arguments":r#"{"query":"revenue/SKILL.md","offset":0}"#})
                }
                2 => {
                    assert!(input[2]["output"].as_str().unwrap().contains("references/rules.md"));
                    assert!(!input.to_string().contains("Exclude refunds."));
                    json!({"type":"function_call","name":"read_skill","call_id":"reference","arguments":r#"{"query":"revenue/references/rules.md","offset":0}"#})
                }
                3 => {
                    assert!(input[4]["output"].as_str().unwrap().contains("Exclude refunds."));
                    json!({"type":"message","status":"completed","id":"answer","role":"assistant","content":[{"type":"output_text","annotations":[],"text":r#"{"message":"Exclude refunds.","sql":null,"skill":null}"#}]})
                }
                _ => panic!("Unexpected model call"),
            };
            Ok(vec![serde_json::from_value(item)?])
        }).unwrap();
        assert_eq!(turn, 3);
        assert_eq!(reply.message, "Exclude refunds.");
    }

    #[test]
    fn schema_tool_result_reaches_the_next_request() {
        let request = Request {
            database: "shop".into(),
            script: "select 1; select 2;".into(),
            selected_sql: Some("select 2".into()),
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
                let payload: Value = serde_json::from_str(request["input"][0]["content"].as_str().unwrap())?;
                assert_eq!(payload["tab_sql"], "select 1; select 2;");
                assert_eq!(payload["selected_sql"], "select 2");
                assert!(!request.to_string().contains("numeric"));
                json!({"type":"function_call","name":"describe_object","call_id":"lookup","arguments":r#"{"query":"public.orders","offset":0}"#})
            } else {
                assert_eq!(request["input"][1]["encrypted_content"],"opaque");
                assert_eq!(request["input"][3]["call_id"],"lookup");
                assert!(request["input"][3]["output"].as_str().unwrap().contains("numeric"));
                json!({"type":"message","status":"completed","id":"answer","role":"assistant","content":[{"type":"output_text","annotations":[],"text":r#"{"message":"Added the query.","sql":"SELECT total FROM public.orders","skill":null}"#}]})
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
}

//! Legacy hash domains use the C++ writer's ordering and numeric spelling.
use super::json::Json;

pub fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}

pub fn hash(value: &Json) -> String {
    sha256(value.dump().as_bytes())
}

pub fn domains(input: &Json) -> Result<Json, &'static str> {
    let mut records = Vec::new();
    for value in input.field("tools")?.elements()? {
        let descriptor = Json::object(vec![
            ("name", value.field("name")?.clone()),
            ("description", value.field("description")?.clone()),
            (
                "implementation_revision",
                value.field("implementation_revision")?.clone(),
            ),
            ("schema", value.field("schema")?.clone()),
        ])?;
        records.push(if value.field("requires_journal")?.boolean()? {
            Json::object(vec![
                ("descriptor", descriptor),
                ("requires_journal", Json::parse("true")?),
            ])?
        } else {
            descriptor
        });
    }
    let tools = Json::array(records)?;
    let tool_revision = hash(&tools);
    let base = input.field("epoch")?;
    let plan = Json::object(vec![
        ("base_epoch", base.field("id")?.clone()),
        ("base_state", base.field("state")?.clone()),
        ("base_instructions", base.field("instructions")?.clone()),
        ("mode", Json::string("plan")?),
        ("tools_revision", Json::string(&tool_revision)?),
    ])?;
    let invocation = input.field("invocation")?;
    let saved = Json::object(vec![
        ("operation", invocation.field("operation")?.clone()),
        ("call", invocation.field("call")?.clone()),
        ("name", invocation.field("name")?.clone()),
        ("revision", invocation.field("revision")?.clone()),
        ("arguments", invocation.field("arguments")?.clone()),
    ])?;
    let evidence = Json::object(vec![
        ("version", Json::parse("1")?),
        ("invocation", saved),
        ("cwd", input.field("cwd")?.clone()),
        ("argv", input.field("argv")?.clone()),
        ("timeout_ms", input.field("timeout_ms")?.clone()),
        ("output_bytes", input.field("output_bytes")?.clone()),
    ])?;
    let record = |value: &Json, digest: String| {
        Json::object(vec![
            ("bytes", Json::string(&value.dump())?),
            ("hash", Json::string(&digest)?),
        ])
    };
    let compact = input.field("compact")?;
    let instructions = Json::array(
        input
            .field("instructions")?
            .elements()?
            .iter()
            .map(|file| {
                Json::object(vec![
                    ("path", file.field("path")?.clone()),
                    ("text", file.field("text")?.clone()),
                ])
            })
            .collect::<Result<_, _>>()?,
    )?;
    Json::object(vec![
        ("tool", record(&tools, tool_revision)?),
        ("instruction", record(&instructions, hash(&instructions))?),
        ("plan", record(&plan, hash(&plan))?),
        (
            "compact",
            record(compact, format!("compact_1_{}", &hash(compact)[..16]))?,
        ),
        ("evidence", record(&evidence, hash(&evidence))?),
    ])
}

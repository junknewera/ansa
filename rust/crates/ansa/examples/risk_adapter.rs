use ansa::protocol::{json::Json, legacy_json_v1::hash};
use std::io::{self, BufRead, Read};
#[path = "support/frame_server.rs"]
mod frame_server;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--serve-h2") {
        use std::io::Write;
        if args.len() != 4 {
            return Err("server arguments".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            println!("{}", listener.local_addr()?.port());
            io::stdout().flush()?;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(6),
                frame_server::serve(listener, &std::path::PathBuf::from(&args[2]), &args[3]),
            )
            .await;
            Ok(())
        });
    }
    let mut input_stream = io::stdin().lock();
    loop {
        let mut line = String::new();
        if Read::by_ref(&mut input_stream)
            .take(2 * 1024 * 1024 + 1)
            .read_line(&mut line)?
            == 0
        {
            break;
        }
        if line.len() > 2 * 1024 * 1024 {
            return Err("risk_input_limit".into());
        }
        let input: serde_json::Value = serde_json::from_str(&line)?;
        if input["op"] == "http" {
            let env = input["proxy_env"]
                .as_object()
                .map(|v| {
                    v.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let spec = ansa::transport::frame_probe::Probe {
                url: input["url"].as_str().ok_or("url")?.into(),
                ca_der: input["ca_der"].as_str().map(std::fs::read).transpose()?,
                proxy_env: env,
                body_limit: input["body_limit"].as_u64().unwrap_or(16 * 1024 * 1024) as usize,
                h2_only: input["h2"].as_bool().unwrap_or(false),
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            println!(
                "{}",
                runtime.block_on(ansa::transport::frame_probe::fetch(spec))
            );
            continue;
        }
        if input["op"] == "domains" {
            let payload = Json::parse(input["payload"].as_str().ok_or("payload")?)?;
            println!(
                "{}",
                ansa::protocol::legacy_json_v1::domains(&payload)?.dump()
            );
            continue;
        }
        if input["op"] != "json" {
            return Err("risk_unknown_operation".into());
        }
        let result = match Json::parse(input["payload"].as_str().ok_or("payload")?) {
            Ok(value) => {
                let mut result =
                    serde_json::json!({"accepted":true,"dump":value.dump(),"sha256":hash(&value)});
                if let Some(other) = input["other"].as_str() {
                    match Json::parse(other) {
                        Ok(other) => result["equal"] = serde_json::json!(value == other),
                        Err(_) => result = serde_json::json!({"accepted":false}),
                    }
                }
                result
            }
            Err(_) => serde_json::json!({"accepted":false}),
        };
        println!("{result}");
    }
    Ok(())
}

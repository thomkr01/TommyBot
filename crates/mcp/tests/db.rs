//! The MCP role against a real Postgres. Skipped unless DATABASE_URL is set.
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use tower::ServiceExt;

const TOKEN: &str = "test-token-test-token-test-token-x";

async fn rpc(st: &tommybot_mcp::State, auth: &str, body: Value) -> (u16, Value) {
    let req = Request::post("/mcp")
        .header("content-type", "application/json")
        .header("authorization", auth)
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = tommybot_mcp::router(st.clone()).oneshot(req).await.unwrap();
    let code = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn tool(st: &tommybot_mcp::State, name: &str, args: Value) -> Value {
    let (code, r) = rpc(st, &format!("Bearer {TOKEN}"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}})).await;
    assert_eq!(code, 200);
    assert!(r["result"]["isError"].is_null(), "{name}: {r}");
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn read_only_role_and_tools() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = tommybot_core::db::connect(&url).await.unwrap();
    tommybot_core::db::set_mcp_password(&admin, "mcp-test-pw")
        .await
        .unwrap();
    let g = 8_000_000_000 + std::process::id() as i64;
    sqlx::query("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, reason) VALUES ($1, 1, 'warn', 42, 7, 'spam')")
        .bind(g).execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO notes (guild_id, user_id, author_id, text) VALUES ($1, 42, 7, 'watch this one')")
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO action_log (guild_id, actor_id, action, target_id) VALUES ($1, 7, 'ban', 42)",
    )
    .bind(g)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query("INSERT INTO mod_actions (guild_id, mod_id, kind, source, target_id) VALUES ($1, 7, 'kick', 'discord', 42)")
        .bind(g).execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 1, '{\"automod\": {\"on\": true}}')")
        .bind(g).execute(&admin).await.unwrap();

    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .username("tommybot_mcp")
        .password("mcp-test-pw");
    let db = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();

    // The role sees only the views and cannot write.
    for sql in [
        "SELECT 1 FROM cases LIMIT 1",
        "SELECT 1 FROM dash_session LIMIT 1",
        "SELECT 1 FROM dash_totp LIMIT 1",
        "SELECT content FROM reports LIMIT 1",
        "SELECT answers FROM appeals LIMIT 1",
        "SELECT 1 FROM mcp_writes LIMIT 1",
        "SELECT 1 FROM mcp_tokens LIMIT 1",
        "SELECT 1 FROM mod_actions LIMIT 1",
    ] {
        assert!(
            sqlx::query(sql).execute(&db).await.is_err(),
            "{sql} was allowed"
        );
    }
    let del = sqlx::query("SELECT purge_guild($1)")
        .bind(g)
        .execute(&db)
        .await;
    assert!(del.is_err(), "purge_guild was allowed");

    let st = tommybot_mcp::State {
        db,
        token: TOKEN.into(),
        log_dir: "/nonexistent".into(),
        oauth: None,
        writes: None,
    };
    let (code, _) = rpc(
        &st,
        "Bearer wrong",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(code, 401);
    let (_, init) = rpc(&st, &format!("Bearer {TOKEN}"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    let cases = tool(&st, "cases", json!({"guild_id": g.to_string()})).await;
    assert_eq!(cases[0]["reason"], "spam");
    assert_eq!(cases[0]["user_id"], "42");
    assert_eq!(
        tool(&st, "case", json!({"guild_id": g, "case_no": 1})).await["kind"],
        "warn"
    );
    assert_eq!(
        tool(
            &st,
            "guild_config",
            json!({"guild_id": g.to_string(), "section": "automod"})
        )
        .await["config"]["on"],
        true
    );
    assert_eq!(
        tool(&st, "config_history", json!({"guild_id": g.to_string()})).await[0]["version"],
        1
    );
    assert!(tool(&st, "guilds", json!({}))
        .await
        .as_array()
        .unwrap()
        .contains(&json!(g.to_string())));
    tool(&st, "status", json!({})).await;
    tool(&st, "raid_log", json!({"guild_id": g.to_string()})).await;
    tool(&st, "joingate_log", json!({"guild_id": g.to_string()})).await;
    for t in [
        "nuke_incidents",
        "quarantines",
        "strikes",
        "appeals",
        "reports",
        "backups",
        "ban_sync",
        "nsfw_scans",
    ] {
        tool(&st, t, json!({"guild_id": g.to_string()})).await;
    }
    tool(&st, "jobs", json!({})).await;
    sqlx::query("INSERT INTO raiders (user_id, status, category) VALUES ($1, 'pending', 'raid') ON CONFLICT DO NOTHING")
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("INSERT INTO raider_evidence (user_id, source_guild, source_owner, kind, ref, reporter_id) VALUES ($1, 1, 2, 'report', '1', 3)")
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    let raiders = tool(&st, "raiders", json!({"status": "pending"})).await;
    let r = raiders
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["user_id"].as_str() == Some(g.to_string().as_str()))
        .unwrap();
    assert_eq!(
        (r["evidence"].as_i64(), r["owners"].as_i64()),
        (Some(1), Some(1))
    );
    assert!(r.get("reporter_id").is_none() && r.get("detail").is_none());
    assert!(sqlx::query("SELECT 1 FROM raider_evidence LIMIT 1")
        .execute(&st.db)
        .await
        .is_err());
    sqlx::query("DELETE FROM raider_evidence WHERE user_id = $1")
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("DELETE FROM raiders WHERE user_id = $1")
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    let notes = tool(
        &st,
        "notes",
        json!({"guild_id": g.to_string(), "user_id": "42"}),
    )
    .await;
    assert_eq!(notes[0]["text"], "watch this one");
    let acts = tool(
        &st,
        "action_log",
        json!({"guild_id": g.to_string(), "actor_id": "7"}),
    )
    .await;
    assert_eq!(acts[0]["action"], "ban");
    assert_eq!(acts[0]["target_id"], "42");

    let stats = tool(
        &st,
        "mod_actions",
        json!({"guild_id": g.to_string(), "mod_id": "7"}),
    )
    .await;
    assert_eq!(
        (stats[0]["kind"].as_str(), stats[0]["source"].as_str()),
        (Some("kick"), Some("discord"))
    );
    assert_eq!(
        tool(
            &st,
            "mod_actions",
            json!({"guild_id": g.to_string(), "mod_id": "8"})
        )
        .await,
        json!([])
    );

    for t in [
        "cases",
        "guild_config",
        "notes",
        "action_log",
        "mod_actions",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {t} WHERE guild_id = $1"
        )))
        .bind(g)
        .execute(&admin)
        .await
        .unwrap();
    }
}

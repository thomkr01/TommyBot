use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

#[test]
fn id_time_reads_the_snowflake() {
    // Discord's documented example id.
    assert!(id_time(175_928_847_299_117_063)
        .unwrap()
        .starts_with("2016-04-30T11:18:25.796"));
}

/// Runs only when DATABASE_URL points at a scratch Postgres. Discord is a local mock.
#[tokio::test]
async fn review_report_and_appeal() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let db = tommybot_core::db::connect(&url).await.unwrap();
    let pid = std::process::id() as i64;
    let base = 9_610_000_000_000 + pid * 100;
    let (owner, server_owner, bot) = (base + 1, base + 2, base + 3);
    // big and old; too small; too new
    let (g1, g2, g3) = (base + 10, base + 11, base + 12);
    let users: Vec<i64> = (base + 50..base + 70).collect();
    let uid0 = users[0].to_string();
    let mock = Router::new()
        .route(
            "/applications/@me",
            get(move || async move { Json(json!({"owner": {"id": owner.to_string()}})) }),
        )
        .route(
            "/users/@me",
            get(move || async move { Json(json!({"id": bot.to_string()})) }),
        )
        .route(
            "/users/{u}",
            get(|Path(u): Path<String>| async move {
                Json(json!({"id": u, "username": format!("u{u}")}))
            }),
        )
        .route(
            "/guilds/{g}",
            get(move |Path(g): Path<i64>| async move {
                let members = if g == g2 { 50 } else { 150 };
                Json(json!({"id": g.to_string(), "name": format!("Server {g}"),
                    "owner_id": server_owner.to_string(), "approximate_member_count": members}))
            }),
        )
        .route(
            "/guilds/{g}/members/{u}",
            get(move |Path((g, _)): Path<(i64, String)>| async move {
                let days = if g == g3 { 1 } else { 30 };
                let at = chrono::Utc::now() - chrono::Duration::days(days);
                Json(json!({"roles": [], "joined_at": at.to_rfc3339()}))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, mock).await });
    let st = AppState {
        db: db.clone(),
        discord: crate::discord::Discord {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            client_id: String::new(),
            client_secret: String::new(),
            bot_token: String::new(),
            redirect_uri: String::new(),
            api,
            memo: Default::default(),
        },
        secure: false,
        totp: Default::default(),
        rate: Default::default(),
        ips: Default::default(),
        vpn_logged: Default::default(),
    };
    let send = |m: &str, path: String, user: i64, up: bool, body: Option<Value>| {
        let mut req = Request::builder()
            .method(m)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        req.extensions_mut().insert(Sess {
            id: String::new(),
            user_id: user,
            username: "t".into(),
            access_token: String::new(),
            stepped_up: up,
            csrf: String::new(),
        });
        let app = crate::router(st.clone(), "/nonexistent");
        async move {
            let res = app.oneshot(req).await.unwrap();
            let code = res.status().as_u16();
            let b = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                code,
                serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
            )
        }
    };
    let post = |path: String, user: i64, body: Value| send("POST", path, user, true, Some(body));
    let get_ = |path: &str, user: i64| send("GET", path.to_string(), user, false, None);
    let exec = |sql: &'static str, a: i64, b: i64| {
        let db = db.clone();
        async move {
            sqlx::query(sql).bind(a).bind(b).execute(&db).await.unwrap();
        }
    };
    let status = |u: i64| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (String, bool, Option<At>)>(
                "select status, verified, expires_at from raiders where user_id = $1",
            )
            .bind(u)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let jobs_for = |u: i64| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (i64, String)>(
                "select guild_id, kind from scheduled_jobs where payload->>'user' = $1 order by id",
            )
            .bind(u.to_string())
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    for (i, g) in [g1, g2, g3].into_iter().enumerate() {
        for (no, kind) in [(1, "ban"), (2, "warn"), (3, "kick")] {
            sqlx::query(
                "insert into cases (guild_id, case_no, kind, user_id, mod_id, reason)
                 values ($1, $2, $3, $4, 7, 'raid')",
            )
            .bind(g)
            .bind(no)
            .bind(kind)
            .bind(users[i])
            .execute(&db)
            .await
            .unwrap();
        }
    }

    // S13: every review route is the bot owner's, and every write needs 2FA.
    for p in ["queue", "listed", "appeals"] {
        let path = format!("/api/raiders/{p}");
        assert_eq!(get_(&path, server_owner).await.0, 403, "{p}");
        assert_eq!(get_(&path, owner).await.0, 200, "{p}");
    }
    for w in [
        format!("/api/raiders/{}/confirm", users[0]),
        format!("/api/raiders/{}/verify", users[0]),
        format!("/api/raiders/{}/reject", users[0]),
        format!("/api/raiders/{}/remove", users[0]),
        "/api/raiders/appeals/1/accept".to_string(),
        "/api/raiders/appeals/1/deny".to_string(),
    ] {
        let body = json!({"category": "raid"});
        assert_eq!(
            post(w.clone(), server_owner, body.clone()).await.0,
            403,
            "{w}"
        );
        let (code, r) = send("POST", w.clone(), owner, false, Some(body)).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (403, Some("step_up_required")),
            "{w}"
        );
    }

    // Report: owner + 2FA, a ban/kick/quarantine case, 100 members, 7 days, and never confirmed.
    let report = |g: i64, no: i32| format!("/api/guilds/{g}/cases/{no}/raider_report");
    let raid = json!({"category": "raid"});
    let (code, r) = send(
        "POST",
        report(g1, 1),
        server_owner,
        false,
        Some(raid.clone()),
    )
    .await;
    assert_eq!((code, r["error"].as_str()), (403, Some("step_up_required")));
    assert_eq!(
        post(report(g1, 1), owner, raid.clone()).await.0,
        403,
        "not an owner here"
    );
    assert_eq!(
        post(report(g1, 2), server_owner, raid.clone()).await.0,
        400,
        "a warn case"
    );
    assert_eq!(
        post(report(g2, 1), server_owner, raid.clone()).await.0,
        400,
        "50 members"
    );
    let (code, r) = post(report(g3, 1), server_owner, raid.clone()).await;
    assert_eq!(code, 400, "bot added yesterday: {r}");
    assert_eq!(
        post(report(g1, 1), server_owner, json!({"category": "x"}))
            .await
            .0,
        400
    );
    let long = json!({"category": "raid", "reason": "r".repeat(401)});
    assert_eq!(post(report(g1, 1), server_owner, long).await.0, 400);
    let why = json!({"category": "raid", "reason": "<b>raided</b> us"});
    let (code, r) = post(report(g1, 1), server_owner, why).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(status(users[0]).await.0, "pending");
    assert_eq!(
        post(report(g1, 1), server_owner, raid.clone()).await.0,
        400,
        "same case twice"
    );
    // S5: ten reports a day per owner, across all their servers.
    for u in &users[10..19] {
        exec(
            "insert into raider_evidence (user_id, source_guild, source_owner, kind, ref) values ($1, 1, $2, 'report', 'x')",
            *u,
            server_owner,
        )
        .await;
    }
    let (code, r) = post(report(g1, 3), server_owner, raid.clone()).await;
    assert_eq!(code, 400, "{r}");
    assert!(r["error"].as_str().unwrap().contains("10 reports"), "{r}");
    exec(
        "delete from raider_evidence where source_owner = $2 and ref = 'x' and user_id <> $1",
        0,
        server_owner,
    )
    .await;

    // The queue shows the entry with its evidence (server name and size, case) and distinct owners.
    let (_, q) = get_("/api/raiders/queue", owner).await;
    let e = q
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["user_id"] == uid0)
        .unwrap()
        .clone();
    let ev = &e["evidence"][0];
    assert_eq!(e["owners"].as_i64(), Some(1));
    assert_eq!(ev["server"].as_str(), Some(format!("Server {g1}").as_str()));
    assert_eq!(
        (ev["case_no"].as_i64(), ev["members"].as_i64()),
        (Some(1), Some(150))
    );
    assert_eq!(e["username"], format!("u{}", users[0]));
    // The reason is kept raw for the bot owner; the page shows it as text.
    assert_eq!(ev["note"].as_str(), Some("<b>raided</b> us"));

    // Confirm: status, review row and the bot's job together. Scam listings expire.
    let confirm = format!("/api/raiders/{}/confirm", users[0]);
    let long = json!({"category": "raid", "note": "x".repeat(201)});
    assert_eq!(post(confirm.clone(), owner, long).await.0, 400);
    exec(
        "update raider_evidence set created_at = now() - interval '30 days' where user_id = $1 and $2 = 0",
        users[0],
        0,
    )
    .await;
    let scam = json!({"category": "scam", "note": "Phishing links"});
    let (code, r) = post(confirm.clone(), owner, scam).await;
    assert_eq!(code, 200, "{r}");
    let (now_status, verified, expires) = status(users[0]).await;
    assert!(now_status == "confirmed" && verified && expires.is_some());
    // R5: 14 days from now, not from old evidence.
    let left = expires.unwrap() - chrono::Utc::now();
    assert!(left > chrono::Duration::days(13), "{left}");
    assert_eq!(jobs_for(users[0]).await, [(0, "raider_check".to_string())]);
    assert_eq!(
        post(confirm, owner, raid.clone()).await.0,
        409,
        "already confirmed"
    );
    // A report on a confirmed entry keeps it confirmed.
    exec(
        "delete from raider_evidence where user_id = $1 and source_guild = $2",
        users[0],
        g1,
    )
    .await;
    assert_eq!(post(report(g1, 1), server_owner, raid.clone()).await.0, 200);
    assert_eq!(status(users[0]).await.0, "confirmed");

    // Verify works only on an automatic listing.
    exec(
        "insert into raiders (user_id, status, category, verified) values ($1, 'confirmed', 'nuke', $2 = 1)",
        users[1],
        0,
    )
    .await;
    let verify = format!("/api/raiders/{}/verify", users[1]);
    assert_eq!(post(verify.clone(), owner, json!({})).await.0, 200);
    assert!(status(users[1]).await.1);
    assert_eq!(post(verify, owner, json!({})).await.0, 409);
    // Reject only from pending.
    exec(
        "insert into raiders (user_id, status, category) values ($1, 'pending', 'spam') on conflict do nothing",
        users[2],
        0,
    )
    .await;
    let reject = format!("/api/raiders/{}/reject", users[2]);
    assert_eq!(post(reject, owner, json!({})).await.0, 200);
    assert_eq!(status(users[2]).await.0, "rejected");
    assert!(jobs_for(users[2]).await.is_empty());

    // The listed user's page: only their own entry, read from the session.
    let (_, mine) = get_("/api/raider-appeal", users[0]).await;
    assert_eq!(mine["listed"], true);
    assert_eq!(
        (mine["category"].as_str(), mine["can_appeal"].as_bool()),
        (Some("scam"), Some(true))
    );
    assert!(mine.get("evidence").is_none() && !mine.to_string().contains("Server"));
    assert_eq!(
        get_("/api/raider-appeal", users[2]).await.1,
        json!({"listed": false})
    );
    // S12: a user_id in the body is ignored; the appeal is always the session user's.
    let appeal =
        |u: i64, body: Value| send("POST", "/api/raider-appeal".into(), u, false, Some(body));
    let spoof = json!({"text": "hi", "user_id": users[0].to_string()});
    assert_eq!(appeal(users[3], spoof).await.0, 400);
    assert_eq!(appeal(users[0], json!({"text": " "})).await.0, 400);
    assert_eq!(
        appeal(users[0], json!({"text": "y".repeat(1001)})).await.0,
        400
    );
    assert_eq!(
        appeal(users[0], json!({"text": "I was hacked"})).await.0,
        200
    );
    assert_eq!(
        appeal(users[0], json!({"text": "again"})).await.0,
        400,
        "one open appeal"
    );
    let open_appeal = || async {
        let (_, a) = get_("/api/raiders/appeals", owner).await;
        a.as_array()
            .unwrap()
            .iter()
            .find(|a| a["user_id"] == uid0)
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };
    // Deny: the next appeal opens after 30 days.
    let id = open_appeal().await;
    let deny = format!("/api/raiders/appeals/{id}/deny");
    assert_eq!(post(deny.clone(), owner, json!({})).await.0, 200);
    assert_eq!(post(deny, owner, json!({})).await.0, 409);
    assert_eq!(
        get_("/api/raider-appeal", users[0]).await.1["can_appeal"],
        false
    );
    assert_eq!(appeal(users[0], json!({"text": "please"})).await.0, 400);
    exec(
        "update raiders set appeal_after = now() - interval '1 minute' where user_id = $1 and $2 = 0",
        users[0],
        0,
    )
    .await;
    assert_eq!(appeal(users[0], json!({"text": "please"})).await.0, 200);
    // Accept: off the list, and the bot lifts what the list did.
    let id = open_appeal().await;
    let accept = format!("/api/raiders/appeals/{id}/accept");
    assert_eq!(post(accept, owner, json!({})).await.0, 200);
    assert_eq!(status(users[0]).await.0, "removed");
    // R4: the user hears each appeal result, and the lift goes with the acceptance.
    let kinds: Vec<String> = jobs_for(users[0]).await.into_iter().map(|j| j.1).collect();
    assert_eq!(
        kinds,
        [
            "raider_check",
            "raider_notify",
            "raider_lift",
            "raider_notify"
        ]
    );
    let told: Vec<String> = sqlx::query_scalar(
        "select payload->>'text' from scheduled_jobs where payload->>'user' = $1 and kind = 'raider_notify' order by id",
    )
    .bind(uid0.clone())
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(
        told[0].contains("denied") && told[1].contains("accepted"),
        "{told:?}"
    );
    // Remove queues a lift too.
    let remove = format!("/api/raiders/{}/remove", users[1]);
    assert_eq!(post(remove, owner, json!({})).await.0, 200);
    assert_eq!(jobs_for(users[1]).await, [(0, "raider_lift".to_string())]);
    let reviews: i64 =
        sqlx::query_scalar("select count(*) from raider_reviews where user_id = any($1)")
            .bind(&users)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(reviews, 6, "confirm, verify, reject, deny, accept, remove");
    // The bot owner's log: confirm, deny, accept and remove.
    let logged = jobs_for(owner).await;
    assert_eq!(logged.len(), 4, "{logged:?}");
    assert!(logged
        .iter()
        .all(|j| j == &(0, "raider_notify".to_string())));

    // Matches: this server's actions with only the category and the public note.
    exec(
        "insert into raider_actions (guild_id, user_id, action) values ($1, $2, 'ban')",
        g1,
        users[1],
    )
    .await;
    let (code, m) = get_(&format!("/api/guilds/{g1}/raider_matches"), server_owner).await;
    assert_eq!(code, 200, "{m}");
    assert_eq!(
        (m[0]["category"].as_str(), m[0]["action"].as_str()),
        (Some("nuke"), Some("ban"))
    );
    assert!(!m.to_string().contains("evidence") && !m.to_string().contains("reporter"));

    for sql in [
        "delete from raider_evidence where user_id = any($1)",
        "delete from raider_reviews where user_id = any($1)",
        "delete from raider_appeals where user_id = any($1)",
        "delete from raiders where user_id = any($1)",
        "delete from scheduled_jobs where payload->>'user' = any($1::bigint[]::text[])",
    ] {
        sqlx::query(sql).bind(&users).execute(&db).await.unwrap();
    }
    exec(
        "delete from scheduled_jobs where payload->>'user' = $1::text and $2 = 0",
        owner,
        0,
    )
    .await;
    for g in [g1, g2, g3] {
        exec("delete from cases where guild_id = $1 and $2 = 0", g, 0).await;
        exec(
            "delete from raider_actions where guild_id = $1 and $2 = 0",
            g,
            0,
        )
        .await;
    }
}

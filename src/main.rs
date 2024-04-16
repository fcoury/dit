use std::{collections::HashSet, env, time::Duration};

use frankenstein::{
    AsyncApi, AsyncTelegramApi, GetUpdatesParams, SendMessageParams, UpdateContent,
};
use roux::{response::BasicThing, submission::SubmissionData, Reddit};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    // initializations
    let client_id = env::var("REDDIT_CLIENT_ID").unwrap();
    let client_secret = env::var("REDDIT_CLIENT_SECRET").unwrap();
    let username = env::var("REDDIT_USERNAME").unwrap();
    let password = env::var("REDDIT_PASSWORD").unwrap();
    let keywords = env::var("KEYWORDS").unwrap();
    let keywords = keywords.split(",").collect::<Vec<&str>>();
    let subreddit_name = env::var("SUBREDDIT").unwrap();

    // database
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&env::var("DATABASE_URL").unwrap())
        .await?;

    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;
    println!("Monitoring for keywords: {:?}", keywords);
    println!("Monitoring subreddit {}", subreddit_name);

    let subreddit = Reddit::new("macos:dit:0.1.0 (by /u/fcoury)", &client_id, &client_secret)
        .username(&username)
        .password(&password)
        .subreddit(&subreddit_name)
        .await?;

    let api = AsyncApi::new(&env::var("TELEGRAM_TOKEN").unwrap());

    let mut offset = get(&pool, "offset", "0".to_string())
        .await?
        .parse::<i64>()?;
    let mut last_reddit_id: Option<Vec<u8>> = None;

    loop {
        match handle_requests(&pool, &api, offset).await {
            Ok(new_offset) => {
                offset = new_offset;
                println!("Setting new offset to {}", offset);
                set(&pool, "offset", &offset.to_string()).await?;
            }
            Err(e) => {
                eprintln!("Error handling requests: {:?}", e);
            }
        }

        match subreddit.latest(20, None).await {
            Ok(posts) => {
                let last_id = last_reddit_id.clone();
                let submissions = posts.data.children.iter().filter(|s| {
                    let id = base36::decode(&s.data.id).unwrap_or(vec![]);
                    let matches = if let Some(ref last_reddit_id) = last_id {
                        id.cmp(last_reddit_id) == std::cmp::Ordering::Greater
                    } else {
                        true
                    };
                    // matches && contains_any(&s.data.title, keywords.clone())
                    matches && sub_matches(s, keywords.clone())
                });

                last_reddit_id = posts
                    .data
                    .children
                    .iter()
                    .filter_map(|s| base36::decode(&s.data.id).ok())
                    .max();

                println!("Received {} submissions", posts.data.children.len());
                for s in submissions {
                    let submission = &s.data;
                    let subscribers = get_subscribers(&pool).await?;
                    println!(
                        "sending message to {} subscribers: {}",
                        subscribers.len(),
                        submission.title
                    );
                    for &chat_id in &subscribers {
                        let mut message = format!("{}", submission.title);
                        if let Some(ref url) = submission.url {
                            message = format!("{}\n{}", message, url);
                        }
                        let send_message_params = SendMessageParams::builder()
                            .chat_id(chat_id)
                            .text(message)
                            .build();
                        let _ = api.send_message(&send_message_params).await;
                    }
                }
            }
            Err(e) => {
                eprintln!("Error fetching posts: {:?}", e);
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn sub_matches(sub: &BasicThing<SubmissionData>, keywords: Vec<&str>) -> bool {
    let title = sub.data.title.to_lowercase();
    contains_any(&title, keywords.clone()) || contains_any(&sub.data.selftext, keywords.clone())
}

fn contains_any(s: &str, keywords: Vec<&str>) -> bool {
    keywords.iter().any(|keyword| {
        let s = &s.to_lowercase();
        s.contains(&keyword.to_lowercase())
    })
}

async fn get(pool: &sqlx::PgPool, key: &str, default: String) -> anyhow::Result<String> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    match row {
        Some(x) => Ok(x.0),
        None => Ok(default),
    }
}

async fn set(pool: &sqlx::PgPool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO settings (key, value) VALUES ($1, $2)
        ON CONFLICT (key) DO UPDATE 
        SET value = $2
        "#,
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

async fn get_subscribers(pool: &sqlx::PgPool) -> anyhow::Result<HashSet<i64>> {
    let subscribers = sqlx::query_as("SELECT chat_id FROM subscribers")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row: (i64,)| row.0)
        .collect::<HashSet<i64>>();
    Ok(subscribers)
}

async fn add_subscriber(pool: &sqlx::PgPool, chat_id: i64) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO subscribers (chat_id) VALUES ($1)")
        .bind(chat_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn remove_subscriber(pool: &sqlx::PgPool, chat_id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM subscribers WHERE chat_id = $1")
        .bind(chat_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn handle_requests(pool: &sqlx::PgPool, api: &AsyncApi, offset: i64) -> anyhow::Result<i64> {
    let get_updates_params = GetUpdatesParams::builder()
        .offset(offset)
        .timeout(10u32)
        .build();

    let mut new_offset = offset;
    if let Ok(response) = api.get_updates(&get_updates_params).await {
        for update in response.result {
            if let UpdateContent::Message(message) = update.content {
                if let Some(text) = message.text {
                    println!("Message: {:?}", text);
                    if text == "/subscribe" {
                        // subscribers.insert(message.chat.id);
                        let reply = SendMessageParams::builder()
                            .chat_id(message.chat.id)
                            .text("Subscribed to mechmarket".to_string())
                            .build();
                        add_subscriber(&pool, message.chat.id).await?;
                        let _ = api.send_message(&reply).await;
                    } else if text == "/unsubscribe" {
                        let reply = SendMessageParams::builder()
                            .chat_id(message.chat.id)
                            .text("Unsubscribed from mechmarket".to_string())
                            .build();
                        remove_subscriber(&pool, message.chat.id).await?;
                        let _ = api.send_message(&reply).await;
                    }
                }
            }
            new_offset = update.update_id as i64 + 1;
        }
    }

    Ok(new_offset)
}

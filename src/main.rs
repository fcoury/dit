use std::{collections::HashSet, time::Duration};

use frankenstein::{
    AsyncApi, AsyncTelegramApi, GetUpdatesParams, SendMessageParams, UpdateContent,
};
use futures::StreamExt as _;
use roux::Reddit;
use roux_stream::stream_submissions;
use shuttle_persist::PersistInstance;
use shuttle_secrets::SecretStore;
use tokio::select;
use tokio_retry::strategy::ExponentialBackoff;

#[shuttle_runtime::main]
async fn main(
    #[shuttle_persist::Persist] persist: PersistInstance,
    #[shuttle_secrets::Secrets] secret_store: SecretStore,
) -> Result<TelegramService, shuttle_runtime::Error> {
    Ok(TelegramService {
        persist,
        secret_store,
    })
}

struct TelegramService {
    persist: PersistInstance,
    secret_store: SecretStore,
}

#[shuttle_runtime::async_trait]
impl shuttle_runtime::Service for TelegramService {
    async fn bind(self, _addr: std::net::SocketAddr) -> Result<(), shuttle_runtime::Error> {
        execute(self.persist, self.secret_store).await?;
        Ok(())
    }
}

async fn execute(persist: PersistInstance, secret_store: SecretStore) -> anyhow::Result<()> {
    // initializations
    let client_id = secret_store.get("REDDIT_CLIENT_ID").unwrap();
    let client_secret = secret_store.get("REDDIT_CLIENT_SECRET").unwrap();
    let username = secret_store.get("REDDIT_USERNAME").unwrap();
    let password = secret_store.get("REDDIT_PASSWORD").unwrap();
    let keywords = secret_store.get("KEYWORDS").unwrap();
    let keywords = keywords.split(",").collect::<Vec<&str>>();
    let subreddit_name = secret_store.get("SUBREDDIT").unwrap();

    println!("Monitoring for keywords: {:?}", keywords);
    println!("Monitoring subreddit {}", subreddit_name);

    let subreddit = Reddit::new("macos:dit:0.1.0 (by /u/fcoury)", &client_id, &client_secret)
        .username(&username)
        .password(&password)
        .subreddit(&subreddit_name)
        .await?;

    let api = AsyncApi::new(&secret_store.get("TELEGRAM_TOKEN").unwrap());
    let mut offset = 0;
    let mut subscribers = persist
        .load::<HashSet<i64>>("subscribers")
        .unwrap_or_default();

    // stream for reddit posts
    let retry_strategy = ExponentialBackoff::from_millis(5).factor(100).take(3);
    let (mut reddit_stream, _reddit_join_handle) = stream_submissions(
        &subreddit,
        Duration::from_secs(30),
        retry_strategy,
        Some(Duration::from_secs(10)),
    );

    loop {
        select! {
            // Handle Telegram notifications
            new_offset = handle_requests(&persist, &api, offset, &mut subscribers) => {
                offset = new_offset.unwrap();
            },

            // Handle new Reddit posts
            post = reddit_stream.next() => {
                if let Some(Ok(submission)) = post {
                    if contains_any(&submission.title, keywords.clone()) {
                        println!("sending message: {}", submission.title);
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
            },
        }
    }
}

async fn handle_requests(
    persist: &PersistInstance,
    api: &AsyncApi,
    offset: i64,
    subscribers: &mut HashSet<i64>,
) -> anyhow::Result<i64> {
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
                        subscribers.insert(message.chat.id);
                        persist.save("subscribers", &subscribers).unwrap();
                        let _ = api.send_message(&reply).await;
                    } else if text == "/unsubscribe" {
                        let reply = SendMessageParams::builder()
                            .chat_id(message.chat.id)
                            .text("Unsubscribed from mechmarket".to_string())
                            .build();
                        subscribers.remove(&message.chat.id);
                        persist.save("subscribers", &subscribers).unwrap();
                        let _ = api.send_message(&reply).await;
                    }
                }
            }
            new_offset = update.update_id as i64 + 1;
        }
    }

    Ok(new_offset)
}

fn contains_any(s: &str, keywords: Vec<&str>) -> bool {
    keywords.iter().any(|keyword| {
        let s = &s.to_lowercase();
        s.contains(&keyword.to_lowercase())
    })
}

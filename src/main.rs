use futures::stream::StreamExt;
use regex::Regex;
use scraper::{Html, Selector};
use serenity::{
    async_trait,
    model::{
        channel::{Message, ReactionType},
        gateway::Ready,
        id::EmojiId,
    },
    prelude::*,
};
use std::{
    collections::{HashMap, HashSet},
    env,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Mutex};

struct Handler {
    id_scan_re: Regex,
    issue_title_css_selector: Selector,
    pr_title_css_selector: Selector,
    timeout_map: Arc<Mutex<HashMap<usize, Instant>>>,
}

impl Handler {
    async fn scan_message_for_github_references<'a>(
        &'a self,
        message: &'a str,
        timeout_map: &Mutex<HashMap<usize, Instant>>,
    ) -> impl futures::stream::Stream<Item = IdScanResult> + 'a {
        // spawn concurrent futures
        let (submit, output) = mpsc::unbounded_channel();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut timeout_map = timeout_map.lock().await;
        let futures: futures::stream::FuturesUnordered<_> = self
            .id_scan_re
            .captures_iter(message)
            .filter_map(|capture| {
                capture
                    .name("id")
                    .map(|id| id.as_str().parse::<usize>().ok())
                    .flatten()
            })
            .filter(|id| {
                // if the id is on a cooldown
                if let Some(time_added) = timeout_map.get(id) {
                    if time_added.elapsed() >= Duration::from_secs(30) {
                        timeout_map.remove(id);
                    }
                }
                if timeout_map.contains_key(id) || seen.contains(id) {
                    false
                } else {
                    seen.insert(*id);
                    timeout_map.insert(*id, Instant::now());
                    true
                }
            })
            .map(|id| {
                submit_issue_or_pr_from_id(
                    id,
                    submit.clone(),
                    &self.issue_title_css_selector,
                    &self.pr_title_css_selector,
                )
            })
            .collect();

        futures.collect::<Vec<_>>().await;

        output
    }
}

#[derive(Debug)]
enum IdScanResultType {
    Issue,
    PullRequest,
}

impl IdScanResultType {
    fn display(&self) -> &'static str {
        match self {
            IdScanResultType::Issue => "Issue",
            IdScanResultType::PullRequest => "Pull Request",
        }
    }

    fn github_selector(&self) -> &'static str {
        match self {
            IdScanResultType::Issue => "issues",
            IdScanResultType::PullRequest => "pull",
        }
    }
}

#[derive(Debug)]
struct IdScanResult {
    kind: IdScanResultType,
    id: usize,
    title: String,
    url: String,
}

async fn submit_issue_or_pr_from_id(
    id: usize,
    input: mpsc::UnboundedSender<IdScanResult>,
    issue_title_css_selector: &Selector,
    pr_title_css_selector: &Selector,
) {
    if let Some(result) =
        get_issue_or_pr_from_id(id, issue_title_css_selector, pr_title_css_selector).await
    {
        if let Err(why) = input.send(result) {
            eprintln!("Error sending async result message: {}", &why);
        }
    }
}

async fn get_scan_result(
    id: usize,
    selector: &Selector,
    kind: IdScanResultType,
) -> Option<IdScanResult> {
    let url = format!(
        "https://github.com/ziglang/zig/{}/{}",
        kind.github_selector(),
        id
    );
    let html = surf::get(&url).recv_string().await.ok()?;
    let document = Html::parse_document(&*html);
    document
        .select(selector)
        .next()
        .map(|element| element.text().next().map(|s| s.trim().to_string()))
        .flatten()
        .map(|title| IdScanResult {
            kind,
            title,
            url,
            id,
        })
}

async fn get_issue_or_pr_from_id(
    id: usize,
    issue_title_css_selector: &Selector,
    pr_title_css_selector: &Selector,
) -> Option<IdScanResult> {
    // more likely to be a issue than a pr
    match get_scan_result(id, issue_title_css_selector, IdScanResultType::Issue).await {
        Some(result) => Some(result),
        None => get_scan_result(id, pr_title_css_selector, IdScanResultType::PullRequest).await,
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.content.to_ascii_lowercase().contains("fast") {
            if let Err(why) = msg
                .react(
                    &ctx.http,
                    ReactionType::Custom {
                        animated: false,
                        id: EmojiId(712478923388354602),
                        name: Some(String::from("zigfast")),
                    },
                )
                .await
            {
                eprintln!("Failed to zigfast react: {}", &why);
            }
        }
        let mut stream = self
            .scan_message_for_github_references(&*msg.content, &self.timeout_map)
            .await;
        let mut buf = String::new();
        let mut result_counter: usize = 0;
        let before = std::time::Instant::now();
        while let Some(result) = stream.next().await {
            buf.push_str(&*format!(
                "{} **{}** [{}]({})\n",
                result.kind.display(),
                result.id,
                result.title,
                result.url
            ));
            result_counter += 1;
        }
        if !buf.is_empty() {
            println!("Parsed {} issues in {:?}", result_counter, before.elapsed());
            if let Err(why) = msg
                .channel_id
                .send_message(&ctx.http, |m| m.embed(|e| e.description(buf)))
                .await
            {
                eprintln!("Failed to send linked issues: {}", &why);
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        use serenity::model::gateway::Activity;
        use serenity::model::user::OnlineStatus;
        println!("{} is connected!", ready.user.name);

        let activity = Activity::playing("https://github.com/haze/zig-issue-linker");
        let status = OnlineStatus::Online;

        ctx.set_presence(Some(activity), status).await;
    }
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    // compile the issue/pr regex
    let id_scan_re =
        regex::Regex::new(r"#(?P<id>\d{2,})(\s+|$)").expect("Failed to compile issue regex");

    // compile title & pr css selector
    let issue_title_css_selector = Selector::parse(
        "#partial-discussion-header > div.gh-header-show > div > h1 > span.js-issue-title",
    )
    .expect("Failed to compile GitHub Title CSS");
    let pr_title_css_selector = Selector::parse(
        "#partial-discussion-header > div.gh-header-show > h1 > span.js-issue-title",
    )
    .expect("Failed to compile GitHub Title CSS");

    let mut client = Client::new(&token)
        .event_handler(Handler {
            id_scan_re,
            issue_title_css_selector,
            pr_title_css_selector,
            timeout_map: Arc::new(Mutex::new(HashMap::new())),
        })
        .await
        .expect("Err creating client");

    if let Err(why) = client.start().await {
        println!("Client error: {:?}", why);
    }
}

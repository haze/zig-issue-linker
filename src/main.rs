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
use tokio::sync::{mpsc, Mutex, RwLock as TokioRwLock};

struct Handler {
    id_scan_re: Regex,
    src_scan_re: Regex,
    issue_title_css_selector: Selector,
    pr_title_css_selector: Selector,
    timeout_map: Arc<Mutex<HashMap<usize, Instant>>>,
    std_searcher: StdSearcher,
}

struct StdSearcher {
    proc_child: Mutex<tokio::process::Child>,

    link_cache: TokioRwLock<HashMap<String, String>>,
}

struct SearchResult<'a> {
    pattern: &'a str,
    link: String,
}

impl<'a> std::fmt::Display for SearchResult<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}]({})", self.pattern, self.link)
    }
}

impl StdSearcher {
    async fn new() -> Result<StdSearcher, tokio::io::Error> {
        let mut anal_buddy_command =
            tokio::process::Command::new("analysis-buddy/zig-cache/bin/anal-buddy");
        let anal_buddy_command = anal_buddy_command
            .arg("zig/lib/")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let proc_child = Mutex::new(anal_buddy_command.spawn()?);

        Ok(StdSearcher {
            proc_child,
            link_cache: TokioRwLock::new(HashMap::new()),
        })
    }

    async fn lookup<'a>(&self, pattern: &'a str, submit: mpsc::UnboundedSender<SearchResult<'a>>) {
        let pattern = pattern.trim_matches('*');
        let cache_read_guard = self.link_cache.read().await;
        if let Some(link) = cache_read_guard.get(pattern) {
            if let Err(why) = submit.send(SearchResult {
                pattern,
                link: link.clone(),
            }) {
                eprintln!("Failed to submit found doc link: {}", &why);
            }
        } else {
            drop(cache_read_guard);
            if let Some(link) = self.search(pattern).await {
                if let Err(why) = submit.send(SearchResult {
                    pattern,
                    link: link.clone(),
                }) {
                    eprintln!("Failed to submit found doc link: {}", &why);
                } else {
                    let mut cache = self.link_cache.write().await;
                    cache.insert(pattern.to_string(), link);
                }
            } else {
                eprintln!("Search turned up empty for {:?}", pattern);
            }
        }
    }

    async fn search(&self, pattern: &str) -> Option<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut proc_child = self.proc_child.lock().await;
        if let Some(writer) = &mut proc_child.stdin {
            match writer.write(pattern.as_bytes()).await {
                Err(why) => {
                    eprintln!("Failed to send message to StdSearcher: {}", &why);
                    return None;
                }
                _ => {}
            }
            match writer.write(&[0xA]).await {
                Err(why) => {
                    eprintln!("Failed to send message to StdSearcher: {}", &why);
                    return None;
                }
                _ => {}
            }
            drop(writer);
            if let Some(reader) = &mut proc_child.stdout {
                let mut buf_reader = tokio::io::BufReader::new(reader);
                let mut buf = String::with_capacity(128);
                if let Err(why) = buf_reader.read_line(&mut buf).await {
                    eprintln!("Failed to read line from StdSearcher stdout: {}", &why);
                } else {
                    if buf.trim().is_empty() {
                        return None;
                    }
                    // pop captured newline
                    buf.pop();
                    return Some(buf);
                }
            } else {
                eprintln!("No stdout attached! :(");
            }
        } else {
            eprintln!("No stdin attached! :(");
        }
        None
    }
}

impl Handler {
    async fn scan_message_for_src_references<'a>(
        &'a self,
        message: &'a str,
    ) -> impl futures::stream::Stream<Item = SearchResult<'a>> + 'a {
        let (submit, output) = mpsc::unbounded_channel();
        let futures: futures::stream::FuturesUnordered<_> = self
            .src_scan_re
            .captures_iter(message)
            .filter_map(|m| m.get(0).map(|x| x.as_str()))
            .map(|m| self.std_searcher.lookup(m, submit.clone()))
            .collect();

        futures.collect::<Vec<_>>().await;

        output
    }

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
struct SourceReference<'a> {
    permalink: String,
    location: &'a str,
}

impl<'a> std::fmt::Display for SourceReference<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}]({})", self.location, self.permalink)
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

        let mut gh_src_stream = self.scan_message_for_src_references(&*msg.content).await;
        let mut gh_ref_stream = self
            .scan_message_for_github_references(&*msg.content, &self.timeout_map)
            .await;
        let mut buf = String::new();
        let mut ref_result_counter: usize = 0;
        let mut src_result_counter: usize = 0;
        let before = std::time::Instant::now();
        while let Some(result) = gh_ref_stream.next().await {
            buf.push_str(&*format!(
                "{} **{}** [{}]({})\n",
                result.kind.display(),
                result.id,
                result.title,
                result.url
            ));
            ref_result_counter += 1;
        }
        while let Some(result) = gh_src_stream.next().await {
            buf.push_str(&*format!("{}\n", result));
            src_result_counter += 1;
        }
        if !buf.is_empty() {
            println!(
                "Parsed {} issues & {} source locations in {:?}",
                ref_result_counter,
                src_result_counter,
                before.elapsed()
            );
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

async fn update_local_zig() -> Result<(), tokio::io::Error> {
    use tokio::process::Command;
    match tokio::fs::File::open("./zig").await.map_err(|e| e.kind()) {
        Err(tokio::io::ErrorKind::NotFound) => {
            let mut clone_command = Command::new("git");
            clone_command.args(&["clone", "https://github.com/ziglang/zig"]);
            let status: std::process::ExitStatus = clone_command.status().await?;
            if !status.success() {
                eprintln!("Git clone failed: {}", &status);
            }
            return Ok(());
        }
        _ => {}
    }
    let mut update_command = Command::new("git");
    update_command.current_dir("./zig");
    update_command.args(&["reset", "--hard", "HEAD"]);
    let status: std::process::ExitStatus = update_command.status().await?;
    if !status.success() {
        eprintln!("Git update failed: {}", &status);
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    if let Err(why) = update_local_zig().await {
        eprintln!("{}", &why);
    }

    // compile the issue/pr regex
    let id_scan_re = regex::Regex::new(r"#(?P<id>\d{2,})(\s+|[.!,`?;:]|$)")
        .expect("Failed to compile issue regex");

    // compile the code scan regex
    let src_scan_re =
        regex::Regex::new(r"\*(std)(\.(\w+)+)+\*").expect("Failed to compile code scan regex");

    // compile title & pr css selector
    let issue_title_css_selector = Selector::parse(
        "#partial-discussion-header > div.gh-header-show > div > h1 > span.js-issue-title",
    )
    .expect("Failed to compile GitHub Title CSS");
    let pr_title_css_selector = Selector::parse(
        "#partial-discussion-header > div.gh-header-show > h1 > span.js-issue-title",
    )
    .expect("Failed to compile GitHub Title CSS");

    match StdSearcher::new().await {
        Ok(std_searcher) => {
            let mut client = Client::new(&token)
                .event_handler(Handler {
                    id_scan_re,
                    src_scan_re,
                    issue_title_css_selector,
                    pr_title_css_selector,
                    timeout_map: Arc::new(Mutex::new(HashMap::new())),
                    std_searcher,
                })
                .await
                .expect("Err creating client");

            if let Err(why) = client.start().await {
                println!("Client error: {:?}", why);
            }
        }
        Err(why) => {
            eprintln!("Failed to initialize std searcher server: {}", &why);
        }
    }
}
